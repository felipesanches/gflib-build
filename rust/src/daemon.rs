//! Daemon lifecycle (R4): double-fork detach (so the build keeps running after you quit the
//! monitor), a lingering daemon that stays alive ~30 min after completion (so a live `[R]` retry /
//! control still works), and a SIGTERM handler for graceful `--stop`. Ported from the Python
//! `daemonize()` + the daemon's linger loop.
//!
//! CRITICAL: `daemonize()` must be called BEFORE any worker thread is spawned — `fork()` keeps only
//! the calling thread in the child, so forking after `Orchestrator::start()` would lose the pool.

use crate::build::Orchestrator;
use crate::persist;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn fork() -> i32;
    fn setsid() -> i32;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn signal(signum: i32, handler: usize) -> usize;
}

extern "C" fn on_sigterm(_sig: i32) {
    SIGTERM_RECEIVED.store(true, Ordering::SeqCst);
}

/// Install a SIGTERM (15) handler that flips a flag the daemon loop polls (graceful `--stop`).
pub fn install_sigterm_handler() {
    unsafe {
        signal(15, on_sigterm as *const () as usize);
    }
}

pub fn sigterm_received() -> bool {
    SIGTERM_RECEIVED.load(Ordering::SeqCst)
}

/// Double-fork into a background daemon. Returns true in the daemon (which should run the build) and
/// false in the original parent (which can then attach a monitor). Redirects the daemon's stdio to
/// `daemon.log` and writes `daemon.pid`. Call this BEFORE spawning any threads.
pub fn daemonize(build_dir: &Path) -> bool {
    unsafe {
        let pid = fork();
        if pid > 0 {
            // original parent: reap the short-lived first child, then return to attach a monitor
            let mut status: i32 = 0;
            waitpid(pid, &mut status as *mut i32, 0);
            return false;
        }
        // first child
        setsid();
        if fork() > 0 {
            std::process::exit(0); // first child exits; the grandchild becomes the daemon
        }
        // grandchild = daemon
    }
    let _ = std::fs::create_dir_all(build_dir);
    use std::os::unix::io::AsRawFd;
    if let Ok(log) = std::fs::OpenOptions::new().create(true).append(true).open(build_dir.join("daemon.log")) {
        unsafe {
            dup2(log.as_raw_fd(), 1);
            dup2(log.as_raw_fd(), 2);
        }
        std::mem::forget(log); // keep the fd open for the daemon's lifetime
    }
    if let Ok(devnull) = std::fs::File::open("/dev/null") {
        unsafe {
            dup2(devnull.as_raw_fd(), 0);
        }
        std::mem::forget(devnull);
    }
    persist::write_pid(build_dir);
    true
}

/// The daemon's main loop after `start()`: keep running until the build is done AND has been idle for
/// `linger`, or until SIGTERM. While alive the status writer + control watcher keep serving, so a
/// monitor's `[R]` retry re-queues work live (which resets the idle timer). Clears the pidfile on exit.
pub fn run_daemon(orch: &Arc<Orchestrator>, linger: Duration) {
    install_sigterm_handler();
    let mut done_since: Option<Instant> = None;
    loop {
        if sigterm_received() {
            break;
        }
        let snap = orch.snapshot();
        if snap.done {
            match done_since {
                None => done_since = Some(Instant::now()),
                Some(t) if t.elapsed() >= linger => break,
                _ => {}
            }
        } else {
            done_since = None; // new work (e.g. a live retry) arrived → keep lingering
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    orch.finalize(); // synchronous final status + reports before the daemon exits
    orch.request_stop();
    persist::clear_pid(&orch.cfg.build_dir);
}
