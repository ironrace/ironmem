//! Forked children for the crate's unit tests, and only for them.
//!
//! A few tests need a real child that never execs: one that holds a
//! descriptor it inherited, or changes an attribute of itself. Such a child
//! is a copy of a multi-threaded test binary, so it makes only
//! async-signal-safe calls, and the test that forked it must not leave it
//! behind, whichever way the test ends.

use std::os::unix::io::RawFd;

/// A forked child of this test process: killed and reaped when dropped,
/// however the test ends.
pub(crate) struct Reap(libc::pid_t);

impl Reap {
    /// The guard for `child`, the pid `fork` returned to the parent.
    ///
    /// Only a pid that names one process is taken. `kill` reads 0 as the
    /// caller's whole process group and -1 as every process the user may
    /// signal, and those are what `fork` returns in the child and when it
    /// fails.
    pub(crate) fn new(child: libc::pid_t) -> Self {
        assert!(
            child > 0,
            "{child} is not the pid of a forked child, and must never be signalled"
        );
        Self(child)
    }

    /// Waits for a child that ends by itself, and returns its wait status.
    ///
    /// It is not killed, and nothing signals it afterwards: once reaped, its
    /// pid may be another process's.
    pub(crate) fn wait(self) -> libc::c_int {
        let pid = self.0;
        std::mem::forget(self);
        let mut status = 0;
        loop {
            // SAFETY: our own unreaped child, and `status` is ours to write.
            if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
                return status;
            }
            let e = std::io::Error::last_os_error();
            assert_eq!(
                e.kind(),
                std::io::ErrorKind::Interrupted,
                "waitpid({pid}): {e}"
            );
        }
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        // SAFETY: our own unreaped child, by a pid `new` checked names one
        // process.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            libc::waitpid(self.0, std::ptr::null_mut(), 0);
        }
    }
}

/// The first thing a forked child does: arrange its own end, and close
/// every descriptor but `kept` (all of them when `kept` is -1, which no
/// descriptor is).
///
/// The alarm ends it if the test process is killed before it can reap it:
/// a child that pauses would otherwise stay for ever. And it holds a copy
/// of every descriptor the suite's other threads had open; those are
/// closed, so no other test waits on this child for a pipe's end.
///
/// # Safety
///
/// Only in a child this test forked, which owns none of the descriptors it
/// closes. It makes only async-signal-safe calls (alarm, close).
pub(crate) unsafe fn keep_only(kept: RawFd) {
    // SAFETY: the forked child, as the caller promises.
    unsafe {
        libc::alarm(60);
        for fd in 0..4096 {
            if fd != kept {
                libc::close(fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Reap;

    /// `kill(0)` signals the test's own process group and `kill(-1)` every
    /// process the user may signal, so no guard is built for either, nor
    /// for any other pid that is not one process's.
    #[test]
    fn a_pid_that_is_not_one_process_gets_no_guard() {
        for pid in [0, -1, -4242, libc::pid_t::MIN] {
            // Forgotten, never dropped: were a guard built for this pid, the
            // test must not be what sends its signal.
            let built = std::panic::catch_unwind(|| std::mem::forget(Reap::new(pid)));
            assert!(built.is_err(), "a guard was built for pid {pid}");
        }
    }
}
