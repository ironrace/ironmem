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
pub(crate) struct Reap(pub(crate) libc::pid_t);

impl Drop for Reap {
    fn drop(&mut self) {
        // SAFETY: our own unreaped child.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            libc::waitpid(self.0, std::ptr::null_mut(), 0);
        }
    }
}

impl Reap {
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

/// The first thing a forked child does: arrange its own end, and close
/// every descriptor but `kept`.
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
