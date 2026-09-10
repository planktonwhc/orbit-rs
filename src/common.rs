//! Plumbing shared by the gadget and the capture runtime: logging, the RAII
//! wrappers, the worker-thread mechanism the blocking FunctionFS endpoints
//! force on us, and the goggles' outer frame format.

use std::os::unix::thread::JoinHandleExt;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

// --- logging ---------------------------------------------------------------

pub fn log(s: impl AsRef<str>) {
    eprintln!("{}", s.as_ref());
}

/// "a, b, c" -- for telling the user what a board actually has.
pub fn comma_separated(v: &[String]) -> String {
    v.join(", ")
}

// --- signals -------------------------------------------------------------------

/// Deliberately without `SA_RESTART`: a signal has to make the blocking endpoint
/// transfer return `EINTR`, otherwise the kernel restarts it and nothing changes.
pub fn catch_signal(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(sig, &sa, ptr::null_mut());
    }
}

extern "C" fn on_kick(_: libc::c_int) {} // just interrupt the syscall

/// `Worker::stop()` pokes its thread with `SIGUSR1`, which only works if the
/// signal has a handler that does nothing rather than killing the process. Call
/// this once before starting any [`Worker`].
pub fn install_worker_kick_handler() {
    catch_signal(libc::SIGUSR1, on_kick);
}

/// A FunctionFS endpoint transfer blocks until the host moves data, cannot be
/// polled, and only returns early on a signal. So every blocking transfer runs
/// in a `Worker`: a thread that ignores the stop signals (they belong to the
/// main thread) and is poked with `SIGUSR1` until it has actually left.
///
/// The poking is a loop on purpose. A single signal that lands between the
/// worker's quit check and its next transfer is silently consumed by the no-op
/// handler, and the transfer then blocks with nobody left to wake it.
///
/// The callable is passed a `&AtomicBool` quit flag it should check between
/// transfers.
pub struct Worker {
    quit: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    tid: libc::pthread_t,
}

impl Worker {
    pub fn new<F>(f: F) -> Worker
    where
        F: FnOnce(&AtomicBool) + Send + 'static,
    {
        let quit = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let quit_t = Arc::clone(&quit);
        let done_t = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            unsafe {
                let mut stop_signals: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut stop_signals);
                libc::sigaddset(&mut stop_signals, libc::SIGINT);
                libc::sigaddset(&mut stop_signals, libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_BLOCK, &stop_signals, ptr::null_mut());
            }
            let quit_ref: &AtomicBool = &quit_t;
            f(quit_ref);
            done_t.store(true, Ordering::SeqCst);
        });
        let tid = handle.as_pthread_t();
        Worker {
            quit,
            done,
            handle: Some(handle),
            tid,
        }
    }

    pub fn stop(&mut self) {
        let handle = match self.handle.take() {
            Some(h) => h,
            None => return,
        };
        self.quit.store(true, Ordering::SeqCst);
        while !self.done.load(Ordering::SeqCst) {
            unsafe {
                libc::pthread_kill(self.tid, libc::SIGUSR1);
                libc::usleep(10_000);
            }
        }
        let _ = handle.join();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- small RAII helpers ------------------------------------------------------

/// Owns a file descriptor and closes it on drop.
pub struct Fd {
    pub fd: libc::c_int,
}

impl Fd {
    pub fn new(fd: libc::c_int) -> Fd {
        Fd { fd }
    }

    pub fn is_valid(&self) -> bool {
        self.fd >= 0
    }

    pub fn raw(&self) -> libc::c_int {
        self.fd
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

/// Retries short writes and `EINTR`. Returns false with `errno` set otherwise.
pub fn write_all(fd: libc::c_int, buf: &[u8]) -> bool {
    let mut p = buf.as_ptr();
    let mut n = buf.len();
    while n > 0 {
        let w = unsafe { libc::write(fd, p as *const libc::c_void, n) };
        if w < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return false;
        }
        let w = w as usize;
        p = unsafe { p.add(w) };
        n -= w;
    }
    true
}

/// The current thread's `errno`.
pub fn errno() -> libc::c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `strerror(errno)` as a `String`.
pub fn strerror(err: libc::c_int) -> String {
    std::io::Error::from_raw_os_error(err).to_string()
}

// --- the goggles' outer framing --------------------------------------------
// Everything the goggles send is wrapped in 55 CC frames:
//   55 CC <channel> <..> <length lo> <length hi> <seq lo> <seq hi> <payload>
pub const FRAME_HEADER: usize = 8;

pub struct Frame<'a> {
    pub channel: u8,
    pub payload: &'a [u8],
}

/// Next complete frame at or after `pos`, skipping garbage between frames.
/// Returns `None` when the buffer holds no complete frame yet.
pub fn next_frame<'a>(buf: &'a [u8], pos: &mut usize) -> Option<Frame<'a>> {
    while buf.len() - *pos >= FRAME_HEADER {
        let p = *pos;
        if buf[p] != 0x55 || buf[p + 1] != 0xCC {
            *pos += 1;
            continue;
        }
        let length = buf[p + 4] as usize | ((buf[p + 5] as usize) << 8);
        if buf.len() - p < FRAME_HEADER + length {
            return None;
        }
        let frame = Frame {
            channel: buf[p + 2],
            payload: &buf[p + FRAME_HEADER..p + FRAME_HEADER + length],
        };
        *pos += FRAME_HEADER + length;
        return Some(frame);
    }
    None
}
