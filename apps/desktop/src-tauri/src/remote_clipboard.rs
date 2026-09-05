//! Native plain-text clipboard access and bounded remote-clipboard state.
//!
//! Clipboard contents are deliberately kept out of the remote-control packet
//! model. This module owns the small platform seam needed by both the existing
//! native text-shortcut actuator and the cross-machine clipboard extension.
//! Rich formats and recognized file-transfer formats are never serialized.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use crate::sync_ext::MutexExt;

pub(crate) const REMOTE_CLIPBOARD_TEXT_TOPIC: &str = "petal.remote-control.clipboard-text";
pub(crate) const REMOTE_CLIPBOARD_TEXT_MIME: &str = "text/plain; charset=utf-8";
pub(crate) const MAX_REMOTE_CLIPBOARD_TEXT_BYTES: usize = 1_048_576;
pub(crate) const REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH: usize = 32;
pub(crate) const REMOTE_CLIPBOARD_OPERATION_TTL: Duration = Duration::from_secs(10);
pub(crate) const REMOTE_COPY_OBSERVATION_DEADLINE: Duration = Duration::from_secs(2);
const CLIPBOARD_OPEN_ATTEMPTS: usize = 10;
const MAX_RECENT_PASTE_OPERATIONS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardError {
    Unavailable,
    FileTransfer,
    NoText,
    Empty,
    TooLarge,
    InvalidUtf8,
    Nul,
    WriteFailed,
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unavailable => "clipboard unavailable",
            Self::FileTransfer => "clipboard contains a file transfer",
            Self::NoText => "clipboard has no plain text",
            Self::Empty => "clipboard text is empty",
            Self::TooLarge => "clipboard text exceeds the remote limit",
            Self::InvalidUtf8 => "clipboard text is not valid UTF-8",
            Self::Nul => "clipboard text contains NUL",
            Self::WriteFailed => "clipboard write failed",
        };
        f.write_str(message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipboardContents {
    pub(crate) sequence: u64,
    /// True when the source advertises an actual file-list/file-promise
    /// flavor. This is checked before reading any companion path text.
    pub(crate) has_file_transfer: bool,
    /// The source's plain-text flavor, if one is available. It is omitted for
    /// a file-bearing source even when a path-looking companion string exists.
    pub(crate) text: Option<Vec<u8>>,
}

pub(crate) trait ClipboardBackend: Send + Sync {
    /// Read the plain string flavor for the native AX shortcut path. This is
    /// intentionally not the transfer read: a native Paste may use the
    /// ordinary text flavor even when richer formats are also present.
    fn read_text(&self) -> Result<Option<String>, ClipboardError>;
    fn write_text(&self, text: &str) -> Result<(), ClipboardError>;
    fn sequence(&self) -> Result<u64, ClipboardError>;
    fn contents(&self) -> Result<ClipboardContents, ClipboardError>;

    fn read_transfer_text(&self) -> Result<Vec<u8>, ClipboardError> {
        validate_clipboard_contents(&self.contents()?)
    }
}

pub(crate) fn validate_remote_text(bytes: &[u8]) -> Result<&str, ClipboardError> {
    if bytes.is_empty() {
        return Err(ClipboardError::Empty);
    }
    if bytes.len() > MAX_REMOTE_CLIPBOARD_TEXT_BYTES {
        return Err(ClipboardError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ClipboardError::InvalidUtf8)?;
    if text.contains('\0') {
        return Err(ClipboardError::Nul);
    }
    Ok(text)
}

pub(crate) fn validate_clipboard_contents(
    contents: &ClipboardContents,
) -> Result<Vec<u8>, ClipboardError> {
    if contents.has_file_transfer {
        return Err(ClipboardError::FileTransfer);
    }
    let bytes = contents.text.as_deref().ok_or(ClipboardError::NoText)?;
    validate_remote_text(bytes)?;
    Ok(bytes.to_vec())
}

pub(crate) fn operation_id_is_valid(value: &str) -> bool {
    value.len() == REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn new_operation_id() -> Result<String, ClipboardError> {
    let mut bytes = [0u8; REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH / 2];
    getrandom::fill(&mut bytes).map_err(|_| ClipboardError::Unavailable)?;
    let mut id = String::with_capacity(REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut id, "{byte:02x}").expect("writing an operation id cannot fail");
    }
    Ok(id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCopy {
    pub(crate) operation_id: String,
    pub(crate) owner_identity: String,
    pub(crate) window_id: u32,
    pub(crate) grant_token: String,
    pub(crate) local_clipboard_sequence: u64,
    pub(crate) expires_at: Instant,
}

#[derive(Debug, Default)]
struct ClipboardOperationState {
    pending_copy: Option<PendingCopy>,
    recent_copy_operations: HashMap<(String, String), Instant>,
    recent_paste_operations: HashMap<(String, String), Instant>,
}

fn operation_state() -> &'static Mutex<ClipboardOperationState> {
    static STATE: OnceLock<Mutex<ClipboardOperationState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(ClipboardOperationState::default()))
}

pub(crate) fn replace_pending_copy(pending: PendingCopy) {
    operation_state().lock_unpoisoned().pending_copy = Some(pending);
}

pub(crate) fn pending_copy(now: Instant) -> Option<PendingCopy> {
    let mut state = operation_state().lock_unpoisoned();
    if state
        .pending_copy
        .as_ref()
        .is_some_and(|pending| pending.expires_at <= now)
    {
        state.pending_copy = None;
    }
    state.pending_copy.clone()
}

pub(crate) fn take_pending_copy_if(
    now: Instant,
    predicate: impl FnOnce(&PendingCopy) -> bool,
) -> Option<PendingCopy> {
    let mut state = operation_state().lock_unpoisoned();
    if state
        .pending_copy
        .as_ref()
        .is_some_and(|pending| pending.expires_at <= now)
    {
        state.pending_copy = None;
        return None;
    }
    if state.pending_copy.as_ref().is_some_and(predicate) {
        state.pending_copy.take()
    } else {
        None
    }
}

pub(crate) fn clear_pending_copy() {
    operation_state().lock_unpoisoned().pending_copy = None;
}

pub(crate) fn clear_pending_copy_if_operation(operation_id: &str) {
    let mut state = operation_state().lock_unpoisoned();
    if state
        .pending_copy
        .as_ref()
        .is_some_and(|pending| pending.operation_id == operation_id)
    {
        state.pending_copy = None;
    }
}

pub(crate) fn clear_pending_copy_for(window_id: u32, owner_identity: Option<&str>) {
    let mut state = operation_state().lock_unpoisoned();
    if state.pending_copy.as_ref().is_some_and(|pending| {
        pending.window_id == window_id
            && owner_identity.is_none_or(|owner| pending.owner_identity == owner)
    }) {
        state.pending_copy = None;
    }
}

pub(crate) fn clear_pending_copy_for_owner(owner_identity: &str) {
    let mut state = operation_state().lock_unpoisoned();
    if state
        .pending_copy
        .as_ref()
        .is_some_and(|pending| pending.owner_identity == owner_identity)
    {
        state.pending_copy = None;
    }
}

fn reserve_recent_operation(
    operations: &mut HashMap<(String, String), Instant>,
    sender_identity: &str,
    operation_id: &str,
    now: Instant,
) -> bool {
    operations.retain(|_, seen_at| {
        now.saturating_duration_since(*seen_at) <= REMOTE_CLIPBOARD_OPERATION_TTL
    });
    let key = (sender_identity.to_string(), operation_id.to_string());
    if operations.contains_key(&key) {
        return false;
    }
    if operations.len() >= MAX_RECENT_PASTE_OPERATIONS {
        if let Some(oldest) = operations
            .iter()
            .min_by_key(|(_, seen_at)| *seen_at)
            .map(|(key, _)| key.clone())
        {
            operations.remove(&oldest);
        }
    }
    operations.insert(key, now);
    true
}

/// Reserve a Copy operation after request authentication and immediately
/// before native clipboard/target side effects. Reliable transport retries
/// must not invoke the target twice.
pub(crate) fn reserve_copy_operation(
    sender_identity: &str,
    operation_id: &str,
    now: Instant,
) -> bool {
    reserve_recent_operation(
        &mut operation_state().lock_unpoisoned().recent_copy_operations,
        sender_identity,
        operation_id,
        now,
    )
}

/// Reserve a Paste operation after all stream/header checks and immediately
/// before the clipboard side effect.
pub(crate) fn reserve_paste_operation(
    sender_identity: &str,
    operation_id: &str,
    now: Instant,
) -> bool {
    reserve_recent_operation(
        &mut operation_state().lock_unpoisoned().recent_paste_operations,
        sender_identity,
        operation_id,
        now,
    )
}

pub(crate) fn clear_copy_operations() {
    operation_state()
        .lock_unpoisoned()
        .recent_copy_operations
        .clear();
}

pub(crate) fn clear_copy_operations_for_sender(sender_identity: &str) {
    operation_state()
        .lock_unpoisoned()
        .recent_copy_operations
        .retain(|(sender, _), _| sender != sender_identity);
}

pub(crate) fn clear_paste_operations() {
    operation_state()
        .lock_unpoisoned()
        .recent_paste_operations
        .clear();
}

pub(crate) fn clear_paste_operations_for_sender(sender_identity: &str) {
    operation_state()
        .lock_unpoisoned()
        .recent_paste_operations
        .retain(|(sender, _), _| sender != sender_identity);
}

pub(crate) fn prune_paste_operations(now: Instant) {
    let mut state = operation_state().lock_unpoisoned();
    state
        .recent_copy_operations
        .retain(|_, seen_at| now.saturating_duration_since(*seen_at) <= REMOTE_CLIPBOARD_OPERATION_TTL);
    state
        .recent_paste_operations
        .retain(|_, seen_at| now.saturating_duration_since(*seen_at) <= REMOTE_CLIPBOARD_OPERATION_TTL);
}

pub(crate) fn try_clipboard_operation_lock() -> Option<MutexGuard<'static, ()>> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[cfg(target_os = "macos")]
struct SystemClipboardBackend;

#[cfg(target_os = "macos")]
impl ClipboardBackend for SystemClipboardBackend {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
        Ok(unsafe {
            pasteboard
                .stringForType(objc2_app_kit::NSPasteboardTypeString)
                .map(|value| value.to_string())
        })
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
        let value = objc2_foundation::NSString::from_str(text);
        unsafe {
            pasteboard.clearContents();
            if pasteboard.setString_forType(&value, objc2_app_kit::NSPasteboardTypeString) {
                Ok(())
            } else {
                Err(ClipboardError::WriteFailed)
            }
        }
    }

    fn sequence(&self) -> Result<u64, ClipboardError> {
        Ok(objc2_app_kit::NSPasteboard::generalPasteboard()
            .changeCount()
            .max(0) as u64)
    }

    fn contents(&self) -> Result<ClipboardContents, ClipboardError> {
        let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
        let sequence = pasteboard.changeCount().max(0) as u64;
        let has_file_transfer = pasteboard
            .types()
            .map(|types| {
                types.to_vec().iter().any(|kind| {
                    matches!(
                        kind.to_string().as_str(),
                        "public.file-url"
                            | "public.file-path"
                            | "NSFilenamesPboardType"
                            | "NSFilesPromisePboardType"
                            | "com.apple.pasteboard.promised-file-url"
                            | "com.apple.pasteboard.promised-file-content"
                            | "com.apple.pasteboard.promised-file-name"
                            | "com.apple.NSFilePromise"
                    )
                })
            })
            .unwrap_or(false);
        let text = if has_file_transfer {
            None
        } else {
            unsafe {
                pasteboard
                    .stringForType(objc2_app_kit::NSPasteboardTypeString)
                    .map(|value| value.to_string().into_bytes())
            }
        };
        Ok(ClipboardContents {
            sequence,
            has_file_transfer,
            text,
        })
    }
}

#[cfg(target_os = "windows")]
struct SystemClipboardBackend;

#[cfg(target_os = "windows")]
impl ClipboardBackend for SystemClipboardBackend {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        use clipboard_win::Getter;
        let _clipboard = clipboard_win::Clipboard::new_attempts(CLIPBOARD_OPEN_ATTEMPTS)
            .map_err(|_| ClipboardError::Unavailable)?;
        let mut text = String::new();
        match clipboard_win::formats::Unicode.read_clipboard(&mut text) {
            Ok(_) => Ok(Some(text)),
            Err(_) => Ok(None),
        }
    }

    fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
        use clipboard_win::Setter;
        let _clipboard = clipboard_win::Clipboard::new_attempts(CLIPBOARD_OPEN_ATTEMPTS)
            .map_err(|_| ClipboardError::Unavailable)?;
        clipboard_win::formats::Unicode
            .write_clipboard(&text)
            .map_err(|_| ClipboardError::WriteFailed)
    }

    fn sequence(&self) -> Result<u64, ClipboardError> {
        clipboard_win::seq_num()
            .map(|sequence| u64::from(sequence.get()))
            .ok_or(ClipboardError::Unavailable)
    }

    fn contents(&self) -> Result<ClipboardContents, ClipboardError> {
        let _clipboard = clipboard_win::Clipboard::new_attempts(CLIPBOARD_OPEN_ATTEMPTS)
            .map_err(|_| ClipboardError::Unavailable)?;
        let sequence = clipboard_win::seq_num()
            .map(|value| u64::from(value.get()))
            .ok_or(ClipboardError::Unavailable)?;
        let has_file_transfer = windows_file_format_available();
        let text = if has_file_transfer {
            None
        } else {
            use clipboard_win::Getter;
            let mut value = String::new();
            match clipboard_win::formats::Unicode.read_clipboard(&mut value) {
                Ok(_) => Some(value.into_bytes()),
                Err(_) => None,
            }
        };
        Ok(ClipboardContents {
            sequence,
            has_file_transfer,
            text,
        })
    }
}

#[cfg(target_os = "windows")]
fn windows_file_format_available() -> bool {
    use clipboard_win::formats::CF_HDROP;

    if clipboard_win::is_format_avail(CF_HDROP) {
        return true;
    }
    // These are the shell/file-promise formats used by Explorer and common
    // drag-and-drop providers. Registering an existing name only resolves its
    // format id; it does not read or mutate the clipboard contents.
    [
        "Shell IDList Array",
        "Shell Object Offsets",
        "Preferred DropEffect",
        "FileName",
        "FileNameW",
        "FileContents",
        "FileGroupDescriptor",
        "FileGroupDescriptorW",
    ]
    .into_iter()
    .filter_map(clipboard_win::register_format)
    .any(|format| clipboard_win::is_format_avail(format.get()))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct SystemClipboardBackend;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl ClipboardBackend for SystemClipboardBackend {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        Err(ClipboardError::Unavailable)
    }

    fn write_text(&self, _text: &str) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unavailable)
    }

    fn sequence(&self) -> Result<u64, ClipboardError> {
        Err(ClipboardError::Unavailable)
    }

    fn contents(&self) -> Result<ClipboardContents, ClipboardError> {
        Err(ClipboardError::Unavailable)
    }
}

pub(crate) fn system_clipboard() -> &'static dyn ClipboardBackend {
    static BACKEND: SystemClipboardBackend = SystemClipboardBackend;
    &BACKEND
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn state_test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn remote_text_validation_enforces_nonempty_utf8_nul_and_byte_limit() {
        assert_eq!(validate_remote_text(b"hello"), Ok("hello"));
        assert_eq!(
            validate_remote_text(&vec![b'a'; MAX_REMOTE_CLIPBOARD_TEXT_BYTES])
                .unwrap()
                .len(),
            MAX_REMOTE_CLIPBOARD_TEXT_BYTES
        );
        assert_eq!(validate_remote_text(b""), Err(ClipboardError::Empty));
        assert_eq!(
            validate_remote_text(&[b'a'; MAX_REMOTE_CLIPBOARD_TEXT_BYTES + 1]),
            Err(ClipboardError::TooLarge)
        );
        assert_eq!(
            validate_remote_text(&[0xff]),
            Err(ClipboardError::InvalidUtf8)
        );
        assert_eq!(validate_remote_text(b"a\0b"), Err(ClipboardError::Nul));
    }

    #[test]
    fn file_transfer_is_rejected_before_companion_text() {
        let contents = ClipboardContents {
            sequence: 7,
            has_file_transfer: true,
            text: Some(b"C:\\Users\\user\\file.txt".to_vec()),
        };
        assert_eq!(
            validate_clipboard_contents(&contents),
            Err(ClipboardError::FileTransfer)
        );
    }

    #[test]
    fn valid_text_is_owned_without_mutating_the_source() {
        let contents = ClipboardContents {
            sequence: 12,
            has_file_transfer: false,
            text: Some("ordinary/path-looking text".as_bytes().to_vec()),
        };
        assert_eq!(
            validate_clipboard_contents(&contents).unwrap(),
            contents.text.unwrap()
        );
    }

    #[test]
    fn operation_ids_are_lowercase_hex_and_fixed_length() {
        let id = new_operation_id().unwrap();
        assert_eq!(id.len(), REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH);
        assert!(operation_id_is_valid(&id));
        assert!(!operation_id_is_valid(&id.to_ascii_uppercase()));
        assert!(!operation_id_is_valid("short"));
    }

    #[test]
    fn latest_pending_copy_replaces_previous_and_expires() {
        let _guard = state_test_lock();
        clear_pending_copy();
        let now = Instant::now();
        replace_pending_copy(PendingCopy {
            operation_id: "a".repeat(REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH),
            owner_identity: "owner-a".to_string(),
            window_id: 1,
            grant_token: "token-a".to_string(),
            local_clipboard_sequence: 4,
            expires_at: now + REMOTE_CLIPBOARD_OPERATION_TTL,
        });
        replace_pending_copy(PendingCopy {
            operation_id: "b".repeat(REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH),
            owner_identity: "owner-b".to_string(),
            window_id: 2,
            grant_token: "token-b".to_string(),
            local_clipboard_sequence: 5,
            expires_at: now + REMOTE_CLIPBOARD_OPERATION_TTL,
        });
        assert_eq!(
            pending_copy(now).unwrap().operation_id,
            "b".repeat(REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH)
        );
        assert!(
            pending_copy(now + REMOTE_CLIPBOARD_OPERATION_TTL + Duration::from_nanos(1)).is_none()
        );
    }

    #[test]
    fn pending_copy_is_consumed_only_by_exact_correlation() {
        let _guard = state_test_lock();
        clear_pending_copy();
        let now = Instant::now();
        replace_pending_copy(PendingCopy {
            operation_id: "c".repeat(REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH),
            owner_identity: "owner".to_string(),
            window_id: 3,
            grant_token: "token".to_string(),
            local_clipboard_sequence: 8,
            expires_at: now + REMOTE_CLIPBOARD_OPERATION_TTL,
        });
        assert!(take_pending_copy_if(now, |pending| pending.window_id == 99).is_none());
        assert!(pending_copy(now).is_some());
        assert!(take_pending_copy_if(now, |pending| {
            pending.operation_id == "c".repeat(REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH)
                && pending.owner_identity == "owner"
                && pending.window_id == 3
                && pending.grant_token == "token"
        })
        .is_some());
        assert!(pending_copy(now).is_none());
    }

    #[test]
    fn copy_and_paste_state_remain_independent() {
        let _guard = state_test_lock();
        clear_pending_copy();
        clear_copy_operations();
        clear_paste_operations();
        let now = Instant::now();
        replace_pending_copy(PendingCopy {
            operation_id: "d".repeat(REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH),
            owner_identity: "owner".to_string(),
            window_id: 4,
            grant_token: "token".to_string(),
            local_clipboard_sequence: 9,
            expires_at: now + REMOTE_CLIPBOARD_OPERATION_TTL,
        });
        assert!(reserve_paste_operation(
            "controller",
            "paste-operation",
            now
        ));
        assert!(reserve_copy_operation("controller", "copy-operation", now));
        assert!(!reserve_copy_operation("controller", "copy-operation", now));
        assert!(pending_copy(now).is_some());
        assert!(!reserve_paste_operation(
            "controller",
            "paste-operation",
            now
        ));
        clear_pending_copy();
        clear_copy_operations();
        clear_paste_operations();
    }

    #[test]
    fn paste_reservation_deduplicates_authenticated_sender_and_operation() {
        let _guard = state_test_lock();
        clear_paste_operations();
        let now = Instant::now();
        assert!(reserve_paste_operation("sender", "operation", now));
        assert!(!reserve_paste_operation("sender", "operation", now));
        assert!(reserve_paste_operation("other-sender", "operation", now));
        assert!(reserve_paste_operation(
            "sender",
            "operation",
            now + REMOTE_CLIPBOARD_OPERATION_TTL + Duration::from_nanos(1)
        ));
        clear_paste_operations();
    }

    #[test]
    fn lifecycle_helpers_clear_pending_and_recent_sender_state() {
        let _guard = state_test_lock();
        clear_pending_copy();
        clear_copy_operations();
        clear_paste_operations();
        let now = Instant::now();
        replace_pending_copy(PendingCopy {
            operation_id: "e".repeat(REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH),
            owner_identity: "owner".to_string(),
            window_id: 5,
            grant_token: "token".to_string(),
            local_clipboard_sequence: 1,
            expires_at: now + REMOTE_CLIPBOARD_OPERATION_TTL,
        });
        assert!(reserve_paste_operation("owner", "paste", now));
        clear_pending_copy_for(5, Some("owner"));
        clear_paste_operations_for_sender("owner");
        assert!(pending_copy(now).is_none());
        assert!(reserve_paste_operation("owner", "paste", now));
        clear_paste_operations();
    }

    #[test]
    fn clipboard_operation_lock_is_exclusive_and_released() {
        let first = try_clipboard_operation_lock().expect("first lock");
        assert!(try_clipboard_operation_lock().is_none());
        drop(first);
        assert!(try_clipboard_operation_lock().is_some());
    }

    struct FakeClipboard {
        contents: Mutex<ClipboardContents>,
    }

    impl FakeClipboard {
        fn new(text: &[u8]) -> Self {
            Self {
                contents: Mutex::new(ClipboardContents {
                    sequence: 1,
                    has_file_transfer: false,
                    text: Some(text.to_vec()),
                }),
            }
        }
    }

    impl ClipboardBackend for FakeClipboard {
        fn read_text(&self) -> Result<Option<String>, ClipboardError> {
            Ok(self
                .contents
                .lock()
                .unwrap()
                .text
                .as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .map(str::to_string))
        }

        fn write_text(&self, text: &str) -> Result<(), ClipboardError> {
            let mut contents = self.contents.lock().unwrap();
            contents.sequence += 1;
            contents.text = Some(text.as_bytes().to_vec());
            Ok(())
        }

        fn sequence(&self) -> Result<u64, ClipboardError> {
            Ok(self.contents.lock().unwrap().sequence)
        }

        fn contents(&self) -> Result<ClipboardContents, ClipboardError> {
            Ok(self.contents.lock().unwrap().clone())
        }
    }

    #[test]
    fn fake_backend_supports_plain_text_transfer_without_a_platform() {
        let backend = FakeClipboard::new(b"clipboard text");
        assert_eq!(backend.read_transfer_text().unwrap(), b"clipboard text");
        backend.write_text("new text").unwrap();
        assert_eq!(backend.read_transfer_text().unwrap(), b"new text");
        assert_eq!(backend.sequence().unwrap(), 2);
    }
}
