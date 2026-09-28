//! Child processes that cannot outlive this one, and whose PEAK memory survives their death.
//!
//! Two things std cannot do, so this module does them itself:
//!
//! * **Peak RSS.** `Child::wait` throws away the rusage the kernel already has, and sampling
//!   `/proc/<pid>/status` cannot see a process that has exited — which is exactly what made an
//!   earlier OOM series unreadable from its own tail. `wait4` returns `ru_maxrss` for THIS child,
//!   high-water mark included, and the kernel holds it until the child is reaped, so a process that
//!   died still reports what it reached. `ru_maxrss` is KiB on Linux (bytes on macOS; capture is
//!   Linux-only and the field is named for the unit it carries here).
//! * **Killing the tree.** The JVM is spawned into its own process group (`process_group(0)`, which
//!   std does provide), so one `kill(-pgid)` reaps whatever it spawned rather than leaving orphans
//!   holding port 25565 for the next episode. [`Child::drop`] does that on every path out of
//!   [`crate::capture_episode`].
//!
//! An exit code below zero is the negated terminating signal, so an OOM kill arrives as `-9`. That is indistinguishable from any other SIGKILL, which is why
//! the caller records whether it did the killing itself rather than guessing here.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

/// A spawned child in its own process group, reaped by us so its peak RSS survives.
pub(crate) struct Child {
    pid: i32,
    exit_code: Option<i32>,
    peak_rss_kib: Option<u64>,
    reaped: bool,
}

/// Outcome of reaping one child.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Reaped {
    /// `None` when the child was still running at the deadline (and we then killed it).
    pub(crate) exit_code: Option<i32>,
    pub(crate) peak_rss_kib: Option<u64>,
}

impl Child {
    /// Spawn `command` in a fresh process group. The caller sets up cwd, env and stdio.
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Child> {
        // Safety-relevant only in the POSIX sense: `process_group` is std's own API, and the group
        // id 0 means "make the child its own group leader".
        let child = command.process_group(0).spawn()?;
        Ok(Child {
            pid: child.id() as i32,
            exit_code: None,
            peak_rss_kib: None,
            reaped: false,
        })
    }

    pub(crate) fn pid(&self) -> i32 {
        self.pid
    }

    /// Non-blocking check: `Some(exit_code)` once the child has been reaped.
    pub(crate) fn poll(&mut self) -> Option<i32> {
        if !self.reaped {
            self.reap(false);
        }
        self.exit_code
    }

    /// Wait up to `timeout` for the child to exit, then report its code and peak RSS. A child still
    /// running at the deadline reports `exit_code: None` and is left alone — the caller decides
    /// whether to kill it, because "we killed it" and "the kernel killed it" are different facts
    /// that arrive as the same `-9`.
    pub(crate) fn wait(&mut self, timeout: Duration) -> Reaped {
        let deadline = Instant::now() + timeout;
        while !self.reaped {
            self.reap(false);
            if self.reaped || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Reaped {
            exit_code: self.exit_code,
            peak_rss_kib: self.peak_rss_kib,
        }
    }

    /// SIGKILL the whole process group, then reap (blocking — a killed process exits at once).
    pub(crate) fn kill_group(&mut self) -> Reaped {
        if !self.reaped {
            // Negative pid = the process group led by that pid.
            unsafe { libc::kill(-self.pid, libc::SIGKILL) };
            self.reap(true);
        }
        Reaped {
            exit_code: self.exit_code,
            peak_rss_kib: self.peak_rss_kib,
        }
    }

    fn reap(&mut self, blocking: bool) {
        let mut status: libc::c_int = 0;
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        let options = if blocking { 0 } else { libc::WNOHANG };
        let result = unsafe { libc::wait4(self.pid, &mut status, options, &mut usage) };
        if result == self.pid {
            self.reaped = true;
            self.exit_code = Some(if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                -libc::WTERMSIG(status)
            });
            self.peak_rss_kib = Some(usage.ru_maxrss.max(0) as u64);
        } else if result < 0 {
            // ECHILD: someone else reaped it, or it never existed. Nothing left to learn.
            self.reaped = true;
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.kill_group();
        }
    }
}
