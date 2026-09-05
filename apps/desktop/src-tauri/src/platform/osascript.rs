//! Shared `/usr/bin/osascript` child-process runner.
//!
//! Two call sites need to run an AppleScript with a killable deadline and
//! captured stdout/stderr: `test_cockpit`'s native<->native remote-control
//! drive (`cockpit-privileged`-feature only) and `browser_url`'s shared-
//! browser-window URL extraction (always compiled). This is the one runner
//! -- do not add a second (issue #915).
//!
//! Uses an absolute path (never relies on `PATH`), captures both stdout and
//! stderr (the caller needs stderr to distinguish a `-1743` Apple Events
//! denial from any other failure), and is killable at its deadline: the
//! child is awaited on a dedicated thread and the caller blocks on a channel
//! with `recv_timeout` rather than a polling sleep loop, so a long deadline
//! (browser_url's 60s first attempt) costs nothing but one blocked thread.

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Outcome of one `osascript` invocation.
#[derive(Debug)]
pub enum OsascriptOutcome {
    /// The process exited zero. **Raw** stdout -- only osascript's own
    /// trailing newline is a given, not general whitespace -- because one
    /// caller (`test_cockpit`) compares document content where leading/
    /// trailing whitespace is significant. Callers that want a trimmed
    /// result (`browser_url`) trim it themselves.
    Ok(String),
    /// The deadline elapsed before the process exited; it was killed.
    Timeout,
    /// The process exited non-zero. `stderr` is trimmed, full captured text
    /// (callers that only want the first line should split on their own --
    /// this runner does not assume a caller's log-message shape).
    Failed { status: i32, stderr: String },
    /// The process could not be spawned, or its exit/output could not be
    /// read.
    Spawn(String),
}

/// Run `/usr/bin/osascript -e <line> -e <line> ...` and wait up to `timeout`
/// for it to exit, killing it if the deadline passes.
pub fn run_osascript(lines: &[&str], timeout: Duration) -> OsascriptOutcome {
    let mut command = Command::new("/usr/bin/osascript");
    for line in lines {
        command.arg("-e").arg(line);
    }

    let child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return OsascriptOutcome::Spawn(format!("spawn osascript: {error}")),
    };

    // Captured before `child` moves into the waiter thread below. Sending
    // SIGKILL to this pid on timeout (instead of calling `Child::kill()`,
    // which needs `&mut Child` and so isn't reachable from this thread once
    // `child` has moved) is pid-safe, not merely pid-*probably*-safe: a
    // POSIX pid stays reserved to its zombie process until its parent reaps
    // it via `wait`/`wait_with_output`, and the waiter thread below is the
    // only place that ever calls either for this child, always reporting
    // back over `tx` first. So if `recv_timeout` times out, this process
    // has not been reaped and this pid cannot yet have been reused by the
    // kernel -- the exact same guarantee `Child::kill()` relies on
    // internally (it is implemented as this same `libc::kill(pid, SIGKILL)`
    // call on Unix).
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        // `wait_with_output` reads stdout and stderr concurrently
        // internally before blocking on the actual wait. Reading them
        // sequentially here instead (as an earlier version of this
        // function did) can deadlock: if the child interleaves writes to
        // both streams and the one we read second fills its pipe buffer
        // while we're still blocked reading the first to EOF, the child
        // blocks trying to write to it and never reaches EOF on either.
        let result = child.wait_with_output();
        // The receiver may already have timed out and stopped listening --
        // that's fine, the send just becomes a no-op.
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            let _ = waiter.join();
            // Lossy, not `read_to_string`/`String::from_utf8`: osascript's
            // output is not guaranteed valid UTF-8 (a page title or URL
            // path can carry anything), and a decode failure must not
            // silently become an empty string -- it must still show
            // whatever was legible.
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if output.status.success() {
                OsascriptOutcome::Ok(stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                OsascriptOutcome::Failed {
                    status: output.status.code().unwrap_or(-1),
                    stderr: stderr.trim().to_string(),
                }
            }
        }
        Ok(Err(error)) => {
            let _ = waiter.join();
            OsascriptOutcome::Spawn(format!("wait for osascript: {error}"))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            // The waiter thread's blocking wait will now return; join it so
            // the process is fully reaped before we return.
            let _ = waiter.join();
            OsascriptOutcome::Timeout
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            OsascriptOutcome::Spawn("osascript waiter thread ended without a result".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_exit_returns_raw_stdout_with_only_the_trailing_newline_added() {
        match run_osascript(&["return \"  hello  \""], Duration::from_secs(5)) {
            // osascript prints the result then appends exactly one newline;
            // the interior/leading/trailing spaces from the script itself
            // must survive untouched (test_cockpit relies on this).
            OsascriptOutcome::Ok(stdout) => assert_eq!(stdout, "  hello  \n"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn nonzero_exit_captures_stderr() {
        match run_osascript(&["error \"boom\""], Duration::from_secs(5)) {
            OsascriptOutcome::Failed { status, stderr } => {
                assert_ne!(status, 0);
                assert!(!stderr.is_empty(), "expected non-empty stderr");
                assert!(stderr.contains("boom"), "stderr was: {stderr}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn large_concurrent_stdout_and_stderr_do_not_deadlock() {
        // AppleScript's `log` writes to stderr. 800 repeats of a ~140-byte
        // line produces >110KB of stderr (measured: exceeds macOS's ~64KB
        // pipe buffer) before the script ever returns its 5-byte stdout
        // result. An earlier version of this runner read stdout to EOF
        // before touching stderr: the child would block writing to a full
        // stderr pipe, our stdout read would block waiting for output that
        // was never coming (the child can't reach `return` until its stderr
        // backlog drains), and the two threads would deadlock until this
        // function's own timeout forcibly killed the (otherwise-healthy,
        // sub-100ms) script. `wait_with_output` reads both concurrently, so
        // this must finish almost immediately, not time out.
        let script = "repeat 800 times\n  log \"filler filler filler filler filler filler \
                       filler filler filler filler filler filler filler filler filler filler \
                       filler filler filler filler\"\nend repeat\nreturn \"done\"";
        let start = std::time::Instant::now();
        match run_osascript(&[script], Duration::from_secs(15)) {
            OsascriptOutcome::Ok(stdout) => assert_eq!(stdout.trim(), "done"),
            other => panic!(
                "expected Ok(\"done\"), got {other:?} after {:?} -- likely a stdout/stderr \
                 pipe deadlock",
                start.elapsed()
            ),
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the script itself completes in well under a second; anything near the 15s \
             deadline means the streams deadlocked and this only returned because the timeout \
             eventually killed it, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn deadline_kills_and_reports_timeout() {
        let start = std::time::Instant::now();
        let outcome = run_osascript(&["delay 3"], Duration::from_millis(200));
        assert!(
            matches!(outcome, OsascriptOutcome::Timeout),
            "expected Timeout, got {outcome:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the deadline should have killed the child well under the script's own 3s delay, took {:?}",
            start.elapsed()
        );
    }
}
