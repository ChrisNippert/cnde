//! The Default Apps page: which installed app opens what. Defaults are the
//! freedesktop `mimeapps.list` in the config dir, written through
//! `cnde-apps`; the index of installed apps is built off the UI thread.

use cnde_apps::{App, Env, Index, MimeApps, mime};

/// One row of the page: a kind of app and the types it opens.
pub struct Category {
    pub id: &'static str,
    pub title: &'static str,
    pub subtitle: &'static str,
    /// The MIME types it covers; picking an app sets it for each of them
    /// the app handles.
    pub types: &'static [&'static str],
}

pub const CATEGORIES: [Category; 8] = [
    Category {
        id: "browser",
        title: "Web browser",
        subtitle: "Opens links and web pages",
        types: &[
            "x-scheme-handler/http",
            "x-scheme-handler/https",
            "text/html",
            "application/xhtml+xml",
        ],
    },
    Category {
        id: "email",
        title: "Email",
        subtitle: "Opens email links",
        types: &["x-scheme-handler/mailto"],
    },
    Category {
        id: "files",
        title: "Files",
        subtitle: "Opens folders",
        types: &["inode/directory"],
    },
    Category {
        id: "images",
        title: "Images",
        subtitle: "Opens pictures",
        types: &[
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "image/avif",
            "image/svg+xml",
            "image/bmp",
            "image/tiff",
        ],
    },
    Category {
        id: "video",
        title: "Video",
        subtitle: "Opens movies and clips",
        types: &[
            "video/mp4",
            "video/x-matroska",
            "video/webm",
            "video/quicktime",
            "video/x-msvideo",
        ],
    },
    Category {
        id: "music",
        title: "Music",
        subtitle: "Opens songs and audio files",
        types: &[
            "audio/mpeg",
            "audio/flac",
            "audio/ogg",
            "audio/x-wav",
            "audio/mp4",
            "audio/opus",
        ],
    },
    Category {
        id: "text",
        title: "Text editor",
        subtitle: "Opens text, Markdown and scripts",
        types: &["text/plain", "text/markdown", "application/x-shellscript"],
    },
    Category {
        id: "pdf",
        title: "PDF viewer",
        subtitle: "Opens PDF documents",
        types: &["application/pdf"],
    },
];

/// The category with this id.
pub fn category(id: &str) -> Option<&'static Category> {
    CATEGORIES.iter().find(|c| c.id == id)
}

/// The installed apps and the association lists, as last read.
pub struct State {
    /// Every app, by name.
    pub apps: Vec<App>,
    pub lists: Vec<MimeApps>,
    pub env: Env,
}

impl State {
    /// Scan the desktop files and read the lists (call off the UI thread).
    pub fn load(env: Env) -> State {
        let lists = cnde_apps::load_lists(&env);
        let Index { apps, env } = Index::build(env);
        State { apps, lists, env }
    }

    /// Read the lists again, in case they were edited elsewhere.
    pub fn reload_lists(&mut self) {
        self.lists = cnde_apps::load_lists(&self.env);
    }

    /// The apps that can be picked for `cat`: shown ones that handle any of
    /// its types, then the current default if it isn't one of them.
    pub fn offered(&self, cat: &Category) -> Vec<&App> {
        let mut out: Vec<&App> = self
            .apps
            .iter()
            .filter(|a| !a.no_display && !handled_types(a, cat).is_empty())
            .collect();
        if let Some(cur) = self.current(cat)
            && !out.iter().any(|a| a.id == cur.id)
        {
            out.push(cur);
        }
        out
    }

    /// The app that opens the first of the category's types that has one.
    pub fn current(&self, cat: &Category) -> Option<&App> {
        cat.types.iter().find_map(|t| {
            cnde_apps::resolve(&self.apps, &self.lists, &mime::chain(t))
                .first()
                .map(|&i| &self.apps[i])
        })
    }

    /// Make `id` the default for each of the category's types it handles.
    pub fn set(&mut self, cat: &Category, id: &str) -> std::io::Result<()> {
        let app = self
            .apps
            .iter()
            .find(|a| a.id == id)
            .ok_or_else(|| std::io::Error::other(format!("{id} isn't installed")))?;
        let types = handled_types(app, cat);
        if types.is_empty() {
            return Err(std::io::Error::other(format!(
                "{} doesn't open {}",
                app.name, cat.title
            )));
        }
        cnde_apps::set_defaults(&self.env, &types, id)?;
        self.reload_lists();
        Ok(())
    }

    /// The terminal emulators that can be picked: those with the
    /// `TerminalEmulator` category.
    pub fn terminals(&self) -> Vec<&App> {
        self.apps
            .iter()
            .filter(|a| !a.no_display && a.categories.iter().any(|c| c == "TerminalEmulator"))
            .collect()
    }
}

/// The file picker that goes with a file manager (xdg-desktop-portal
/// backends, most wanted first): its own where it has one, else GTK's.
pub fn file_chooser_for(app_id: &str) -> &'static str {
    match app_id.trim_end_matches(".desktop") {
        "cnde-files" => "cnde;gtk",
        "org.gnome.Nautilus" | "nautilus" => "gnome;gtk",
        "org.kde.dolphin" => "kde;gtk",
        _ => "gtk",
    }
}

/// Point the portal's file picker at `backends` in the user's
/// `cnde-portals.conf` (the compositor puts it there at first login).
/// Whether it changed.
pub fn set_file_chooser(env: &Env, backends: &str) -> std::io::Result<bool> {
    let path = env.config_home.join("xdg-desktop-portal/cnde-portals.conf");
    let old = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            include_str!("../../data/cnde-portals.conf").to_string()
        }
        Err(err) => return Err(err),
    };
    let new = with_file_chooser(&old, backends);
    if new == old && path.exists() {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, new)?;
    Ok(true)
}

const FILE_CHOOSER: &str = "org.freedesktop.impl.portal.FileChooser=";

/// `conf` with its FileChooser line set to `backends` (added under
/// `[preferred]` if it has none); the rest left as it was.
fn with_file_chooser(conf: &str, backends: &str) -> String {
    let line = format!("{FILE_CHOOSER}{backends}");
    let mut lines: Vec<String> = conf.lines().map(str::to_string).collect();
    if let Some(old) = lines.iter_mut().find(|l| l.trim_start().starts_with(FILE_CHOOSER)) {
        *old = line;
    } else if let Some(at) = lines.iter().position(|l| l.trim() == "[preferred]") {
        lines.insert(at + 1, line);
    } else {
        lines.extend(["[preferred]".to_string(), line]);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Stop the running portal (the one on our session bus), so it reads its
/// config again: D-Bus starts it afresh the next time an app asks.
pub fn restart_portal() {
    let pid = (|| -> zbus::Result<u32> {
        let conn = zbus::blocking::Connection::session()?;
        let bus = zbus::blocking::fdo::DBusProxy::new(&conn)?;
        let name = zbus::names::BusName::try_from("org.freedesktop.portal.Desktop")?;
        Ok(bus.get_connection_unix_process_id(name)?)
    })();
    // Not running: it reads the new config when it starts.
    if let Ok(pid) = pid {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    }
}

/// The category's types `app` lists.
fn handled_types<'a>(app: &App, cat: &Category) -> Vec<&'a str> {
    cat.types
        .iter()
        .copied()
        .filter(|t| app.mimes.iter().any(|m| m == t))
        .collect()
}

/// Some apps in a temp tree, for tests and the preview.
#[cfg(test)]
pub fn fake_state(root: &std::path::Path) -> State {
    let web = "x-scheme-handler/http;x-scheme-handler/https;text/html;";
    let apps: &[(&str, &str, &str)] = &[
        ("firefox", "Firefox", web),
        ("chromium", "Chromium", web),
        ("thunderbird", "Thunderbird", "x-scheme-handler/mailto;"),
        ("nautilus", "Files", "inode/directory;"),
        (
            "loupe",
            "Image Viewer",
            "image/png;image/jpeg;image/webp;image/gif;",
        ),
        ("gimp", "GIMP", "image/png;image/jpeg;image/tiff;"),
        ("mpv", "mpv", "video/mp4;video/webm;audio/mpeg;audio/flac;"),
        ("gedit", "Text Editor", "text/plain;text/markdown;"),
        ("evince", "Document Viewer", "application/pdf;"),
        ("hidden", "Hidden Thing", "application/pdf;"),
    ];
    let dir = root.join("data/applications");
    std::fs::create_dir_all(&dir).unwrap();
    for (id, name, mimes) in apps {
        let extra = if *id == "hidden" {
            "NoDisplay=true\n"
        } else {
            ""
        };
        std::fs::write(
            dir.join(format!("{id}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec={id}\nMimeType={mimes}\n{extra}"
            ),
        )
        .unwrap();
    }
    for id in ["foot", "kitty"] {
        std::fs::write(
            dir.join(format!("{id}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName={id}\nExec={id}\nCategories=System;TerminalEmulator;\n"
            ),
        )
        .unwrap();
    }
    State::load(Env {
        config_home: root.join("cfg"),
        config_dirs: vec![],
        data_home: root.join("data"),
        data_dirs: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_chooser_line_is_swapped_in_place() {
        let conf = include_str!("../../data/cnde-portals.conf");
        let gtk = with_file_chooser(conf, "gtk");
        assert!(gtk.contains("\norg.freedesktop.impl.portal.FileChooser=gtk\n"));
        assert!(!gtk.contains("=cnde;gtk"));
        assert_eq!(gtk.lines().count(), conf.lines().count());
        assert_eq!(with_file_chooser(&gtk, "cnde;gtk"), conf);
        let bare = with_file_chooser("[preferred]\ndefault=gtk\n", "kde;gtk");
        assert_eq!(
            bare,
            "[preferred]\norg.freedesktop.impl.portal.FileChooser=kde;gtk\ndefault=gtk\n"
        );
        assert_eq!(file_chooser_for("cnde-files.desktop"), "cnde;gtk");
        assert_eq!(file_chooser_for("thunar"), "gtk");
    }

    #[test]
    fn file_chooser_is_written_to_the_user_config() {
        let root = std::env::temp_dir().join(format!("cnde-chooser-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let env = Env {
            config_home: root.clone(),
            config_dirs: Vec::new(),
            data_home: root.join("data"),
            data_dirs: Vec::new(),
        };
        assert!(set_file_chooser(&env, "gnome;gtk").unwrap());
        assert!(!set_file_chooser(&env, "gnome;gtk").unwrap());
        let text =
            std::fs::read_to_string(root.join("xdg-desktop-portal/cnde-portals.conf")).unwrap();
        assert!(text.contains("FileChooser=gnome;gtk"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cnde-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("cfg")).unwrap();
        d
    }

    fn ids(apps: Vec<&App>) -> Vec<&str> {
        apps.into_iter().map(|a| a.id.as_str()).collect()
    }

    #[test]
    fn categories_cover_their_types() {
        assert!(category("browser").unwrap().types.contains(&"text/html"));
        assert_eq!(category("files").unwrap().types, ["inode/directory"]);
        assert!(category("images").unwrap().types.contains(&"image/svg+xml"));
        assert!(category("nope").is_none());
    }

    #[test]
    fn offers_apps_that_handle_any_type_and_hides_nodisplay() {
        let root = temp("offer");
        let st = fake_state(&root);
        assert_eq!(
            ids(st.offered(category("pdf").unwrap())),
            ["evince.desktop"]
        );
        assert_eq!(
            ids(st.offered(category("images").unwrap())),
            ["gimp.desktop", "loupe.desktop"]
        );
        assert_eq!(ids(st.terminals()), ["foot.desktop", "kitty.desktop"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_the_current_default() {
        let root = temp("current");
        let mut st = fake_state(&root);
        let browser = category("browser").unwrap();
        // No list: the first match by name.
        assert_eq!(st.current(browser).unwrap().id, "chromium.desktop");
        std::fs::write(
            root.join("cfg/mimeapps.list"),
            "[Default Applications]\nx-scheme-handler/http=firefox.desktop;\n",
        )
        .unwrap();
        st.reload_lists();
        assert_eq!(st.current(browser).unwrap().id, "firefox.desktop");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn picking_sets_every_handled_type_and_keeps_the_rest() {
        let root = temp("set");
        let mut st = fake_state(&root);
        std::fs::write(
            root.join("cfg/mimeapps.list"),
            "[Added Associations]\nx/y=a.desktop;\n\n[Default Applications]\nvideo/mp4=other.desktop;\n",
        )
        .unwrap();
        st.reload_lists();
        let images = category("images").unwrap();
        st.set(images, "gimp.desktop").unwrap();
        let text = std::fs::read_to_string(root.join("cfg/mimeapps.list")).unwrap();
        let lists = cnde_apps::parse_mimeapps(&text);
        for t in ["image/png", "image/jpeg", "image/tiff"] {
            assert_eq!(lists.default[t][0], "gimp.desktop", "{t}");
        }
        // Not one GIMP handles.
        assert!(!lists.default.contains_key("image/gif"));
        assert_eq!(lists.default["video/mp4"], ["other.desktop"]);
        assert_eq!(lists.added["x/y"], ["a.desktop"]);
        assert_eq!(st.current(images).unwrap().id, "gimp.desktop");
        assert!(st.set(images, "evince.desktop").is_err());
        assert!(st.set(images, "missing.desktop").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
