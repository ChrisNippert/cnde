//! `cnde-files-portal`: the xdg-desktop-portal backend for
//! `org.freedesktop.impl.portal.FileChooser`. It owns no window: each
//! request runs `cnde-files --pick ...` (the file manager's views in picker
//! mode) as a child process and turns its answer into the portal's
//! response. Separate from the file manager, which stays single-instance
//! and keeps `org.cnde.Files`.
//!
//! Activated by D-Bus (`org.freedesktop.impl.portal.desktop.cnde`) when
//! the portal frontend first needs a file chooser.

#[path = "request.rs"]
mod request;

use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use rustix::process::{Pid, Signal, kill_process};
use tracing::{debug, warn};
use zbus::{
    Connection, interface,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

use request::{Answer, Kind, Request, uri_of};

const NAME: &str = "org.freedesktop.impl.portal.desktop.cnde";
const PATH: &str = "/org/freedesktop/portal/desktop";

const RESPONSE_OK: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_OTHER: u32 = 2;

type Results = HashMap<String, OwnedValue>;

/// A value set once from another thread, awaited here.
struct Slot<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

struct Done<T>(Arc<Mutex<Slot<T>>>);

impl<T> Future for Done<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut slot = self.0.lock().unwrap();
        match slot.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// What the picker process did.
struct Outcome {
    /// Exit code; `None` if it was killed or never started.
    code: Option<i32>,
    stdout: String,
}

/// One running request, for `Request.Close`.
#[derive(Default)]
struct Job {
    pid: Mutex<Option<u32>>,
    closed: AtomicBool,
}

impl Job {
    fn stop(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let pid = *self.pid.lock().unwrap();
        if let Some(pid) = pid.and_then(|p| Pid::from_raw(p as i32)) {
            let _ = kill_process(pid, Signal::TERM);
        }
    }
}

/// `org.freedesktop.impl.portal.Request` at the request's handle.
struct Handle(Arc<Job>);

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl Handle {
    fn close(&self) {
        self.0.stop();
    }
}

/// The picker binary: `$CNDE_FILES_BIN`, `cnde-files` next to us, or on
/// the PATH.
fn picker_command() -> Command {
    if let Some(bin) = std::env::var_os("CNDE_FILES_BIN") {
        return Command::new(bin);
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("cnde-files")))
        .filter(|p| p.exists());
    Command::new(beside.unwrap_or_else(|| PathBuf::from("cnde-files")))
}

fn run_picker(args: Vec<String>, job: &Job) -> Done<Outcome> {
    let slot = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
    }));
    let finish = {
        let slot = slot.clone();
        move |outcome: Outcome| {
            let mut slot = slot.lock().unwrap();
            slot.value = Some(outcome);
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
        }
    };
    let child = picker_command()
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn();
    match child {
        Ok(child) => {
            *job.pid.lock().unwrap() = Some(child.id());
            let _ = std::thread::Builder::new()
                .name("picker".into())
                .spawn(move || {
                    let outcome = match child.wait_with_output() {
                        Ok(out) => Outcome {
                            code: out.status.code(),
                            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                        },
                        Err(_) => Outcome {
                            code: None,
                            stdout: String::new(),
                        },
                    };
                    finish(outcome);
                });
        }
        Err(err) => {
            warn!(%err, "can't start the picker");
            finish(Outcome {
                code: Some(-1),
                stdout: String::new(),
            });
        }
    }
    Done(slot)
}

/// The portal response for how the picker ended; `names` are the files
/// to put in the chosen folder for SaveFiles.
fn respond(
    kind: Kind,
    outcome: &Outcome,
    closed: bool,
    request: &Request,
    names: Option<&[String]>,
) -> (u32, Results) {
    if closed {
        return (RESPONSE_CANCELLED, Results::new());
    }
    match outcome.code {
        Some(0) => {}
        Some(1) => return (RESPONSE_CANCELLED, Results::new()),
        _ => return (RESPONSE_OTHER, Results::new()),
    }
    let answer = Answer::parse(&outcome.stdout);
    let paths: Vec<PathBuf> = match names {
        // SaveFiles: the folder, with each file in it.
        Some(names) => answer
            .paths
            .first()
            .map(|dir| names.iter().map(|n| dir.join(n)).collect())
            .unwrap_or_default(),
        None => answer.paths.clone(),
    };
    if paths.is_empty() || (kind != Kind::Open && paths.len() > 1 && names.is_none()) {
        return (RESPONSE_OTHER, Results::new());
    }
    let uris: Vec<String> = paths.iter().map(|p| uri_of(p)).collect();
    let mut results = Results::new();
    if let Ok(v) = OwnedValue::try_from(Value::from(uris)) {
        results.insert("uris".into(), v);
    }
    if let Some(filter) = answer.filter.and_then(|i| request.filters.get(i))
        && let Some(v) = filter_value(filter)
    {
        results.insert("current_filter".into(), v);
    }
    (RESPONSE_OK, results)
}

/// A filter as the `(sa(us))` the portal returns.
fn filter_value(filter: &request::Filter) -> Option<OwnedValue> {
    OwnedValue::try_from(Value::from(filter.to_raw())).ok()
}

struct FileChooser;

impl FileChooser {
    async fn run(
        conn: &Connection,
        kind: Kind,
        handle: OwnedObjectPath,
        title: String,
        options: HashMap<String, OwnedValue>,
        names: Option<Vec<String>>,
    ) -> (u32, Results) {
        let request = Request::from_options(kind, &title, &options);
        let job = Arc::new(Job::default());
        let path = ObjectPath::from(&handle);
        let exported = conn
            .object_server()
            .at(&path, Handle(job.clone()))
            .await
            .unwrap_or(false);
        debug!(?kind, %handle, "file chooser request");
        let outcome = run_picker(request.to_args(), &job).await;
        if exported {
            let _ = conn.object_server().remove::<Handle, _>(&path).await;
        }
        let closed = job.closed.load(Ordering::Relaxed);
        respond(kind, &outcome, closed, &request, names.as_deref())
    }
}

#[interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    async fn open_file(
        &self,
        #[zbus(connection)] conn: &Connection,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        Self::run(conn, Kind::Open, handle, title, options, None).await
    }

    async fn save_file(
        &self,
        #[zbus(connection)] conn: &Connection,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        Self::run(conn, Kind::Save, handle, title, options, None).await
    }

    /// Several files into one folder: the user picks the folder.
    async fn save_files(
        &self,
        #[zbus(connection)] conn: &Connection,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let names = file_names(&options);
        Self::run(conn, Kind::Folder, handle, title, options, Some(names)).await
    }
}

/// The names of the `files` option of SaveFiles (`aay`, NUL-terminated).
fn file_names(options: &HashMap<String, OwnedValue>) -> Vec<String> {
    use std::os::unix::ffi::OsStrExt;
    let files = options
        .get("files")
        .and_then(|v| Vec::<Vec<u8>>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();
    files
        .iter()
        .filter_map(|bytes| {
            let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
            let path = std::path::Path::new(std::ffi::OsStr::from_bytes(bytes));
            Some(path.file_name()?.to_string_lossy().into_owned())
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    zbus::block_on(async {
        let _conn = zbus::connection::Builder::session()?
            .name(NAME)?
            .serve_at(PATH, FileChooser)?
            .build()
            .await?;
        tracing::info!("serving {NAME}");
        std::future::pending::<()>().await;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request::from_options(Kind::Open, "t", &HashMap::new())
    }

    fn outcome(code: Option<i32>, stdout: &str) -> Outcome {
        Outcome {
            code,
            stdout: stdout.into(),
        }
    }

    #[test]
    fn exit_codes_become_responses() {
        let r = request();
        let ok = respond(
            Kind::Open,
            &outcome(Some(0), "path /tmp/a b\npath /tmp/c\n"),
            false,
            &r,
            None,
        );
        assert_eq!(ok.0, RESPONSE_OK);
        let uris = Vec::<String>::try_from(ok.1["uris"].try_clone().unwrap()).unwrap();
        assert_eq!(uris, ["file:///tmp/a%20b", "file:///tmp/c"]);
        assert_eq!(
            respond(Kind::Open, &outcome(Some(1), ""), false, &r, None).0,
            RESPONSE_CANCELLED
        );
        assert_eq!(
            respond(Kind::Open, &outcome(None, ""), false, &r, None).0,
            RESPONSE_OTHER
        );
        assert_eq!(
            respond(Kind::Open, &outcome(Some(0), ""), false, &r, None).0,
            RESPONSE_OTHER
        );
        // Closed through the Request object wins over whatever the exit was.
        assert_eq!(
            respond(Kind::Open, &outcome(None, ""), true, &r, None).0,
            RESPONSE_CANCELLED
        );
    }

    #[test]
    fn save_files_join_the_folder() {
        let r = request();
        let names = vec!["a.txt".to_string(), "b c.txt".to_string()];
        let (code, results) = respond(
            Kind::Folder,
            &outcome(Some(0), "path /tmp/out\n"),
            false,
            &r,
            Some(&names),
        );
        assert_eq!(code, RESPONSE_OK);
        let uris = Vec::<String>::try_from(results["uris"].try_clone().unwrap()).unwrap();
        assert_eq!(uris, ["file:///tmp/out/a.txt", "file:///tmp/out/b%20c.txt"]);
    }

    #[test]
    fn answers_carry_the_filter() {
        let mut o: HashMap<String, OwnedValue> = HashMap::new();
        o.insert(
            "filters".into(),
            OwnedValue::try_from(Value::from(vec![(
                "Text".to_string(),
                vec![(0u32, "*.txt".to_string())],
            )]))
            .unwrap(),
        );
        let r = Request::from_options(Kind::Open, "t", &o);
        let (_, results) = respond(
            Kind::Open,
            &outcome(Some(0), "path /a\nfilter 0\n"),
            false,
            &r,
            None,
        );
        assert!(results.contains_key("current_filter"));
    }

    #[test]
    fn names_of_files() {
        let mut o: HashMap<String, OwnedValue> = HashMap::new();
        o.insert(
            "files".into(),
            OwnedValue::try_from(Value::from(vec![
                b"/x/a.txt\0".to_vec(),
                b"b.txt\0".to_vec(),
            ]))
            .unwrap(),
        );
        assert_eq!(file_names(&o), ["a.txt", "b.txt"]);
    }
}
