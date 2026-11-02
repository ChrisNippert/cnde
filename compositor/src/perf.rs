//! Where the compositor's time goes, for a performance run (the
//! `tools/perf` sampler turns it on and off over IPC).
//!
//! While on, timed sections add up per quarter second and each one that
//! ran is written as a row of `$XDG_RUNTIME_DIR/cnde-perf.csv`:
//! `unix_ms,section,calls,total_us,max_us`. Sections nest (`frame` holds
//! `frame.*`), so totals of different levels don't add up. Off, a timing
//! point costs an atomic load.

use std::{
    cell::RefCell,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tracing::{info, warn};

const BUCKET: Duration = Duration::from_millis(250);

static ON: AtomicBool = AtomicBool::new(false);

thread_local! {
    static RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) };
}

struct Recorder {
    out: BufWriter<File>,
    since: Instant,
    /// Section, calls, total and longest, this bucket.
    sections: Vec<(&'static str, u32, Duration, Duration)>,
}

impl Recorder {
    fn add(&mut self, name: &'static str, took: Duration) {
        match self.sections.iter_mut().find(|s| s.0 == name) {
            Some(s) => {
                s.1 += 1;
                s.2 += took;
                s.3 = s.3.max(took);
            }
            None => self.sections.push((name, 1, took, took)),
        }
        if self.since.elapsed() >= BUCKET {
            self.write();
        }
    }

    fn write(&mut self) {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        for (name, calls, total, max) in self.sections.drain(..) {
            let _ = writeln!(
                self.out,
                "{ms},{name},{calls},{},{}",
                total.as_micros(),
                max.as_micros()
            );
        }
        let _ = self.out.flush();
        self.since = Instant::now();
    }
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("cnde-perf.csv"))
}

/// Start recording (afresh) or stop.
pub fn set(on: bool) {
    RECORDER.with(|r| {
        let mut r = r.borrow_mut();
        if let Some(mut old) = r.take() {
            old.write();
        }
        ON.store(false, Ordering::Relaxed);
        if !on {
            info!("performance recording stopped");
            return;
        }
        let Some(path) = path() else {
            warn!("no XDG_RUNTIME_DIR to record performance in");
            return;
        };
        match File::create(&path) {
            Ok(file) => {
                let mut out = BufWriter::new(file);
                let _ = writeln!(out, "unix_ms,section,calls,total_us,max_us");
                *r = Some(Recorder {
                    out,
                    since: Instant::now(),
                    sections: Vec::new(),
                });
                ON.store(true, Ordering::Relaxed);
                info!(path = %path.display(), "recording performance");
            }
            Err(err) => warn!(?err, path = %path.display(), "can't record performance"),
        }
    });
}

/// A section's start, if recording.
pub fn now() -> Option<Instant> {
    ON.load(Ordering::Relaxed).then(Instant::now)
}

/// A section that began at `start` (from [`now`]) ends.
pub fn since(name: &'static str, start: Option<Instant>) {
    if let Some(start) = start {
        let took = start.elapsed();
        RECORDER.with(|r| {
            if let Some(r) = r.borrow_mut().as_mut() {
                r.add(name, took);
            }
        });
    }
}

/// Time `f` as `name`.
pub fn time<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    let start = now();
    let result = f();
    since(name, start);
    result
}
