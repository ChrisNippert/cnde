//! A file chooser request: what the portal backend (`cnde-files-portal`)
//! asks of the picker window (`cnde-files --pick ...`). Both binaries
//! include this module. The options of the D-Bus call become [`Request`],
//! which travels to the picker as command line arguments; the picker
//! answers on stdout (see [`Answer`]) and by its exit status.

// Each binary uses a different half.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use zbus::zvariant::OwnedValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Open,
    Save,
    /// Pick a folder; for SaveFiles and for Open with `directory`.
    Folder,
}

/// One pattern of a filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// A glob on the file name (`*.png`).
    Glob(String),
    /// A mime type, maybe with a wildcard subtype (`image/*`).
    Mime(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub label: String,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub kind: Kind,
    pub title: String,
    /// The accept button's text; empty for the default.
    pub accept: String,
    pub multiple: bool,
    pub filters: Vec<Filter>,
    /// Index into `filters`.
    pub current_filter: usize,
    /// Save: the suggested name.
    pub name: String,
    /// Where to start.
    pub folder: Option<PathBuf>,
}

/// A filter as the portal sends it: `(sa(us))`.
type RawFilter = (String, Vec<(u32, String)>);

impl Filter {
    fn from_raw((label, rules): RawFilter) -> Self {
        Self {
            label,
            rules: rules
                .into_iter()
                .filter_map(|(kind, pattern)| match kind {
                    0 => Some(Rule::Glob(pattern)),
                    1 => Some(Rule::Mime(pattern)),
                    _ => None,
                })
                .collect(),
        }
    }

    pub fn to_raw(&self) -> RawFilter {
        (
            self.label.clone(),
            self.rules
                .iter()
                .map(|r| match r {
                    Rule::Glob(p) => (0, p.clone()),
                    Rule::Mime(p) => (1, p.clone()),
                })
                .collect(),
        )
    }

    /// Does the file called `name` pass? A filter without rules passes all.
    pub fn matches(&self, name: &str) -> bool {
        self.rules.is_empty()
            || self.rules.iter().any(|rule| match rule {
                Rule::Glob(pattern) => glob(pattern, name),
                Rule::Mime(pattern) => mime_matches(pattern, name),
            })
    }
}

/// `*` and `?` wildcards, ignoring case.
pub fn glob(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let n: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
    let (mut pi, mut ni) = (0, 0);
    // Where the last `*` was, and the name position it currently covers to.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) && p[pi] != '*' {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// The shared-mime-info globs: (mime, glob).
fn mime_globs() -> &'static [(String, String)] {
    static GLOBS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    GLOBS.get_or_init(|| {
        let text = std::fs::read_to_string("/usr/share/mime/globs2").unwrap_or_default();
        let mut globs = parse_globs2(&text);
        // Without the database, the types apps ask for most.
        if globs.is_empty() {
            globs = [
                ("image/png", "*.png"),
                ("image/jpeg", "*.jpg"),
                ("image/jpeg", "*.jpeg"),
                ("image/gif", "*.gif"),
                ("image/webp", "*.webp"),
                ("image/svg+xml", "*.svg"),
                ("text/plain", "*.txt"),
                ("text/markdown", "*.md"),
                ("text/html", "*.html"),
                ("application/pdf", "*.pdf"),
                ("application/json", "*.json"),
                ("application/zip", "*.zip"),
                ("audio/mpeg", "*.mp3"),
                ("video/mp4", "*.mp4"),
            ]
            .map(|(m, g)| (m.to_string(), g.to_string()))
            .to_vec();
        }
        globs
    })
}

/// `weight:mime:glob[:flags]` lines of a `globs2` file.
fn parse_globs2(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.splitn(4, ':');
            let (_weight, mime, glob) = (parts.next()?, parts.next()?, parts.next()?);
            (glob != "__NOGLOBS__").then(|| (mime.to_string(), glob.to_string()))
        })
        .collect()
}

/// Is `name` of the mime type `pattern` (`image/png`, `image/*`), going by
/// its file name?
pub fn mime_matches(pattern: &str, name: &str) -> bool {
    mime_matches_in(mime_globs(), pattern, name)
}

fn mime_matches_in(globs: &[(String, String)], pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let prefix = pattern.strip_suffix('*');
    globs.iter().any(|(mime, g)| {
        let hit = match prefix {
            Some(prefix) => mime.starts_with(prefix),
            None => *mime == pattern,
        };
        hit && glob(g, name)
    })
}

/// A path option: a NUL-terminated byte string.
fn path_of(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    (!bytes.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

fn get<T: TryFrom<OwnedValue>>(options: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    T::try_from(options.get(key)?.try_clone().ok()?).ok()
}

impl Request {
    /// The options of OpenFile (`kind` Open), SaveFile (Save) and SaveFiles
    /// (Folder) calls.
    pub fn from_options(kind: Kind, title: &str, options: &HashMap<String, OwnedValue>) -> Self {
        let mut kind = kind;
        let flag = |key: &str| get::<bool>(options, key).unwrap_or(false);
        if kind == Kind::Open && flag("directory") {
            kind = Kind::Folder;
        }
        let mut filters: Vec<Filter> = options
            .get("filters")
            .and_then(|v| Vec::<RawFilter>::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default()
            .into_iter()
            .map(Filter::from_raw)
            .collect();
        // The current filter may be one of the list, or one of its own.
        let current = options
            .get("current_filter")
            .and_then(|v| RawFilter::try_from(v.try_clone().ok()?).ok())
            .map(Filter::from_raw);
        let current_filter = match current {
            Some(f) => filters.iter().position(|x| *x == f).unwrap_or_else(|| {
                filters.push(f);
                filters.len() - 1
            }),
            None => 0,
        };
        let bytes = |key: &str| get::<Vec<u8>>(options, key).and_then(|b| path_of(&b));
        let current_file = bytes("current_file");
        let name = get::<String>(options, "current_name")
            .or_else(|| {
                let file = current_file.as_deref()?.file_name()?;
                Some(file.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let folder = bytes("current_folder")
            .or_else(|| Some(current_file.as_deref()?.parent()?.to_path_buf()));
        Request {
            kind,
            title: title.to_string(),
            accept: get::<String>(options, "accept_label").unwrap_or_default(),
            multiple: flag("multiple"),
            filters,
            current_filter,
            name,
            folder,
        }
    }

    /// The command line for `cnde-files`.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--pick".to_string(),
            match self.kind {
                Kind::Open => "open",
                Kind::Save => "save",
                Kind::Folder => "folder",
            }
            .to_string(),
            "--title".into(),
            self.title.clone(),
        ];
        if !self.accept.is_empty() {
            args.extend(["--accept".into(), self.accept.clone()]);
        }
        if self.multiple {
            args.push("--multiple".into());
        }
        if !self.name.is_empty() {
            args.extend(["--name".into(), self.name.clone()]);
        }
        if let Some(folder) = &self.folder {
            args.extend(["--folder".into(), folder.to_string_lossy().into_owned()]);
        }
        for f in &self.filters {
            args.extend(["--filter".into(), encode_filter(f)]);
        }
        if !self.filters.is_empty() {
            args.extend(["--current".into(), self.current_filter.to_string()]);
        }
        args
    }

    /// Read the arguments after `--pick`; `None` if they make no sense.
    pub fn from_args(args: &[String]) -> Option<Self> {
        let mut it = args.iter();
        let kind = match it.next()?.as_str() {
            "open" => Kind::Open,
            "save" => Kind::Save,
            "folder" => Kind::Folder,
            _ => return None,
        };
        let mut req = Request {
            kind,
            title: String::new(),
            accept: String::new(),
            multiple: false,
            filters: Vec::new(),
            current_filter: 0,
            name: String::new(),
            folder: None,
        };
        while let Some(flag) = it.next() {
            if flag == "--multiple" {
                req.multiple = true;
                continue;
            }
            let value = it.next()?;
            match flag.as_str() {
                "--title" => req.title = value.clone(),
                "--accept" => req.accept = value.clone(),
                "--name" => req.name = value.clone(),
                "--folder" => req.folder = Some(PathBuf::from(value)),
                "--filter" => req.filters.push(decode_filter(value)),
                "--current" => req.current_filter = value.parse().ok()?,
                _ => return None,
            }
        }
        req.current_filter = req.current_filter.min(req.filters.len().saturating_sub(1));
        Some(req)
    }
}

/// `label TAB g:pattern TAB m:mime ...`
fn encode_filter(f: &Filter) -> String {
    let clean = |s: &str| s.replace(['\t', '\n'], " ");
    let mut out = clean(&f.label);
    for rule in &f.rules {
        out.push('\t');
        match rule {
            Rule::Glob(p) => out.push_str(&format!("g:{}", clean(p))),
            Rule::Mime(p) => out.push_str(&format!("m:{}", clean(p))),
        }
    }
    out
}

fn decode_filter(s: &str) -> Filter {
    let mut parts = s.split('\t');
    Filter {
        label: parts.next().unwrap_or_default().to_string(),
        rules: parts
            .filter_map(|p| match p.split_once(':')? {
                ("g", pattern) => Some(Rule::Glob(pattern.to_string())),
                ("m", pattern) => Some(Rule::Mime(pattern.to_string())),
                _ => None,
            })
            .collect(),
    }
}

/// What the picker prints when the user accepted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub paths: Vec<PathBuf>,
    /// Index of the filter in use.
    pub filter: Option<usize>,
}

impl Answer {
    /// Lines `path <bytes escaped>` and `filter <n>`; paths are escaped so
    /// newlines in names survive.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for p in &self.paths {
            out.push_str("path ");
            out.push_str(&escape(&p.to_string_lossy()));
            out.push('\n');
        }
        if let Some(f) = self.filter {
            out.push_str(&format!("filter {f}\n"));
        }
        out
    }

    pub fn parse(text: &str) -> Self {
        let mut answer = Answer::default();
        for line in text.lines() {
            if let Some(p) = line.strip_prefix("path ") {
                answer.paths.push(PathBuf::from(unescape(p)));
            } else if let Some(f) = line.strip_prefix("filter ") {
                answer.filter = f.parse().ok();
            }
        }
        answer
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A `file://` URI for `path`, percent-encoding all but unreserved bytes
/// and `/`.
pub fn uri_of(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
fn owned(v: impl Into<zbus::zvariant::Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(label: &str, rules: &[(u32, &str)]) -> RawFilter {
        (
            label.into(),
            rules.iter().map(|&(k, p)| (k, p.to_string())).collect(),
        )
    }

    #[test]
    fn globs() {
        assert!(glob("*.png", "a.png"));
        assert!(glob("*.png", "A.PNG"));
        assert!(!glob("*.png", "a.png.bak"));
        assert!(glob("*", ""));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "ac"));
        assert!(glob("*.tar.*", "x.tar.gz"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
        assert!(glob("README", "readme"));
    }

    #[test]
    fn mimes() {
        let globs = parse_globs2(
            "# comment\n80:image/png:*.png\n50:image/jpeg:*.jpg\n0:model/x:__NOGLOBS__\n\
             50:text/plain:*.txt:cs\n",
        );
        assert_eq!(globs.len(), 3);
        assert!(mime_matches_in(&globs, "image/png", "a.PNG"));
        assert!(!mime_matches_in(&globs, "image/png", "a.jpg"));
        assert!(mime_matches_in(&globs, "image/*", "a.jpg"));
        assert!(!mime_matches_in(&globs, "image/*", "a.txt"));
        assert!(mime_matches_in(&globs, "Text/Plain", "b.txt"));
        assert!(!mime_matches_in(&globs, "text/plain", "noext"));
    }

    #[test]
    fn filters_match() {
        let f = Filter::from_raw(filter(
            "Pictures",
            &[(0, "*.png"), (1, "image/jpeg"), (7, "x")],
        ));
        assert_eq!(f.rules.len(), 2);
        assert!(f.matches("a.png"));
        assert!(f.matches("b.jpg") || mime_globs().is_empty());
        assert!(!f.matches("c.txt"));
        let all = Filter::from_raw(filter("All", &[]));
        assert!(all.matches("anything"));
        assert_eq!(Filter::from_raw(f.to_raw()), f);
    }

    #[test]
    fn options() {
        let mut o: HashMap<String, OwnedValue> = HashMap::new();
        o.insert("multiple".into(), owned(true));
        o.insert("accept_label".into(), owned("Choose"));
        o.insert(
            "filters".into(),
            owned(vec![
                filter("Images", &[(1, "image/*")]),
                filter("Text", &[(0, "*.txt")]),
            ]),
        );
        o.insert(
            "current_filter".into(),
            owned(filter("Text", &[(0, "*.txt")])),
        );
        o.insert("current_folder".into(), owned(b"/tmp/x\0".to_vec()));
        let r = Request::from_options(Kind::Open, "Pick", &o);
        assert_eq!(r.kind, Kind::Open);
        assert!(r.multiple);
        assert_eq!(r.accept, "Choose");
        assert_eq!(r.filters.len(), 2);
        assert_eq!(r.current_filter, 1);
        assert_eq!(r.folder.as_deref(), Some(Path::new("/tmp/x")));

        // A current filter outside the list is added to it.
        o.insert(
            "current_filter".into(),
            owned(filter("Other", &[(0, "*.o")])),
        );
        let r = Request::from_options(Kind::Open, "Pick", &o);
        assert_eq!((r.filters.len(), r.current_filter), (3, 2));

        // Directory turns Open into a folder pick; Save takes the file's
        // name and folder.
        let mut o: HashMap<String, OwnedValue> = HashMap::new();
        o.insert("directory".into(), owned(true));
        assert_eq!(Request::from_options(Kind::Open, "", &o).kind, Kind::Folder);
        let mut o: HashMap<String, OwnedValue> = HashMap::new();
        o.insert("current_file".into(), owned(b"/home/me/doc.txt\0".to_vec()));
        let r = Request::from_options(Kind::Save, "Save", &o);
        assert_eq!(r.name, "doc.txt");
        assert_eq!(r.folder.as_deref(), Some(Path::new("/home/me")));
        o.insert("current_name".into(), owned("new.txt"));
        assert_eq!(Request::from_options(Kind::Save, "", &o).name, "new.txt");
        // Wrong types are ignored.
        let mut o: HashMap<String, OwnedValue> = HashMap::new();
        o.insert("multiple".into(), owned("yes"));
        assert!(!Request::from_options(Kind::Open, "", &o).multiple);
    }

    #[test]
    fn args_round_trip() {
        let r = Request {
            kind: Kind::Save,
            title: "Save as".into(),
            accept: "Keep".into(),
            multiple: true,
            filters: vec![
                Filter {
                    label: "Text\tfiles".into(),
                    rules: vec![Rule::Glob("*.txt".into()), Rule::Mime("text/plain".into())],
                },
                Filter {
                    label: "All".into(),
                    rules: vec![],
                },
            ],
            current_filter: 1,
            name: "a b.txt".into(),
            folder: Some(PathBuf::from("/tmp")),
        };
        let args = r.to_args();
        assert_eq!(args[0], "--pick");
        let back = Request::from_args(&args[1..]).unwrap();
        let mut want = r.clone();
        want.filters[0].label = "Text files".into();
        assert_eq!(back, want);
        assert!(Request::from_args(&["nope".to_string()]).is_none());
        assert!(Request::from_args(&["open".into(), "--bogus".into(), "x".into()]).is_none());
    }

    #[test]
    fn answers() {
        let a = Answer {
            paths: vec![PathBuf::from("/tmp/a\nb"), PathBuf::from("/tmp/c d")],
            filter: Some(2),
        };
        let text = a.render();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(Answer::parse(&text), a);
        assert_eq!(
            uri_of(Path::new("/tmp/a b#é")),
            "file:///tmp/a%20b%23%C3%A9"
        );
    }
}
