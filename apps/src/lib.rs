//! Installed applications and which one opens what: an index of the
//! `.desktop` files, the freedesktop mime-apps lists (defaults, added and
//! removed associations), `Exec` expansion and launching. The index is
//! built on a thread at startup.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub mod mime;

/// The desktop whose `<desktop>-mimeapps.list` is read first.
const DESKTOP: &str = "cnde";

/// One installed application.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// The desktop file ID: `org.gnome.TextEditor.desktop`.
    pub id: String,
    pub name: String,
    pub exec: String,
    pub icon: Option<String>,
    pub mimes: Vec<String>,
    /// The `Categories`, e.g. `TerminalEmulator`.
    pub categories: Vec<String>,
    pub terminal: bool,
    pub no_display: bool,
    pub path: PathBuf,
}

/// Parse one desktop file. `None` for what isn't a usable application:
/// another `Type`, `Hidden`, no `Name` or `Exec`.
pub fn parse_desktop(text: &str, id: &str, path: &Path) -> Option<App> {
    let mut in_entry = false;
    let mut kv: HashMap<&str, &str> = HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry
            && !line.starts_with('#')
            && let Some((k, v)) = line.split_once('=')
        {
            // The first of a key wins; localized keys are other keys.
            kv.entry(k.trim()).or_insert(v.trim());
        }
    }
    let flag = |k: &str| kv.get(k) == Some(&"true");
    if kv.get("Type") != Some(&"Application") || flag("Hidden") {
        return None;
    }
    let (name, exec) = (kv.get("Name")?, kv.get("Exec")?);
    if name.is_empty() || exec.is_empty() {
        return None;
    }
    if kv.get("TryExec").is_some_and(|t| !t.is_empty() && !in_path(t)) {
        return None;
    }
    Some(App {
        id: id.to_string(),
        name: name.to_string(),
        exec: exec.to_string(),
        icon: kv
            .get("Icon")
            .filter(|i| !i.is_empty())
            .map(|i| i.to_string()),
        mimes: kv
            .get("MimeType")
            .map(|m| {
                m.split(';')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        categories: kv
            .get("Categories")
            .map(|m| {
                m.split(';')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        terminal: flag("Terminal"),
        no_display: flag("NoDisplay"),
        path: path.to_path_buf(),
    })
}

fn in_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).exists();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).exists()))
}

// ------------------------------------------------------------ the dirs

/// The base dirs the lists and desktop files live in.
#[derive(Debug, Clone)]
pub struct Env {
    pub config_home: PathBuf,
    pub config_dirs: Vec<PathBuf>,
    pub data_home: PathBuf,
    pub data_dirs: Vec<PathBuf>,
}

impl Env {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let var = |name: &str, default: PathBuf| match std::env::var_os(name) {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => default,
        };
        let list = |name: &str, default: &str| -> Vec<PathBuf> {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.into())
                .split(':')
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .collect()
        };
        Env {
            config_home: var("XDG_CONFIG_HOME", home.join(".config")),
            config_dirs: list("XDG_CONFIG_DIRS", "/etc/xdg"),
            data_home: var("XDG_DATA_HOME", home.join(".local/share")),
            data_dirs: list("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
        }
    }

    /// Where desktop files are, most important first.
    fn app_dirs(&self) -> Vec<PathBuf> {
        std::iter::once(&self.data_home)
            .chain(&self.data_dirs)
            .map(|d| d.join("applications"))
            .collect()
    }

    /// The mimeapps lists, most important first.
    pub fn list_files(&self) -> Vec<PathBuf> {
        let dirs = std::iter::once(self.config_home.clone())
            .chain(self.config_dirs.iter().cloned())
            .chain(self.app_dirs());
        let mut files = Vec::new();
        for dir in dirs {
            files.push(dir.join(format!("{DESKTOP}-mimeapps.list")));
            files.push(dir.join("mimeapps.list"));
        }
        files
    }
}

// ------------------------------------------------------------ mimeapps.list

/// One `mimeapps.list`.
#[derive(Debug, Default, PartialEq)]
pub struct MimeApps {
    pub default: HashMap<String, Vec<String>>,
    pub added: HashMap<String, Vec<String>>,
    pub removed: HashMap<String, Vec<String>>,
}

pub fn parse_mimeapps(text: &str) -> MimeApps {
    let mut out = MimeApps::default();
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
            continue;
        }
        let Some((mime, ids)) = line.split_once('=') else {
            continue;
        };
        let map = match section {
            "Default Applications" => &mut out.default,
            "Added Associations" => &mut out.added,
            "Removed Associations" => &mut out.removed,
            _ => continue,
        };
        let ids = ids
            .split(';')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        // The first line for a type wins within a file.
        map.entry(mime.trim().to_string()).or_insert(ids);
    }
    out
}

/// Every list that exists, most important first.
pub fn load_lists(env: &Env) -> Vec<MimeApps> {
    env.list_files()
        .iter()
        .filter_map(|f| fs::read_to_string(f).ok())
        .map(|t| parse_mimeapps(&t))
        .collect()
}

/// `text` with `id` as the first default for `mime`, the rest untouched.
pub fn with_default(text: &str, mime: &str, id: &str) -> String {
    let key = format!("{mime}=");
    let value = |old: &str| {
        let rest = old
            .split(';')
            .filter(|s| !s.is_empty() && *s != id)
            .collect::<Vec<_>>()
            .join(";");
        if rest.is_empty() {
            format!("{key}{id};")
        } else {
            format!("{key}{id};{rest};")
        }
    };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut section_at = None;
    let mut in_section = false;
    for i in 0..lines.len() {
        let t = lines[i].trim();
        if t.starts_with('[') {
            in_section = t == "[Default Applications]";
            if in_section && section_at.is_none() {
                section_at = Some(i);
            }
        } else if in_section && let Some(old) = t.strip_prefix(&key) {
            lines[i] = value(old);
            return lines.join("\n") + "\n";
        }
    }
    match section_at {
        Some(i) => lines.insert(i + 1, value("")),
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[Default Applications]".into());
            lines.push(value(""));
        }
    }
    lines.join("\n") + "\n"
}

/// Make `id` the default for `mime` in `$XDG_CONFIG_HOME/mimeapps.list`,
/// keeping everything else in the file. Written to a temp file and renamed
/// (through a symlink, if the list is one).
pub fn set_default(env: &Env, mime: &str, id: &str) -> io::Result<()> {
    set_defaults(env, &[mime], id)
}

/// [`set_default`] for several types, in one write.
pub fn set_defaults(env: &Env, mimes: &[&str], id: &str) -> io::Result<()> {
    let path = env.config_home.join("mimeapps.list");
    let target = fs::canonicalize(&path).unwrap_or(path);
    let old = fs::read_to_string(&target).unwrap_or_default();
    let new = mimes
        .iter()
        .fold(old, |text, mime| with_default(&text, mime, id));
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = target.with_extension(format!("list.tmp{}", std::process::id()));
    fs::write(&tmp, new)?;
    fs::rename(&tmp, &target).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

// ------------------------------------------------------------ the index

pub struct Index {
    /// By name.
    pub apps: Vec<App>,
    pub env: Env,
}

static INDEX: OnceLock<Index> = OnceLock::new();

/// Build the index on a thread.
pub fn start() {
    std::thread::spawn(|| {
        let _ = INDEX.set(Index::build(Env::from_env()));
    });
}

/// Put an index in place of the real one (the preview's).
pub fn install(ix: Index) {
    let _ = INDEX.set(ix);
}

/// The index, once it is built.
pub fn index() -> Option<&'static Index> {
    INDEX.get()
}

fn scan(dir: &Path, prefix: &str, seen: &mut HashSet<String>, out: &mut Vec<App>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if path.is_dir() {
            scan(&path, &format!("{prefix}{name}-"), seen, out);
        } else if name.ends_with(".desktop") {
            let id = format!("{prefix}{name}");
            // A hidden file in a nearer dir still hides the farther one.
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(app) = fs::read_to_string(&path)
                .ok()
                .and_then(|t| parse_desktop(&t, &id, &path))
            {
                out.push(app);
            }
        }
    }
}

impl Index {
    pub fn build(env: Env) -> Index {
        let mut apps = Vec::new();
        let mut seen = HashSet::new();
        for dir in env.app_dirs() {
            scan(&dir, "", &mut seen, &mut apps);
        }
        apps.sort_by_key(|a| a.name.to_lowercase());
        Index { apps, env }
    }

    /// The apps for a file of type `mime`, the default first; indexes into
    /// [`Index::apps`].
    pub fn for_mime(&self, mime: &str) -> Vec<usize> {
        resolve(&self.apps, &load_lists(&self.env), &mime::chain(mime))
    }

    /// The app that opens `mime`, if any.
    pub fn default_for(&self, mime: &str) -> Option<usize> {
        self.for_mime(mime).first().copied()
    }

    /// Everything that can be picked from "Other Application…".
    pub fn all(&self) -> Vec<usize> {
        (0..self.apps.len())
            .filter(|&i| !self.apps[i].no_display)
            .collect()
    }
}

/// Apps for the types in `chain` (nearest first), default first. `lists`
/// are most important first.
pub fn resolve(apps: &[App], lists: &[MimeApps], chain: &[String]) -> Vec<usize> {
    let by_id = |id: &str| apps.iter().position(|a| a.id == id);
    let mut out: Vec<usize> = Vec::new();
    // The first default that is installed.
    'find: for m in chain {
        for l in lists {
            for id in l.default.get(m).into_iter().flatten() {
                if let Some(i) = by_id(id) {
                    out.push(i);
                    break 'find;
                }
            }
        }
    }
    for m in chain {
        let mut set: Vec<usize> = (0..apps.len())
            .filter(|&i| !apps[i].no_display && apps[i].mimes.contains(m))
            .collect();
        // From the least important list up: later lists override.
        for l in lists.iter().rev() {
            for id in l.removed.get(m).into_iter().flatten() {
                set.retain(|&i| apps[i].id != *id);
            }
            for id in l.added.get(m).into_iter().flatten() {
                if let Some(i) = by_id(id)
                    && !set.contains(&i)
                {
                    set.push(i);
                }
            }
        }
        for i in set {
            if !out.contains(&i) {
                out.push(i);
            }
        }
    }
    out
}

// ------------------------------------------------------------ Exec

/// Split an `Exec` value into words: spaces separate, double quotes group,
/// and inside them a backslash escapes `"`, `` ` ``, `$` and `\`.
pub fn split_exec(exec: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut any) = (false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\\' if quoted => match chars.next() {
                Some(n) if matches!(n, '"' | '`' | '$' | '\\') => cur.push(n),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => cur.push('\\'),
            },
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// The `file://` URI of a path, escaped the way GLib does (what Nautilus
/// hashes): letters, digits and `-_.~!$&'()*+,;=:@/` stay as they are.
pub fn uri_for(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:@/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One word with its field codes replaced, `files` being what to open.
fn expand_word(word: &str, files: &[&Path], app: &App) -> Vec<String> {
    let path = |p: &Path| p.display().to_string();
    let uri = |p: &Path| uri_for(p);
    match word {
        "%F" => return files.iter().map(|p| path(p)).collect(),
        "%U" => return files.iter().map(|p| uri(p)).collect(),
        "%i" => {
            return app
                .icon
                .iter()
                .flat_map(|i| ["--icon".to_string(), i.clone()])
                .collect();
        }
        _ => {}
    }
    let mut out = String::new();
    let mut stripped = false;
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('f' | 'F') => out.push_str(&files.first().map(|p| path(p)).unwrap_or_default()),
            Some('u' | 'U') => out.push_str(&files.first().map(|p| uri(p)).unwrap_or_default()),
            Some('i') => out.push_str(app.icon.as_deref().unwrap_or("")),
            Some('c') => out.push_str(&app.name),
            Some('k') => out.push_str(&app.path.display().to_string()),
            // Deprecated codes (%d %D %n %N %v %m) and strays are dropped.
            Some(_) | None => stripped = true,
        }
    }
    if out.is_empty() && stripped {
        Vec::new()
    } else {
        vec![out]
    }
}

/// The commands to run to open `files` with `app`: one with every file if
/// the `Exec` takes a list (`%F`, `%U`), else one per file.
pub fn commands(app: &App, files: &[PathBuf]) -> Vec<Vec<String>> {
    let words = split_exec(&app.exec);
    let takes_list = words.iter().any(|w| w == "%F" || w == "%U");
    let takes_one = !takes_list
        && words.iter().any(|w| {
            let mut it = w.chars();
            let mut found = false;
            while let Some(c) = it.next() {
                if c == '%' {
                    found |= matches!(it.next(), Some('f' | 'F' | 'u' | 'U'));
                }
            }
            found
        });
    let groups: Vec<Vec<&Path>> = if takes_one {
        files.iter().map(|f| vec![f.as_path()]).collect()
    } else {
        vec![files.iter().map(PathBuf::as_path).collect()]
    };
    groups
        .iter()
        .map(|g| {
            words
                .iter()
                .flat_map(|w| expand_word(w, g, app))
                .collect::<Vec<_>>()
        })
        .filter(|argv| !argv.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, exec: &str, mimes: &[&str]) -> App {
        App {
            id: id.into(),
            name: name.into(),
            exec: exec.into(),
            icon: Some("ico".into()),
            categories: vec![],
            mimes: mimes.iter().map(|m| m.to_string()).collect(),
            terminal: false,
            no_display: false,
            path: PathBuf::from(format!("/usr/share/applications/{id}")),
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cnde-apps-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn desktop_files() {
        let text = "# c\n[Desktop Entry]\nType=Application\nName=Edit\nName[de]=Bearbeiten\nExec=edit %F\nIcon=edit\nMimeType=text/plain;text/x-c;\nTerminal=true\n[Desktop Action x]\nName=Other\nExec=nope\n";
        let a = parse_desktop(text, "edit.desktop", Path::new("/e.desktop")).unwrap();
        assert_eq!(a.name, "Edit");
        assert_eq!(a.exec, "edit %F");
        assert_eq!(a.icon.as_deref(), Some("edit"));
        assert_eq!(a.mimes, ["text/plain", "text/x-c"]);
        assert!(a.terminal && !a.no_display);
        let p = |t: &str| parse_desktop(t, "x.desktop", Path::new("/x"));
        assert!(p("[Desktop Entry]\nType=Link\nName=a\nExec=a\n").is_none());
        assert!(p("[Desktop Entry]\nType=Application\nName=a\nExec=a\nHidden=true\n").is_none());
        assert!(
            p("[Desktop Entry]\nType=Application\nName=a\nExec=a\nTryExec=/no/such/prog\n")
                .is_none()
        );
        assert!(
            p("[Desktop Entry]\nType=Application\nName=a\nExec=a\nNoDisplay=true\n")
                .unwrap()
                .no_display
        );
    }

    #[test]
    fn exec_words_and_quotes() {
        assert_eq!(split_exec("a  b\tc"), ["a", "b", "c"]);
        assert_eq!(
            split_exec(r#"env "A B=\"q\" \$x" run"#),
            ["env", r#"A B="q" $x"#, "run"]
        );
        assert_eq!(split_exec(r#"x "" y"#), ["x", "", "y"]);
    }

    #[test]
    fn field_codes() {
        let a = app("e.desktop", "Ed", "ed", &[]);
        let files = vec![PathBuf::from("/a b/x.txt"), PathBuf::from("/c/y.txt")];
        let run = |exec: &str| {
            commands(
                &App {
                    exec: exec.into(),
                    ..a.clone()
                },
                &files,
            )
        };
        // A list code takes all the files at once.
        assert_eq!(run("ed %F"), [["ed", "/a b/x.txt", "/c/y.txt"]]);
        assert_eq!(
            run("ed %U"),
            [["ed", "file:///a%20b/x.txt", "file:///c/y.txt"]]
        );
        // A single code runs once per file.
        assert_eq!(
            run("ed --f=%f"),
            [["ed", "--f=/a b/x.txt"], ["ed", "--f=/c/y.txt"]]
        );
        // Deprecated codes go; %% stays; %i, %c and %k fill in.
        assert_eq!(
            run("ed %d %n 100%% %i %c %k"),
            [[
                "ed",
                "100%",
                "--icon",
                "ico",
                "Ed",
                "/usr/share/applications/e.desktop"
            ]]
        );
        // No file code: no files.
        assert_eq!(run("ed"), [["ed"]]);
        // A quoted program with spaces stays one word.
        assert_eq!(run(r#""/opt/My App/run" %f"#)[0][0], "/opt/My App/run");
    }

    #[test]
    fn mimeapps_parse() {
        let l = parse_mimeapps(
            "# x\n[Default Applications]\ntext/plain=a.desktop;b.desktop;\n[Added Associations]\nimage/png=c.desktop;\n[Removed Associations]\nimage/png=d.desktop;\n[Other]\nx=y\n",
        );
        assert_eq!(l.default["text/plain"], ["a.desktop", "b.desktop"]);
        assert_eq!(l.added["image/png"], ["c.desktop"]);
        assert_eq!(l.removed["image/png"], ["d.desktop"]);
    }

    #[test]
    fn precedence() {
        let apps = vec![
            app("a.desktop", "A", "a", &["text/plain"]),
            app("b.desktop", "B", "b", &["text/plain"]),
            app("c.desktop", "C", "c", &["image/png"]),
        ];
        let chain = |m: &str| vec![m.to_string()];
        // No lists: the apps that declare the type.
        assert_eq!(resolve(&apps, &[], &chain("text/plain")), [0, 1]);
        // A default goes first, and the nearer list wins.
        let near = parse_mimeapps("[Default Applications]\ntext/plain=b.desktop;\n");
        let far = parse_mimeapps("[Default Applications]\ntext/plain=a.desktop;\n");
        assert_eq!(resolve(&apps, &[near, far], &chain("text/plain")), [1, 0]);
        // A default that isn't installed is skipped.
        let gone = parse_mimeapps("[Default Applications]\ntext/plain=zz.desktop;b.desktop;\n");
        assert_eq!(resolve(&apps, &[gone], &chain("text/plain"))[0], 1);
        // Removed drops it; a nearer Added brings it back; Added adds.
        let far = parse_mimeapps("[Removed Associations]\ntext/plain=a.desktop;\n");
        assert_eq!(resolve(&apps, &[far], &chain("text/plain")), [1]);
        let near = parse_mimeapps("[Added Associations]\ntext/plain=a.desktop;c.desktop;\n");
        let far = parse_mimeapps("[Removed Associations]\ntext/plain=a.desktop;\n");
        assert_eq!(
            resolve(&apps, &[near, far], &chain("text/plain")),
            [1, 0, 2]
        );
        // A kind of text is opened by the text app.
        let kinds = ["text/x-foo", "text/plain"].map(String::from);
        assert_eq!(resolve(&apps, &[], &kinds), [0, 1]);
    }

    #[test]
    fn lists_are_read_in_order() {
        let root = temp("lists");
        let (cfg, data) = (root.join("cfg"), root.join("data"));
        fs::create_dir_all(data.join("applications")).unwrap();
        fs::create_dir_all(&cfg).unwrap();
        fs::write(
            data.join("applications/mimeapps.list"),
            "[Default Applications]\ntext/plain=far.desktop;\n",
        )
        .unwrap();
        fs::write(
            cfg.join("mimeapps.list"),
            "[Default Applications]\ntext/plain=mid.desktop;\n",
        )
        .unwrap();
        fs::write(
            cfg.join("cnde-mimeapps.list"),
            "[Default Applications]\ntext/plain=top.desktop;\n",
        )
        .unwrap();
        let env = Env {
            config_home: cfg,
            config_dirs: vec![],
            data_home: data,
            data_dirs: vec![],
        };
        let lists = load_lists(&env);
        let first: Vec<_> = lists
            .iter()
            .map(|l| l.default["text/plain"][0].as_str())
            .collect();
        assert_eq!(first, ["top.desktop", "mid.desktop", "far.desktop"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn writing_a_default_keeps_the_rest() {
        let old = "# mine\n[Added Associations]\nimage/png=x.desktop;\n\n[Default Applications]\ntext/plain=a.desktop;b.desktop;\nimage/png=old.desktop;\n\n[Removed Associations]\nfoo/bar=z.desktop;\n";
        let new = with_default(old, "text/plain", "b.desktop");
        assert_eq!(
            new,
            "# mine\n[Added Associations]\nimage/png=x.desktop;\n\n[Default Applications]\ntext/plain=b.desktop;a.desktop;\nimage/png=old.desktop;\n\n[Removed Associations]\nfoo/bar=z.desktop;\n"
        );
        // A new type is added to the section.
        let new = with_default(old, "video/mp4", "v.desktop");
        let l = parse_mimeapps(&new);
        assert_eq!(l.default["video/mp4"], ["v.desktop"]);
        assert_eq!(l.default["image/png"], ["old.desktop"]);
        assert_eq!(l.removed["foo/bar"], ["z.desktop"]);
        assert_eq!(l.added["image/png"], ["x.desktop"]);
        // No section yet: it is appended.
        assert_eq!(
            with_default("[Added Associations]\na/b=c.desktop;\n", "x/y", "z.desktop"),
            "[Added Associations]\na/b=c.desktop;\n\n[Default Applications]\nx/y=z.desktop;\n"
        );
        assert_eq!(
            with_default("", "x/y", "z.desktop"),
            "[Default Applications]\nx/y=z.desktop;\n"
        );
    }

    #[test]
    fn set_default_writes_atomically_in_a_temp_dir() {
        let root = temp("write");
        let env = Env {
            config_home: root.join("cfg"),
            config_dirs: vec![],
            data_home: root.join("data"),
            data_dirs: vec![],
        };
        fs::create_dir_all(&env.config_home).unwrap();
        fs::write(
            env.config_home.join("mimeapps.list"),
            "[Added Associations]\na/b=c.desktop;\n",
        )
        .unwrap();
        set_default(&env, "text/plain", "ed.desktop").unwrap();
        let l = parse_mimeapps(&fs::read_to_string(env.config_home.join("mimeapps.list")).unwrap());
        assert_eq!(l.default["text/plain"], ["ed.desktop"]);
        assert_eq!(l.added["a/b"], ["c.desktop"]);
        // No temp files left behind.
        assert_eq!(fs::read_dir(&env.config_home).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn set_defaults_writes_several_types_at_once() {
        let root = temp("many");
        let env = Env {
            config_home: root.clone(),
            config_dirs: vec![],
            data_home: root.join("d"),
            data_dirs: vec![],
        };
        fs::write(
            root.join("mimeapps.list"),
            "[Added Associations]\nx/y=a.desktop;\n\n[Default Applications]\nimage/png=old.desktop;\n",
        )
        .unwrap();
        set_defaults(&env, &["image/png", "image/gif"], "new.desktop").unwrap();
        let text = fs::read_to_string(root.join("mimeapps.list")).unwrap();
        let lists = parse_mimeapps(&text);
        assert_eq!(lists.default["image/png"], ["new.desktop", "old.desktop"]);
        assert_eq!(lists.default["image/gif"], ["new.desktop"]);
        assert_eq!(lists.added["x/y"], ["a.desktop"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn index_reads_desktop_files_and_hides() {
        let root = temp("index");
        let (home, sys) = (root.join("home"), root.join("sys"));
        for d in [&home, &sys] {
            fs::create_dir_all(d.join("applications/sub")).unwrap();
        }
        let entry = |name: &str, extra: &str| {
            format!("[Desktop Entry]\nType=Application\nName={name}\nExec={name}\n{extra}")
        };
        fs::write(
            sys.join("applications/a.desktop"),
            entry("Zed", "MimeType=text/plain;\n"),
        )
        .unwrap();
        fs::write(sys.join("applications/b.desktop"), entry("Bee", "")).unwrap();
        fs::write(sys.join("applications/sub/c.desktop"), entry("Cee", "")).unwrap();
        // The nearer file overrides, and Hidden removes.
        fs::write(home.join("applications/a.desktop"), entry("Alpha", "")).unwrap();
        fs::write(
            home.join("applications/b.desktop"),
            entry("Bee", "Hidden=true\n"),
        )
        .unwrap();
        let ix = Index::build(Env {
            config_home: root.join("cfg"),
            config_dirs: vec![],
            data_home: home,
            data_dirs: vec![sys],
        });
        let ids: Vec<_> = ix.apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["a.desktop", "sub-c.desktop"]);
        assert_eq!(ix.apps[0].name, "Alpha");
        let _ = fs::remove_dir_all(&root);
    }
}
