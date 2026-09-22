//! SIGHUP handling for the daemon, via an async-signal-safe pipe: the
//! signal handler writes one byte into a non-blocking pipe, and the daemon's
//! main loop polls the pipe's read end. No locks, no allocations, in the
//! handler.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

static READ_FD: AtomicI32 = AtomicI32::new(-1);
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// The SIGHUP handler. Async-signal-safe: reads the published fd and writes
/// one byte into the pipe, nothing else.
extern "C" fn on_sighup(_sig: libc::c_int) {
    let fd = unsafe { AtomicI32::from_ptr(WRITE_FD.as_ptr()) }.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte: libc::c_int = 1;
        unsafe {
            libc::write(fd, &byte as *const _ as *const _, 1);
        }
    }
}

/// A registered SIGHUP handler whose deliveries are reported through
/// [`Sighup::take`].
#[derive(Debug)]
pub struct Sighup {
    read_fd: RawFd,
}

impl Sighup {
    /// Install the SIGHUP handler and the pipe.
    ///
    /// SIGHUP is blocked for the calling thread for the duration of the
    /// setup, so a signal arriving in the window between the handler
    /// installation and the pipe being ready cannot be lost.
    pub fn new() -> std::io::Result<Self> {
        // 1. Block SIGHUP on this thread (we unblock it at the end).
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGHUP);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // 2. Create the pipe and make the read end non-blocking.
        let mut fds: [libc::c_int; 2] = [-1, -1];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(std::io::Error::last_os_error());
        }

        // 3. Install the (now safe) handler via `sigaction`, publish the
        //    fds, and unblock SIGHUP.
        let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
        act.sa_sigaction = on_sighup as *const () as libc::sighandler_t;
        act.sa_flags = 0;
        let rc = unsafe {
            libc::sigemptyset(&mut act.sa_mask);
            libc::sigaction(libc::SIGHUP, &act, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        READ_FD.store(read_fd, Ordering::SeqCst);
        WRITE_FD.store(write_fd, Ordering::SeqCst);
        let rc = unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self { read_fd })
    }

    /// Whether a SIGHUP has been delivered since the last call. Consumes and
    /// discards all pending signal bytes.
    pub fn take(&self) -> bool {
        let mut got = false;
        loop {
            let mut buf = [0u8; 8];
            let n = unsafe { libc::read(self.read_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                got = true;
                continue;
            }
            break;
        }
        got
    }
}

impl Drop for Sighup {
    fn drop(&mut self) {
        unsafe {
            // Restore the default disposition and close the pipe ends.
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = libc::SIG_DFL;
            act.sa_flags = 0;
            libc::sigemptyset(&mut act.sa_mask);
            libc::sigaction(libc::SIGHUP, &act, std::ptr::null_mut());
            let fd = READ_FD.swap(-1, Ordering::SeqCst);
            if fd >= 0 {
                libc::close(fd);
            }
            let wfd = WRITE_FD.swap(-1, Ordering::SeqCst);
            if wfd >= 0 {
                libc::close(wfd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_byte_is_reported_by_take() {
        let sighup = Sighup::new().unwrap();
        assert!(!sighup.take(), "no signal yet");
        unsafe {
            libc::kill(libc::getpid(), libc::SIGHUP);
        }
        // The handler runs on a different thread; give it a moment.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(sighup.take(), "SIGHUP should be reported");
        assert!(!sighup.take(), "and consumed");
    }
}
