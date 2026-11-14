//! Forked children for the crate's unit tests, and only for them.
//!
//! A few tests need a real child that never execs: one that holds a
//! descriptor it inherited, or changes an attribute of itself. Such a child
//! is a copy of a multi-threaded test binary, so it makes only
//! async-signal-safe calls and plain system calls, which take no lock in the
//! process, and the test that forked it must not leave it behind, whichever
//! way the test ends.

use std::os::unix::io::{AsRawFd, BorrowedFd, RawFd};
use std::time::{Duration, Instant};

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
    ///
    /// # Safety
    ///
    /// `child` is a child of this process that has not been waited for, and
    /// nothing but this guard signals it or waits for it. The guard kills
    /// and reaps whatever process has that pid when it is dropped, and the
    /// pid of a child already reaped may by then be another process's.
    pub(crate) unsafe fn new(child: libc::pid_t) -> Self {
        assert!(
            child > 0,
            "{child} is not the pid of a forked child, and must never be signalled"
        );
        Self(child)
    }

    /// Waits for a child that ends by itself, and returns its wait status.
    ///
    /// The wait has no bound of its own: the child must have called
    /// [`keep_only`], whose alarm is what ends a child that would otherwise
    /// never end.
    ///
    /// It is not killed, and nothing signals it afterwards: once reaped, its
    /// pid may be another process's.
    pub(crate) fn wait(self) -> libc::c_int {
        let pid = self.0;
        std::mem::forget(self);
        match wait_for(pid, 0) {
            Ok(Some(status)) => status,
            Ok(None) => panic!("waitpid({pid}) returned without a child"),
            Err(e) => panic!("waitpid({pid}): {e}"),
        }
    }

    /// Polls, without ever blocking in a wait, for a child that ends by
    /// itself, and returns its wait status: `None` if it is still running at
    /// `deadline`, when it is killed and reaped as a dropped guard's is.
    pub(crate) fn wait_until(self, deadline: Instant) -> Option<libc::c_int> {
        loop {
            match wait_for(self.0, libc::WNOHANG) {
                Ok(Some(status)) => {
                    // Reaped, so its pid is no longer this guard's to signal.
                    std::mem::forget(self);
                    return Some(status);
                }
                Ok(None) => {}
                Err(e) => {
                    let pid = self.0;
                    std::mem::forget(self);
                    panic!("waitpid({pid}): {e}");
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        // SAFETY: our own unreaped child, as `new`'s caller promised, by a
        // pid `new` checked names one process.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
        // Nothing to do about a failure here: the child was sent its kill.
        let _ = wait_for(self.0, 0);
    }
}

/// `waitpid` on `pid`, called again whenever a signal interrupts it: the
/// wait status of the child it reaped, or `None` when `options` has
/// `WNOHANG` and the child is still running.
fn wait_for(pid: libc::pid_t, options: libc::c_int) -> std::io::Result<Option<libc::c_int>> {
    let mut status = 0;
    loop {
        // SAFETY: `status` is ours to write, and `waitpid` only fails on a
        // pid that is not an unreaped child of this process.
        let reaped = unsafe { libc::waitpid(pid, &mut status, options) };
        if reaped == pid {
            return Ok(Some(status));
        }
        if reaped == 0 {
            return Ok(None);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// One more than the highest descriptor [`keep_only`] closes. A descriptor
/// at or above it stays open. The kernel gives a new descriptor the lowest
/// free number, so there is one only in a process that holds at least this
/// many at once, and the test binary is assumed never to.
const CLOSED_BELOW: RawFd = 4096;

/// The first thing a forked child does: arrange its own end, and close
/// every descriptor below [`CLOSED_BELOW`] but `kept` (all of them when
/// there is none to keep).
///
/// The alarm ends the child `seconds` after the call, whatever it is doing:
/// that is the longest it lives. A child that pauses would otherwise stay
/// for ever if the test process were killed before reaping it, and one that
/// never ended would hold [`Reap::wait`] for ever. And the child holds a
/// copy of every descriptor the suite's other threads had open; those are
/// closed, so no other test waits on this child for a pipe's end.
///
/// # Safety
///
/// Only in a child this test forked, which owns none of the descriptors it
/// closes. It makes only async-signal-safe calls (alarm, close).
pub(crate) unsafe fn keep_only(kept: Option<BorrowedFd<'_>>, seconds: libc::c_uint) {
    let kept = kept.map(|fd| fd.as_raw_fd());
    // SAFETY: the forked child, as the caller promises.
    unsafe {
        libc::alarm(seconds);
        for fd in 0..CLOSED_BELOW {
            if Some(fd) != kept {
                libc::close(fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::unix::io::{AsFd, AsRawFd};
    use std::time::{Duration, Instant};

    use super::{keep_only, Reap};

    /// `kill(0)` signals the test's own process group and `kill(-1)` every
    /// process the user may signal, so no guard is built for either, nor
    /// for any other pid that is not one process's.
    #[test]
    fn a_pid_that_is_not_one_process_gets_no_guard() {
        for pid in [0, -1, -4242, libc::pid_t::MIN] {
            // Forgotten, never dropped: were a guard built for this pid, the
            // test must not be what sends its signal.
            // SAFETY: a guard that is never dropped signals and reaps
            // nothing, whatever pid it was built for.
            let built = std::panic::catch_unwind(|| std::mem::forget(unsafe { Reap::new(pid) }));
            assert!(built.is_err(), "a guard was built for pid {pid}");
        }
    }

    /// The exit status of a forked child that calls `keep_only` and then
    /// reports which of two descriptors it still has: 1 for `a`, 2 for `b`.
    fn open_after_keep_only(a: &File, b: &File, keep_a: bool) -> libc::c_int {
        // SAFETY: the child makes only async-signal-safe calls (alarm,
        // close, fcntl, _exit).
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
        if child == 0 {
            // SAFETY: the forked child, as above.
            unsafe {
                keep_only(if keep_a { Some(a.as_fd()) } else { None }, 60);
                let has_a = libc::fcntl(a.as_raw_fd(), libc::F_GETFD) != -1;
                let has_b = libc::fcntl(b.as_raw_fd(), libc::F_GETFD) != -1;
                libc::_exit(libc::c_int::from(has_a) | (libc::c_int::from(has_b) << 1));
            }
        }
        // SAFETY: `child` is the pid `fork` just returned to this process,
        // and nothing else signals it or waits for it.
        let status = unsafe { Reap::new(child) }.wait();
        assert!(
            libc::WIFEXITED(status),
            "the child did not exit: wait status {status}"
        );
        libc::WEXITSTATUS(status)
    }

    /// The fork tests rest on both halves: a child that lost the descriptor
    /// it kept would hold nothing, and one that kept the others would hold
    /// the suite's pipes.
    #[test]
    fn keep_only_closes_every_descriptor_but_the_one_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = File::create(dir.path().join("a")).unwrap();
        let b = File::create(dir.path().join("b")).unwrap();
        assert_eq!(
            open_after_keep_only(&a, &b, true),
            1,
            "the child keeps `a` (1) and not `b` (2)"
        );
        assert_eq!(
            open_after_keep_only(&a, &b, false),
            0,
            "the child keeps neither `a` (1) nor `b` (2)"
        );
    }

    /// The alarm is what ends a child no test reaps. Here a child that only
    /// pauses is ended by a 1 s alarm.
    #[test]
    fn keep_only_ends_a_child_that_outlives_its_seconds() {
        // SAFETY: the child makes only async-signal-safe calls (alarm,
        // close, pause) until its alarm ends it.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
        if child == 0 {
            // SAFETY: the forked child, as above.
            unsafe {
                keep_only(None, 1);
                loop {
                    libc::pause();
                }
            }
        }
        // SAFETY: `child` is the pid `fork` just returned to this process,
        // and nothing else signals it or waits for it.
        let reap = unsafe { Reap::new(child) };
        let status = reap
            .wait_until(Instant::now() + Duration::from_secs(30))
            .expect("the child was still running 30 s after its 1 s alarm was set");
        assert!(
            libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGALRM,
            "the child was not ended by its alarm: wait status {status}"
        );
    }
}
