//! Stall watchdog: evidence for the next time the main loop hangs.
//!
//! The main loop bumps a heartbeat every iteration, and a 1 s calloop timer
//! bumps it while idle. A plain thread checks it once a second; when it hasn't
//! moved for [`STALL_AFTER`] it logs one error with the loop phase, the main
//! thread's kernel state (`wchan` names the wait, e.g. a DRM fence) and a
//! backtrace of the main thread, then logs once more when the loop recovers.
//!
//! The backtrace comes from a SIGUSR2 sent to the main thread whose handler
//! calls `Backtrace::force_capture`. That is not async-signal-safe (it
//! allocates), so if the main thread hung inside malloc the capture never
//! arrives; the watchdog gives up after a second and logs without it. It is a
//! last-resort diagnostic on an already hung process; and the signal is only
//! sent while the thread is blocked in the kernel, never inside malloc.
//! The handler only fills a buffer: all logging happens on the watchdog thread,
//! since the main thread might be stuck holding the stderr lock.

use std::{
    backtrace::Backtrace,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use smithay::reexports::calloop::{
    LoopHandle,
    timer::{TimeoutAction, Timer},
};
use tracing::{error, info};

use crate::state::State;

const STALL_AFTER: Duration = Duration::from_secs(5);
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// Which part of the main loop is running, for the stall report.
#[derive(Clone, Copy)]
pub enum Phase {
    /// Waiting for events or running their handlers (between iterations).
    Dispatch = 0,
    /// Housekeeping at the top of `on_loop_iteration`.
    Housekeeping = 1,
    /// Rendering and presenting frames.
    Render = 2,
    /// Capture, IPC sync and flushing clients.
    Finish = 3,
}

const PHASE_NAMES: [&str; 4] = [
    "dispatch (waiting for or handling events)",
    "housekeeping",
    "render",
    "finish (captures, IPC sync, flush)",
];

/// Milliseconds since `START` at the last sign of life.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static PHASE: AtomicU8 = AtomicU8::new(Phase::Dispatch as u8);
static START: OnceLock<Instant> = OnceLock::new();
static MAIN_TID: AtomicI32 = AtomicI32::new(0);
/// Set by the signal handler once `BACKTRACE` is filled.
static CAPTURED: AtomicBool = AtomicBool::new(false);
static BACKTRACE: Mutex<String> = Mutex::new(String::new());

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Mark the loop as alive and in `phase`.
pub fn beat(phase: Phase) {
    PHASE.store(phase as u8, Ordering::Relaxed);
    HEARTBEAT.store(now_ms(), Ordering::Relaxed);
}

/// Call on the main thread before the loop starts.
pub fn start(handle: &LoopHandle<'static, State>) {
    beat(Phase::Dispatch);
    // Idle loops block indefinitely; this keeps them visibly alive.
    let _ = handle.insert_source(Timer::from_duration(CHECK_EVERY), |_, _, _| {
        HEARTBEAT.store(now_ms(), Ordering::Relaxed);
        TimeoutAction::ToDuration(CHECK_EVERY)
    });

    // Safety: plain syscalls; the handler is installed before the thread that
    // can raise the signal exists.
    unsafe {
        MAIN_TID.store(libc::syscall(libc::SYS_gettid) as i32, Ordering::Relaxed);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = capture_backtrace as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGUSR2, &action, std::ptr::null_mut());
    }
    if let Err(err) = thread::Builder::new()
        .name("stall-watchdog".into())
        .spawn(watch)
    {
        error!(?err, "could not start the stall watchdog");
    }
}

/// SIGUSR2 handler, runs on the main thread. See the module docs for why
/// the non-async-signal-safe capture is accepted.
extern "C" fn capture_backtrace(_: libc::c_int) {
    let trace = Backtrace::force_capture().to_string();
    if let Ok(mut slot) = BACKTRACE.try_lock() {
        *slot = trace;
        CAPTURED.store(true, Ordering::Release);
    }
}

fn watch() {
    let mut stalled_since: Option<u64> = None;
    loop {
        thread::sleep(CHECK_EVERY);
        let beat_at = HEARTBEAT.load(Ordering::Relaxed);
        let age = Duration::from_millis(now_ms().saturating_sub(beat_at));
        match stalled_since {
            None if age >= STALL_AFTER => {
                stalled_since = Some(beat_at);
                report(age);
            }
            Some(since) if beat_at != since => {
                stalled_since = None;
                let secs = beat_at.saturating_sub(since) / 1000;
                info!("main loop recovered after about {secs} s");
            }
            _ => {}
        }
    }
}

fn report(age: Duration) {
    let tid = MAIN_TID.load(Ordering::Relaxed);
    let phase = PHASE_NAMES
        .get(PHASE.load(Ordering::Relaxed) as usize)
        .copied()
        .unwrap_or("?");

    let proc = |file: &str| {
        std::fs::read_to_string(format!("/proc/self/task/{tid}/{file}"))
            .map(|text| text.trim().to_string())
            .unwrap_or_else(|err| format!("<{err}>"))
    };
    let stat = proc("stat");
    // The state letter follows the ")" that closes the thread name.
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .unwrap_or("?")
        .to_string();

    // Only interrupt a thread that's blocked in the kernel: one running
    // user code could be inside malloc, and the handler's allocation would
    // then deadlock it for good.
    let mut backtrace = None;
    if state == "S" || state == "D" {
        CAPTURED.store(false, Ordering::Release);
        // Safety: tgkill on our own main thread.
        unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGUSR2) };
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(100));
            if CAPTURED.load(Ordering::Acquire) {
                backtrace = BACKTRACE.lock().ok().map(|trace| trace.clone());
                break;
            }
        }
    }

    let text = format!(
        "main loop stalled for {} s in phase: {phase}\n\
         main thread state: {state} (R running, S sleeping, D uninterruptible, e.g. GPU/DRM)\n\
         wchan: {}\nsyscall: {}\nkernel stack: {}\nbacktrace:\n{}",
        age.as_secs(),
        proc("wchan"),
        proc("syscall"),
        proc("stack"),
        backtrace.as_deref().unwrap_or(
            "<not captured: main thread running user code, or unresponsive to signals>"
        ),
    );
    error!("{text}");
    save(&text);
}

/// Also keep the report on disk, synced: the journal's last lines are lost
/// if the machine is then hard-reset.
fn save(text: &str) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/state")))
    else {
        return;
    };
    let dir = dir.join("cnde");
    let path = dir.join("stall.log");
    let written = std::fs::create_dir_all(&dir).and_then(|()| {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        writeln!(file, "--- unix time {stamp}\n{text}\n")?;
        file.sync_all()
    });
    if let Err(err) = written {
        error!(?err, path = %path.display(), "could not save the stall report");
    }
}
