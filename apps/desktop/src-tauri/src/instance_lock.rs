//! An atomic, OS-guaranteed single-instance gate, closing a real race in
//! `tauri-plugin-single-instance` 2.4.2's macOS backend.
//!
//! User-reported, 2026-08-11: six identical Petal Dock icons, all apparently
//! running, on 0.8.5. Root cause, confirmed by reading the pinned dependency
//! source directly (`tauri-plugin-single-instance-2.4.2/src/platform_impl/
//! macos.rs`): that plugin decides "am I the singleton?" via
//! `UnixStream::connect(socket)` -- `NotFound`/`ConnectionRefused` both mean
//! "nobody's listening, I must be first," at which point the process
//! unconditionally `remove_file`s whatever is at that path and binds its own
//! listener there. This check-then-act sequence has NO cross-process
//! synchronization. A burst of near-simultaneous launches (rapid Dock
//! double/triple-click; or a stale socket left behind by a prior crash/
//! force-quit that skipped the plugin's `RunEvent::Exit` cleanup, which makes
//! `connect()` return `ConnectionRefused` just like "nobody's there") can let
//! several processes all observe "no singleton yet" before any of them
//! finishes binding -- each concludes it's the primary, deletes whatever
//! socket a sibling racer may have just created, and proceeds to launch a
//! full independent app. One Dock icon per survivor.
//!
//! Confirmed macOS-only: the plugin's Windows backend uses `CreateMutexW` +
//! `GetLastError() == ERROR_ALREADY_EXISTS`, a real atomic OS primitive, so
//! Windows is not exposed to this race. Confirmed still present in the latest
//! published version (2.4.2) with no relevant upstream fix.
//!
//! The fix: acquire an OS-level `flock(2)` advisory lock BEFORE
//! `tauri::Builder` (and therefore the plugin's racy `setup()`) ever runs.
//! `flock` is a genuinely atomic kernel primitive with no TOCTOU window, and
//! -- unlike the plugin's socket file -- it needs no explicit cleanup code:
//! the OS releases it automatically when the holding file descriptor closes,
//! including on a crash or `SIGKILL`. That auto-release property is also why
//! this is strictly more robust than the plugin's own mechanism, which is
//! partly what feeds the original race (a stale socket left by an unclean
//! exit reads as "nobody's there" to the next launch).
//!
//! `flock` locks are keyed to the OPEN FILE DESCRIPTION, not the process, so
//! two independent `File::open()` calls to the same path -- even from one
//! process -- genuinely contend, exactly like two separate processes would.
//! This module's tests rely on that to exercise real cross-process semantics
//! without needing to spawn subprocesses.
//!
//! The lock path (and the identifier used to notify an existing primary) is
//! caller-supplied rather than hardcoded: `tauri-plugin-single-instance`
//! scopes its own socket per build `identifier`
//! (`test_cockpit/native_peer.rs` documents and depends on exactly this --
//! the SHARE-N2N local dual-instance test builds a second binary with a
//! different `identifier` specifically so it gets its own socket and can run
//! alongside the primary). A fixed, identifier-agnostic lock here would
//! silently break that.
//!
//! Known accepted trade-off, not fixed here: `shutdown.rs`'s
//! `request_restart_for_second_launch_if_quitting()` self-relaunches via
//! `app.request_restart()`, which spawns the replacement process and only
//! then exits the old one. Tauri dispatches the single-instance plugin's
//! `RunEvent::Exit` -- which removes ITS socket file -- as part of that
//! controlled shutdown, strictly BEFORE the old process's fds (and therefore
//! this module's `flock`) are reclaimed at the final `exit(0)`. So there is a
//! narrow window, specific to this self-restart path, where the freshly
//! spawned process can see our lock as `AlreadyRunning` (the old process
//! hasn't exited yet) while the plugin's notify socket is already gone (its
//! cleanup ran first) -- `notify_running_instance` then fails to connect,
//! logs a warning, and the restart silently no-ops (the user just has to
//! relaunch manually). Strictly less bad than the six-rogue-processes bug
//! this module fixes, and out of scope for it: closing this would need this
//! lock's release synchronized with the plugin's own `RunEvent::Exit`
//! handling rather than left to implicit end-of-`run()` drop.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Held for the life of the process. The OS still releases the underlying
/// `flock` on its own if the process is killed, so nothing here is required
/// for crash safety; the explicit `Drop` below only makes an ordinary release
/// immediate rather than dependent on every copy of the descriptor closing.
pub struct InstanceLock {
    file: File,
}

impl Drop for InstanceLock {
    /// Release explicitly rather than relying on the fd close. `flock` locks
    /// belong to the OPEN FILE DESCRIPTION, so `close()` only releases once
    /// the LAST descriptor referencing it is gone -- and any child forked by a
    /// concurrent `Command::spawn` holds an inherited copy between fork and
    /// exec (`O_CLOEXEC` closes it AT exec, not before). `LOCK_UN` releases
    /// the description's lock outright (#868).
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub enum Acquire {
    Acquired(InstanceLock),
    /// Another process already holds this lock.
    AlreadyRunning,
}

/// `$HOME/Library/Application Support/<identifier>/instance.lock`. Mirrors
/// `logging.rs`'s existing convention of reading `$HOME` directly rather than
/// pulling in the `dirs` crate for one lookup, and macOS's real
/// `app_data_dir()` layout (`~/Library/Application Support/<identifier>/`) --
/// unlike `logging.rs`'s log directory, this one MUST be identifier-scoped
/// (see the module doc comment), so it takes the build identifier as an
/// argument rather than hardcoding "Petal".
pub fn lock_path(identifier: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join("Library")
        .join("Application Support")
        .join(identifier)
        .join("instance.lock")
}

/// Try to become the sole holder of `path`. Creates the file (and its parent
/// directories) if needed. Non-blocking: returns immediately either way,
/// never waits for a holder to release.
pub fn acquire(path: &Path) -> io::Result<Acquire> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).write(true).open(path)?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Acquire::Acquired(InstanceLock { file }));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(Acquire::AlreadyRunning),
        _ => Err(err),
    }
}

/// Reimplements the CLIENT half of `tauri-plugin-single-instance` 2.4.2's
/// macOS wire protocol (`platform_impl/macos.rs`'s `notify_singleton` +
/// `socket_path`) so a process that loses OUR `flock` gate can still hand off
/// to the real primary and get its window activated -- exactly like an
/// ordinary second launch does today. The plugin's own `setup()` (which
/// normally makes this call) never runs on this path; that racy code is
/// what's being bypassed, so nothing else will make this call for us.
///
/// Best-effort: if the primary isn't listening yet for any reason, this logs
/// and gives up rather than erroring -- worst case, that one launch's window
/// doesn't auto-activate, which is strictly better than today's rogue
/// process. If the plugin's socket path derivation or wire format ever
/// change, this needs to move in lockstep -- see the exact-pinned version in
/// Cargo.toml.
pub fn notify_running_instance(identifier: &str) {
    // Must match the plugin's own transform exactly (macos.rs's
    // `socket_path`): `identifier.replace(['.', '-'], '_')`.
    let socket_id = identifier.replace(['.', '-'], "_");
    let socket_path = PathBuf::from(format!("/tmp/{socket_id}_si.sock"));
    notify_at_socket(&socket_path);
}

/// Split from `notify_running_instance` purely so a test can point it at a
/// throwaway socket path and assert the exact bytes sent, instead of only
/// testing the identifier-to-path transform in isolation.
fn notify_at_socket(socket_path: &Path) {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    match UnixStream::connect(socket_path) {
        Ok(mut stream) => {
            let cwd = std::env::current_dir().unwrap_or_default();
            let cwd = cwd.to_string_lossy();
            let args = std::env::args().collect::<Vec<_>>().join("\0");
            if let Err(e) = write!(stream, "{cwd}\0\0{args}").and_then(|_| stream.flush()) {
                log::warn!(
                    "instance_lock: connected to the running instance but the handoff write failed ({e}); its window will not auto-activate for this launch"
                );
            }
        }
        Err(e) => {
            log::warn!(
                "instance_lock: lost the startup race but could not notify the running instance ({e}); its window will not auto-activate for this launch"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per call, not merely per test: pid alone is reusable once a
    /// prior run exits, and a stale file left by a killed run must never be
    /// the same inode a later run locks (#868).
    fn temp_lock_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "petal-instance-lock-test-{name}-{}-{:?}-{}-{}",
            std::process::id(),
            std::thread::current().id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ))
    }

    #[test]
    fn acquire_succeeds_when_nothing_else_holds_the_lock() {
        let path = temp_lock_path("fresh");
        let _ = std::fs::remove_file(&path);

        let result = acquire(&path).unwrap();
        assert!(matches!(result, Acquire::Acquired(_)));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn acquire_reports_already_running_while_a_real_holder_is_alive() {
        // Two independent `File::open()`s of the same path -- via two
        // separate `acquire()` calls -- reproduce genuine cross-process
        // contention: flock is keyed to the open file description, not the
        // process, so this is not a same-process shortcut.
        let path = temp_lock_path("contended");
        let _ = std::fs::remove_file(&path);

        let holder = acquire(&path).unwrap();
        let Acquire::Acquired(_lock) = holder else {
            panic!("first acquire must succeed on an uncontended path");
        };

        let second = acquire(&path).unwrap();
        assert!(matches!(second, Acquire::AlreadyRunning));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn acquire_succeeds_again_once_the_holder_is_dropped() {
        // Proves auto-release: dropping the `InstanceLock` simulates the
        // holding process exiting (normally OR via a crash/SIGKILL, since
        // the OS releases flock on fd-close either way).
        let path = temp_lock_path("released");
        let _ = std::fs::remove_file(&path);

        let holder = acquire(&path).unwrap();
        drop(holder);

        let result = acquire(&path).unwrap();
        assert!(matches!(result, Acquire::Acquired(_)));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dropping_the_lock_releases_it_even_while_an_inherited_fd_copy_survives() {
        // A child forked by any concurrent `Command::spawn` transiently holds
        // an inherited copy of our descriptor. `dup` reproduces exactly that
        // state. Without `InstanceLock`'s explicit `LOCK_UN`, closing our own
        // copy does NOT release the flock and the next `acquire` wrongly
        // reports `AlreadyRunning` -- the #868 flake, deterministically.
        let path = temp_lock_path("inherited-fd");
        let _ = std::fs::remove_file(&path);

        let Acquire::Acquired(lock) = acquire(&path).unwrap() else {
            panic!("first acquire must succeed on an uncontended path");
        };
        let inherited = unsafe { libc::dup(lock.file.as_raw_fd()) };
        assert!(inherited >= 0, "dup failed: {}", io::Error::last_os_error());
        drop(lock);

        let result = acquire(&path).unwrap();
        assert!(matches!(result, Acquire::Acquired(_)));

        unsafe { libc::close(inherited) };
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn acquire_creates_missing_parent_directories() {
        let path = temp_lock_path("nested").join("nested").join("instance.lock");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());

        let result = acquire(&path).unwrap();
        assert!(matches!(result, Acquire::Acquired(_)));
        assert!(path.parent().unwrap().is_dir());

        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn acquire_fails_open_when_the_lock_path_is_unusable() {
        // The safety-critical branch actually wired into `run()`: when
        // acquisition errors for a reason OTHER than contention (here: a
        // regular file sits where a parent DIRECTORY needs to exist),
        // `acquire` must surface `Err` rather than silently reporting either
        // `Acquired` or `AlreadyRunning` -- callers fail OPEN on `Err`
        // (proceed without the extra guard) rather than ever blocking a
        // legitimate solo launch.
        let blocker = temp_lock_path("blocker-file");
        let _ = std::fs::remove_file(&blocker);
        std::fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join("instance.lock");

        let result = acquire(&path);
        assert!(result.is_err());

        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn lock_path_is_scoped_by_identifier() {
        // Must differ across identifiers (e.g. `com.petal.app` vs
        // `com.petal.app.testpeer`) -- a fixed path would make the
        // SHARE-N2N test-peer instance see the primary as "already
        // running" and exit instead of starting a second local instance.
        let primary = lock_path("com.petal.app");
        let testpeer = lock_path("com.petal.app.testpeer");
        assert_ne!(primary, testpeer);
        assert!(primary.ends_with("com.petal.app/instance.lock"));
        assert!(testpeer.ends_with("com.petal.app.testpeer/instance.lock"));
    }

    #[test]
    fn notify_socket_id_transform_matches_the_plugin_exactly() {
        // `tauri-plugin-single-instance` 2.4.2's macos.rs `socket_path`:
        // `identifier.replace(['.', '-'], '_')`. Pinned verbatim so a future
        // edit here can't silently drift from what the plugin actually
        // listens on.
        assert_eq!(
            "com.petal.app".replace(['.', '-'], "_"),
            "com_petal_app"
        );
        assert_eq!(
            "com.petal.app.testpeer".replace(['.', '-'], "_"),
            "com_petal_app_testpeer"
        );
    }

    #[test]
    fn notify_sends_the_exact_wire_format_the_plugin_expects() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        // Deliberately `/tmp` directly, NOT `temp_lock_path()`'s
        // `std::env::temp_dir()`: `sockaddr_un.sun_path` has a tight length
        // limit (~104 bytes on macOS), and macOS's real per-process temp dir
        // (`/var/folders/.../T/...`) is long enough on its own to overflow it
        // once combined with a descriptive filename -- confirmed live on CI
        // (`InvalidInput: path must be shorter than SUN_LEN`). This is
        // exactly why the real production code (`notify_running_instance`)
        // puts its socket under a short, fixed `/tmp/...` path rather than
        // the system temp dir. Mirror that here, both to fix the overflow
        // and to test the same path shape production actually uses.
        let socket_path = PathBuf::from(format!("/tmp/petal-notify-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        notify_at_socket(&socket_path);

        let (mut stream, _) = listener.accept().unwrap();
        let mut received = String::new();
        stream.read_to_string(&mut received).unwrap();

        // Must match macos.rs's `notify_singleton` exactly: cwd, then a
        // DOUBLE null separator, then argv joined by single nulls.
        let expected_cwd = std::env::current_dir().unwrap_or_default();
        let expected_cwd = expected_cwd.to_string_lossy();
        let expected_args = std::env::args().collect::<Vec<_>>().join("\0");
        assert_eq!(received, format!("{expected_cwd}\0\0{expected_args}"));

        let _ = std::fs::remove_file(&socket_path);
    }
}
