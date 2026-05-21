//! Unix PTY wrapper.
//!
//! This module owns the small but tricky pieces: allocating a pseudo-terminal,
//! forking, setting the slave as the child's controlling terminal, putting
//! the parent's stdin into raw mode (with a guaranteed restore), and pumping
//! bytes between the user's terminal and the PTY master through the
//! [`crate::rewriter::PathRewriter`].
//!
//! The module is intentionally tiny so it can be audited by eye. Anything
//! that can be tested without spawning a real PTY lives in [`crate::rewriter`]
//! instead and is covered by exhaustive unit tests there.

#![cfg(unix)]

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use libc::{c_int, winsize};
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{execvp, fork, ForkResult, Pid};

use crate::rewriter::PathRewriter;

// --- ioctl helpers ---------------------------------------------------------
//
// `nix` 0.29 does not re-export TIOCSWINSZ / TIOCGWINSZ / TIOCSCTTY through
// safe wrappers, so we go through libc directly. Each helper is a one-liner
// with the same error-translation pattern.

/// Set the window size of the terminal referenced by `fd`.
///
/// # Safety
///
/// `fd` must be a valid, open file descriptor referring to a terminal.
unsafe fn ioctl_set_winsize(fd: RawFd, ws: &winsize) -> io::Result<()> {
    let r = libc::ioctl(fd, libc::TIOCSWINSZ as _, ws as *const winsize);
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Read the window size of the terminal referenced by `fd`.
///
/// # Safety
///
/// `fd` must be a valid, open file descriptor referring to a terminal.
unsafe fn ioctl_get_winsize(fd: RawFd) -> io::Result<winsize> {
    let mut ws = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let r = libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws as *mut winsize);
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ws)
    }
}

/// Make `fd` the controlling terminal of the calling session.
///
/// # Safety
///
/// Must be called only in a freshly-forked child that has already called
/// `setsid()`, with `fd` being a valid slave-side PTY descriptor.
unsafe fn ioctl_set_ctty(fd: RawFd) -> io::Result<()> {
    let r = libc::ioctl(fd, libc::TIOCSCTTY as _, 0 as c_int);
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// --- raw mode guard --------------------------------------------------------

/// RAII guard that puts a tty into raw mode and restores the original
/// settings on drop. This is the single source of truth for terminal state
/// on the parent side: as long as the guard exists, the user's terminal is
/// in raw mode; once it is dropped (including on panic), the original
/// settings come back.
struct RawModeGuard {
    fd: RawFd,
    original: Termios,
}

impl RawModeGuard {
    fn install(fd: RawFd) -> io::Result<Self> {
        // SAFETY: caller promises `fd` is a valid open tty for the lifetime
        // of this guard.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let original = tcgetattr(borrowed)?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(borrowed, SetArg::TCSANOW, &raw)?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort restore. If this fails, the user can always type
        // `reset` to recover. We deliberately swallow the error rather than
        // panicking in a Drop.
        // SAFETY: `self.fd` was valid when the guard was installed and is
        // still owned by the same tty in normal program flow.
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = tcsetattr(borrowed, SetArg::TCSANOW, &self.original);
    }
}

// --- public entry point ----------------------------------------------------

/// Spawn `command` (with `args`) inside a fresh PTY and stream its output
/// through the [`PathRewriter`] to real stdout. Returns the child's exit
/// code (or `128 + signal_number` for a signalled exit).
pub fn run(command: &str, args: &[String]) -> io::Result<i32> {
    // 1. Determine the PTY winsize. If stdin is a real tty, mirror its
    //    current dimensions; otherwise fall back to a 24x80 default so
    //    the child still sees a sane terminal.
    let initial_ws = current_winsize().unwrap_or(winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    });
    let nix_ws: Winsize = initial_ws;

    // 2. Allocate the master/slave pair.
    let OpenptyResult { master, slave } = openpty(Some(&nix_ws), None)?;

    // 3. Put stdin into raw mode if it's a tty. The guard lives for the
    //    rest of the function; on drop it restores the original termios.
    //    We must install this AFTER openpty (it must not touch the new
    //    PTY's termios) but BEFORE fork (so the child does not see raw
    //    mode either - we only want raw mode on the *outer* tty so we can
    //    forward keystrokes verbatim).
    let _raw_guard = if stdin_is_tty() {
        Some(RawModeGuard::install(libc::STDIN_FILENO)?)
    } else {
        None
    };

    // 4. Fork.
    // SAFETY: we have not spawned any threads yet, so the child is allowed
    // to run non-async-signal-safe code until it calls execvp().
    match unsafe { fork() }? {
        // `child_after_fork` returns `!`, which coerces to `io::Result<i32>`
        // and lets the match expression type-check.
        ForkResult::Child => child_after_fork(master, slave, command, args),
        ForkResult::Parent { child } => parent_after_fork(master, slave, child),
    }
}
}

/// Code path that runs in the freshly forked child. Does not return.
fn child_after_fork(
    master: OwnedFd,
    slave: OwnedFd,
    command: &str,
    args: &[String],
) -> ! {
    // We don't need the master side in the child.
    drop(master);

    // SAFETY: setsid() is async-signal-safe and side-effect-free aside from
    // changing this process's session/pgid.
    if unsafe { libc::setsid() } < 0 {
        child_die("setsid");
    }

    let sfd = slave.as_raw_fd();

    // Acquire the slave as our controlling terminal so signals from the
    // terminal driver (e.g. SIGINT on Ctrl+C, SIGWINCH on resize) reach us.
    // SAFETY: we just called setsid() and `sfd` is a valid slave PTY fd.
    if unsafe { ioctl_set_ctty(sfd) }.is_err() {
        child_die("ioctl(TIOCSCTTY)");
    }

    // Wire the slave to stdin/stdout/stderr.
    // SAFETY: dup2 is async-signal-safe.
    unsafe {
        if libc::dup2(sfd, libc::STDIN_FILENO) < 0
            || libc::dup2(sfd, libc::STDOUT_FILENO) < 0
            || libc::dup2(sfd, libc::STDERR_FILENO) < 0
        {
            child_die("dup2");
        }
    }
    // Close the original slave fd if it isn't one of 0/1/2 already. We
    // mem::forget when it *is* 0/1/2 because dropping it would close one
    // of the descriptors we just installed.
    if sfd > libc::STDERR_FILENO {
        drop(slave);
    } else {
        std::mem::forget(slave);
    }

    // Build argv. Anything malformed (a NUL in the args) terminates the
    // child cleanly with status 127; the parent will surface that.
    let argv0 = match CString::new(command) {
        Ok(s) => s,
        Err(_) => child_die("invalid command name"),
    };
    let mut owned_argv: Vec<CString> = Vec::with_capacity(args.len() + 1);
    owned_argv.push(argv0.clone());
    for a in args {
        match CString::new(a.as_str()) {
            Ok(s) => owned_argv.push(s),
            Err(_) => child_die("invalid argument"),
        }
    }
    let argv_refs: Vec<&CStr> = owned_argv.iter().map(|s| s.as_c_str()).collect();

    // execvp returns only on failure.
    let _ = execvp(&argv0, &argv_refs);

    // SAFETY: write/_exit are async-signal-safe.
    unsafe {
        let msg = b"nix-pretty: failed to exec shell\n";
        libc::write(libc::STDERR_FILENO, msg.as_ptr() as *const _, msg.len());
        libc::_exit(127);
    }
}

/// Print a short reason to stderr and `_exit(127)`. Used in the child
/// between fork() and exec() where async-signal-safety matters.
fn child_die(why: &str) -> ! {
    // SAFETY: write/_exit are async-signal-safe.
    unsafe {
        let prefix = b"nix-pretty: ";
        libc::write(libc::STDERR_FILENO, prefix.as_ptr() as *const _, prefix.len());
        libc::write(libc::STDERR_FILENO, why.as_ptr() as *const _, why.len());
        libc::write(libc::STDERR_FILENO, b"\n".as_ptr() as *const _, 1);
        libc::_exit(127);
    }
}

/// Code path that runs in the parent after fork. Owns the event loop.
fn parent_after_fork(
    master: OwnedFd,
    slave: OwnedFd,
    child: Pid,
) -> io::Result<i32> {
    // The parent never needs the slave fd; closing it lets the kernel
    // surface EIO to us when the child closes its end.
    drop(slave);

    let master_fd: RawFd = master.as_raw_fd();

    // Background: forward SIGWINCH from the outer tty to the PTY master.
    // We use an Arc<AtomicBool> kill switch so we can stop the thread when
    // the main loop ends, although in practice the thread is allowed to
    // outlive us; std::process::exit() in main() will tear it down.
    let stop_winch = Arc::new(AtomicBool::new(false));
    spawn_winch_thread(master_fd, Arc::clone(&stop_winch))?;

    // Background: forward stdin bytes verbatim to the PTY master.
    spawn_stdin_forwarder(master_fd);

    // Foreground: pump PTY master -> rewriter -> stdout.
    let exit_code = pump_master_to_stdout(master_fd, child)?;

    // Signal the SIGWINCH thread to exit on its next wakeup. It will not
    // do so until the next SIGWINCH arrives, but at process exit it's torn
    // down anyway.
    stop_winch.store(true, Ordering::SeqCst);

    // Keep `master` alive until *after* the pump exits so its raw fd
    // remained valid for the whole loop.
    drop(master);

    Ok(exit_code)
}

// --- background helpers ----------------------------------------------------

fn spawn_stdin_forwarder(master_fd: RawFd) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: STDIN_FILENO is valid; `buf` is a valid writable slice.
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                )
            };
            if n == 0 {
                // EOF on stdin (e.g. user closed it). We deliberately do
                // not propagate this to the master - many shells handle
                // their own stdin EOF semantics.
                return;
            }
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            let n = n as usize;
            if write_all_fd(master_fd, &buf[..n]).is_err() {
                return;
            }
        }
    });
}

fn spawn_winch_thread(master_fd: RawFd, stop: Arc<AtomicBool>) -> io::Result<()> {
    use signal_hook::consts::SIGWINCH;
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGWINCH])?;
    thread::spawn(move || {
        for _sig in signals.forever() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(ws) = current_winsize() {
                // SAFETY: master_fd is valid for the lifetime of the process.
                let _ = unsafe { ioctl_set_winsize(master_fd, &ws) };
            }
        }
    });
    Ok(())
}

/// The main pump: read PTY master, rewrite, write stdout, repeat. Returns
/// the child's exit code once master EOFs and the child has been reaped.
fn pump_master_to_stdout(master_fd: RawFd, child: Pid) -> io::Result<i32> {
    let mut rewriter = PathRewriter::new();
    let mut buf = [0u8; 4096];
    let mut out: Vec<u8> = Vec::with_capacity(8192);
    let stdout_fd = libc::STDOUT_FILENO;

    loop {
        // SAFETY: master_fd is a valid descriptor we own; buf is a valid
        // writable slice.
        let n = unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n == 0 {
            break;
        }
        if n < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                // Linux returns EIO when the slave is closed; treat as EOF.
                Some(libc::EIO) => break,
                _ => return Err(err),
            }
        }
        let n = n as usize;
        out.clear();
        rewriter.process(&buf[..n], &mut out);
        write_all_fd(stdout_fd, &out)?;
    }

    // Flush any tail held back by the rewriter.
    out.clear();
    rewriter.flush(&mut out);
    if !out.is_empty() {
        write_all_fd(stdout_fd, &out)?;
    }

    // Reap the child.
    let status = waitpid(child, None)?;
    let code = match status {
        WaitStatus::Exited(_, c) => c,
        WaitStatus::Signaled(_, sig, _) => 128 + (sig as i32),
        _ => 1,
    };
    Ok(code)
}

// --- low-level helpers -----------------------------------------------------

fn write_all_fd(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: fd is a valid open descriptor; buf is a valid slice.
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const _, buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

fn stdin_is_tty() -> bool {
    // SAFETY: STDIN_FILENO is always a valid descriptor number to query.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

fn current_winsize() -> io::Result<winsize> {
    if !stdin_is_tty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "stdin is not a terminal",
        ));
    }
    // SAFETY: stdin_is_tty() just confirmed STDIN_FILENO refers to a tty.
    unsafe { ioctl_get_winsize(libc::STDIN_FILENO) }
}

// --- unit tests ------------------------------------------------------------
//
// The PTY layer is mostly syscall plumbing, so we keep tests narrow:
//   * RawModeGuard restores termios on drop.
//   * winsize round-trips through ioctl_set/get when run on a real tty
//     (skipped when not available, so the test still passes in CI without
//     a tty).
// The interesting behavioural surface is exercised by `crate::rewriter`'s
// tests, which run the same rewriter the pump uses, against arbitrary
// byte streams.

#[cfg(test)]
mod tests {
    use super::*;
    use nix::pty::openpty;

    /// Helper: open a PTY pair just for testing, return the (master, slave) OwnedFds.
    fn fresh_pty() -> (OwnedFd, OwnedFd) {
        let r = openpty(None, None).expect("openpty must succeed in tests");
        (r.master, r.slave)
    }

    #[test]
    fn raw_mode_guard_restores_termios_on_drop() {
        // Use the slave side of a fresh PTY pair as our "tty" so this works
        // even in CI runners with no controlling terminal.
        let (_master, slave) = fresh_pty();
        let fd = slave.as_raw_fd();

        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let original = tcgetattr(borrowed).expect("tcgetattr on slave");

        {
            let _g = RawModeGuard::install(fd).expect("install raw mode");
            let now = tcgetattr(borrowed).expect("tcgetattr after install");
            // After install, termios must differ (cfmakeraw clears flags).
            assert_ne!(now.input_flags, original.input_flags);
        }

        let after = tcgetattr(borrowed).expect("tcgetattr after drop");
        assert_eq!(after.input_flags, original.input_flags);
        assert_eq!(after.output_flags, original.output_flags);
        assert_eq!(after.local_flags, original.local_flags);
    }

    #[test]
    fn winsize_ioctl_round_trips_on_a_pty() {
        let (_master, slave) = fresh_pty();
        let fd = slave.as_raw_fd();

        let want = winsize {
            ws_row: 42,
            ws_col: 137,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            ioctl_set_winsize(fd, &want).expect("set winsize");
            let got = ioctl_get_winsize(fd).expect("get winsize");
            assert_eq!(got.ws_row, want.ws_row);
            assert_eq!(got.ws_col, want.ws_col);
        }
    }

    #[test]
    fn stdin_is_tty_is_callable() {
        // We don't know whether the test runner has a tty on stdin. We
        // only assert that calling the helper does not panic and returns
        // a bool that matches `isatty` directly.
        let direct = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
        assert_eq!(stdin_is_tty(), direct);
    }

    #[test]
    fn write_all_fd_writes_full_buffer() {
        // Use a fresh pipe as a controllable destination.
        let mut fds = [0i32; 2];
        let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(r, 0, "pipe() must succeed");
        let (rfd, wfd) = (fds[0], fds[1]);

        let payload: Vec<u8> = (0u8..200).collect();
        write_all_fd(wfd, &payload).expect("write_all_fd");

        // Close write end so read returns 0 at EOF.
        unsafe { libc::close(wfd) };

        let mut got = vec![0u8; payload.len()];
        let mut filled = 0;
        while filled < got.len() {
            let n = unsafe {
                libc::read(
                    rfd,
                    got.as_mut_ptr().add(filled) as *mut _,
                    got.len() - filled,
                )
            };
            assert!(n > 0, "unexpected read result {}", n);
            filled += n as usize;
        }
        unsafe { libc::close(rfd) };

        assert_eq!(got, payload);
    }
}
