//! Opaque Windows capture-target identity.
//!
//! Win32 `HWND` values are pointer-sized. They must never be narrowed into the
//! existing JavaScript/wire `window_id: u32`. Windows enumeration registers the
//! native handle here and exposes only the generated process-local token.
//! Thumbnail capture, live capture, border tracking, and input injection must
//! resolve that token through this registry before touching the native target.
//!
//! Windows and displays share ONE token space (a unified counter); the target
//! kind disambiguates. No display-id tagging is needed: tokens are generated
//! registry ids, never narrowed handles.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::sync_ext::MutexExt;

/// Which native object a token refers to. `HMONITOR` values live in a
/// different handle space than `HWND` values, but the two can be numerically
/// equal (handle reuse), so the kind is part of the dedupe key even though the
/// token counter itself is unified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TargetKind {
    Window,
    Display,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NativeTargetKey {
    raw_handle: usize,
    owner_process_id: u32,
    kind: TargetKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowsCaptureTarget {
    key: NativeTargetKey,
    /// `Some(1..)` for display targets: stable per monitor across
    /// re-enumerations (drives the picker's "Screen N" title). `None` for
    /// window targets.
    display_ordinal: Option<u32>,
}

impl WindowsCaptureTarget {
    /// Pointer-sized native value used only inside the Windows backend.
    pub(crate) fn raw_handle(self) -> usize {
        self.key.raw_handle
    }

    pub(crate) fn owner_process_id(self) -> u32 {
        self.key.owner_process_id
    }

    pub(crate) fn kind(self) -> TargetKind {
        self.key.kind
    }

    /// `Some` ordinal for display targets; `None` for windows. The picker's
    /// "Screen N" title comes from here, never from the unified token value.
    pub(crate) fn display_ordinal(self) -> Option<u32> {
        self.display_ordinal
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub(crate) enum TargetRegistryError {
    #[error("native window handle must not be null")]
    NullHandle,
    #[error("capture target token {0} is unknown or stale")]
    UnknownOrStale(u32),
    #[error("capture target token space is exhausted")]
    TokenSpaceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HoverTargetReplacement {
    retired_token: u32,
    replacement_token: u32,
}

#[derive(Debug)]
pub(crate) struct CaptureTargetRegistry {
    next_token: u32,
    by_token: HashMap<u32, WindowsCaptureTarget>,
    by_native_target: HashMap<NativeTargetKey, u32>,
    next_display_ordinal: u32,
    /// One-shot handoff reserved for the currently presented hover target.
    /// The normal resolver never consults this slot.
    pending_hover_replacement: Option<HoverTargetReplacement>,
}

impl Default for CaptureTargetRegistry {
    fn default() -> Self {
        Self {
            next_token: 1,
            by_token: HashMap::new(),
            by_native_target: HashMap::new(),
            next_display_ordinal: 1,
            pending_hover_replacement: None,
        }
    }
}

impl CaptureTargetRegistry {
    /// Register one currently-live native window and return its stable token.
    ///
    /// Re-registering the same `(HWND, PID)` while it remains live returns the
    /// existing token. After invalidation, registering a reused native handle
    /// allocates a new token so old frontend values cannot select the new
    /// window accidentally.
    pub(crate) fn register(
        &mut self,
        raw_handle: usize,
        owner_process_id: u32,
    ) -> Result<u32, TargetRegistryError> {
        if raw_handle == 0 {
            return Err(TargetRegistryError::NullHandle);
        }

        let key = NativeTargetKey {
            raw_handle,
            owner_process_id,
            kind: TargetKind::Window,
        };
        if let Some(token) = self.by_native_target.get(&key) {
            return Ok(*token);
        }

        let token = self.allocate_token()?;
        let target = WindowsCaptureTarget {
            key,
            display_ordinal: None,
        };
        self.by_token.insert(token, target);
        self.by_native_target.insert(key, token);
        Ok(token)
    }

    /// Register one currently-live display (`HMONITOR`) and return its stable
    /// token from the SAME counter as windows (unified token space — no tag,
    /// no separate range). Re-registering the same monitor returns the
    /// existing token with its stable display ordinal.
    pub(crate) fn register_display(
        &mut self,
        raw_handle: usize,
    ) -> Result<u32, TargetRegistryError> {
        if raw_handle == 0 {
            return Err(TargetRegistryError::NullHandle);
        }

        let key = NativeTargetKey {
            raw_handle,
            owner_process_id: 0,
            kind: TargetKind::Display,
        };
        if let Some(token) = self.by_native_target.get(&key) {
            return Ok(*token);
        }

        let token = self.allocate_token()?;
        let display_ordinal = self.allocate_display_ordinal()?;
        let target = WindowsCaptureTarget {
            key,
            display_ordinal: Some(display_ordinal),
        };
        self.by_token.insert(token, target);
        self.by_native_target.insert(key, token);
        Ok(token)
    }

    pub(crate) fn resolve(&self, token: u32) -> Result<WindowsCaptureTarget, TargetRegistryError> {
        self.by_token
            .get(&token)
            .copied()
            .ok_or(TargetRegistryError::UnknownOrStale(token))
    }

    pub(crate) fn invalidate(&mut self, token: u32) -> bool {
        let Some(target) = self.by_token.remove(&token) else {
            return false;
        };
        self.by_native_target.remove(&target.key);
        true
    }

    /// Retire a live window token and atomically allocate a fresh token for
    /// the same native target. The old token is removed from the resolver
    /// immediately; only the hover tracker may consume the one-shot handoff.
    /// A single pending slot keeps this exception bounded and prevents a
    /// replacement from becoming a general stale-token redirect.
    pub(crate) fn retire_for_hover(&mut self, token: u32) -> Option<u32> {
        if self.pending_hover_replacement.is_some() {
            return None;
        }
        let target = self.by_token.get(&token).copied()?;
        if target.kind() != TargetKind::Window {
            return None;
        }
        // Allocate while the old token is still occupied so the replacement
        // is always distinct, including at the token-space boundary.
        let replacement_token = self.allocate_token().ok()?;
        debug_assert_eq!(self.by_native_target.get(&target.key), Some(&token));
        self.by_token.remove(&token);
        self.by_native_target.remove(&target.key);
        self.by_token.insert(replacement_token, target);
        self.by_native_target.insert(target.key, replacement_token);
        self.pending_hover_replacement = Some(HoverTargetReplacement {
            retired_token: token,
            replacement_token,
        });
        Some(replacement_token)
    }

    /// Consume the bounded hover handoff for `retired_token`. A mismatched
    /// token also clears the slot: it is no longer the current hover target.
    pub(crate) fn consume_hover_replacement(&mut self, retired_token: u32) -> Option<u32> {
        let replacement = self.pending_hover_replacement.take()?;
        (replacement.retired_token == retired_token).then_some(replacement.replacement_token)
    }

    fn allocate_token(&mut self) -> Result<u32, TargetRegistryError> {
        if self.next_token == 0 {
            self.next_token = 1;
        }
        let start = self.next_token;
        loop {
            let candidate = self.next_token;
            self.next_token = self.next_token.wrapping_add(1);
            if self.next_token == 0 {
                self.next_token = 1;
            }
            if !self.by_token.contains_key(&candidate) {
                return Ok(candidate);
            }
            if self.next_token == start {
                return Err(TargetRegistryError::TokenSpaceExhausted);
            }
        }
    }

    fn allocate_display_ordinal(&mut self) -> Result<u32, TargetRegistryError> {
        let ordinal = self.next_display_ordinal;
        if ordinal == 0 {
            return Err(TargetRegistryError::TokenSpaceExhausted);
        }
        self.next_display_ordinal = self.next_display_ordinal.wrapping_add(1);
        if self.next_display_ordinal == 0 {
            self.next_display_ordinal = 1;
        }
        Ok(ordinal)
    }
}

fn registry() -> &'static Mutex<CaptureTargetRegistry> {
    static REGISTRY: OnceLock<Mutex<CaptureTargetRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(CaptureTargetRegistry::default()))
}

/// Enumeration's sole path for turning an `HWND` into the public `u32` token.
pub(crate) fn register(
    raw_handle: usize,
    owner_process_id: u32,
) -> Result<u32, TargetRegistryError> {
    registry()
        .lock_unpoisoned()
        .register(raw_handle, owner_process_id)
}

/// Enumeration's sole path for turning an `HMONITOR` into the public `u32`
/// token (same counter as windows).
pub(crate) fn register_display(raw_handle: usize) -> Result<u32, TargetRegistryError> {
    registry().lock_unpoisoned().register_display(raw_handle)
}

/// Every Windows native-media/input consumer resolves public tokens here.
pub(crate) fn resolve(token: u32) -> Result<WindowsCaptureTarget, TargetRegistryError> {
    registry().lock_unpoisoned().resolve(token)
}

/// Retire a public token when its production capture instance ends. Native
/// handles are reusable; keeping the old token alive would let a later HWND or
/// monitor instance inherit stale grants and cached geometry.
pub(crate) fn invalidate(token: u32) -> bool {
    registry().lock_unpoisoned().invalidate(token)
}

/// Windows hover tracker seam: retire a hovered window token while leaving a
/// one-shot replacement for that tracker. This is not a resolver redirect.
pub(crate) fn retire_for_hover(token: u32) -> Option<u32> {
    registry().lock_unpoisoned().retire_for_hover(token)
}

/// Consume a replacement minted by `retire_for_hover`; consumption is scoped
/// to the exact retired token and removes the bounded handoff immediately.
pub(crate) fn consume_hover_replacement(retired_token: u32) -> Option<u32> {
    registry()
        .lock_unpoisoned()
        .consume_hover_replacement(retired_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_sized_handle_round_trips_without_becoming_the_wire_token() {
        let mut registry = CaptureTargetRegistry::default();
        #[cfg(target_pointer_width = "64")]
        let raw_handle = (u32::MAX as usize) + 0x12_345;
        #[cfg(target_pointer_width = "32")]
        let raw_handle = 0xf123_4567usize;

        let token = registry.register(raw_handle, 42).unwrap();
        let target = registry.resolve(token).unwrap();

        assert_ne!(token as usize, raw_handle);
        assert_eq!(target.raw_handle(), raw_handle);
        assert_eq!(target.owner_process_id(), 42);
        assert_eq!(target.kind(), TargetKind::Window);
        assert_eq!(target.display_ordinal(), None);
    }

    #[test]
    fn invalidation_rejects_stale_tokens_and_handle_reuse_gets_a_new_token() {
        let mut registry = CaptureTargetRegistry::default();
        let first = registry.register(0x1234, 7).unwrap();
        assert_eq!(registry.register(0x1234, 7).unwrap(), first);

        assert!(registry.invalidate(first));
        assert_eq!(
            registry.resolve(first),
            Err(TargetRegistryError::UnknownOrStale(first))
        );

        let replacement = registry.register(0x1234, 7).unwrap();
        assert_ne!(replacement, first);
        assert_eq!(registry.resolve(replacement).unwrap().raw_handle(), 0x1234);
    }

    #[test]
    fn hover_replacement_retires_old_token_and_consumes_one_bounded_handoff() {
        let mut registry = CaptureTargetRegistry::default();
        let raw_handle = 0x2345;
        let owner_process_id = 17;
        let first = registry.register(raw_handle, owner_process_id).unwrap();

        let replacement = registry
            .retire_for_hover(first)
            .expect("the live hover target should receive a replacement token");
        assert_ne!(replacement, first);
        assert_eq!(
            registry.resolve(first),
            Err(TargetRegistryError::UnknownOrStale(first))
        );
        let replacement_target = registry.resolve(replacement).unwrap();
        assert_eq!(replacement_target.raw_handle(), raw_handle);
        assert_eq!(replacement_target.owner_process_id(), owner_process_id);

        // A retired token never redirects, and a second pending replacement
        // cannot overwrite the bounded slot.
        let second = registry.register(0x3456, owner_process_id).unwrap();
        assert_eq!(registry.retire_for_hover(second), None);
        assert_eq!(registry.consume_hover_replacement(first), Some(replacement));
        assert_eq!(registry.consume_hover_replacement(first), None);
        assert_eq!(registry.retire_for_hover(first), None);

        let second_replacement = registry
            .retire_for_hover(second)
            .expect("the bounded slot is reusable after consumption");
        assert_eq!(
            registry.consume_hover_replacement(second),
            Some(second_replacement)
        );
        assert_eq!(registry.consume_hover_replacement(second), None);
    }

    #[test]
    fn null_native_handles_are_never_registered() {
        let mut registry = CaptureTargetRegistry::default();
        assert_eq!(
            registry.register(0, 1),
            Err(TargetRegistryError::NullHandle)
        );
        assert_eq!(
            registry.register_display(0),
            Err(TargetRegistryError::NullHandle)
        );
    }

    #[test]
    fn display_registration_gets_kind_and_stable_ordinal_from_the_unified_counter() {
        let mut registry = CaptureTargetRegistry::default();
        let window_token = registry.register(0x1000, 1).unwrap();
        let display_one = registry.register_display(0x2000).unwrap();
        let display_two = registry.register_display(0x3000).unwrap();

        let window_target = registry.resolve(window_token).unwrap();
        assert_eq!(window_target.kind(), TargetKind::Window);
        assert_eq!(window_target.display_ordinal(), None);

        let one = registry.resolve(display_one).unwrap();
        assert_eq!(one.kind(), TargetKind::Display);
        assert_eq!(one.raw_handle(), 0x2000);
        assert_eq!(one.owner_process_id(), 0);
        assert_eq!(one.display_ordinal(), Some(1));

        let two = registry.resolve(display_two).unwrap();
        assert_eq!(two.kind(), TargetKind::Display);
        assert_eq!(two.display_ordinal(), Some(2));

        // Unified counter: display tokens never collide with live window
        // tokens, and are simply consecutive allocations.
        assert_ne!(display_one, window_token);
        assert_ne!(display_two, window_token);
        assert_ne!(display_one, display_two);
    }

    #[test]
    fn display_re_registration_dedupes_token_and_ordinal() {
        let mut registry = CaptureTargetRegistry::default();
        let first = registry.register_display(0x4000).unwrap();
        assert_eq!(registry.register_display(0x4000).unwrap(), first);
        let target = registry.resolve(first).unwrap();
        assert_eq!(target.kind(), TargetKind::Display);
        assert_eq!(target.display_ordinal(), Some(1));

        // The same numeric handle registered as a window is a DIFFERENT
        // target: HWND and HMONITOR are separate handle spaces that can
        // numerically overlap (handle reuse), so kind disambiguates.
        let window = registry.register(0x4000, 9).unwrap();
        assert_ne!(window, first);
        assert_eq!(registry.resolve(window).unwrap().kind(), TargetKind::Window);
        assert_eq!(registry.resolve(first).unwrap().kind(), TargetKind::Display);
    }

    #[test]
    fn display_tokens_support_resolve_and_invalidate_like_windows() {
        let mut registry = CaptureTargetRegistry::default();
        let token = registry.register_display(0x5000).unwrap();
        assert_eq!(registry.resolve(token).unwrap().display_ordinal(), Some(1));

        assert!(registry.invalidate(token));
        assert_eq!(
            registry.resolve(token),
            Err(TargetRegistryError::UnknownOrStale(token))
        );

        // Handle reuse after invalidation allocates a new token (and a new
        // ordinal — the old one is never resurrected).
        let replacement = registry.register_display(0x5000).unwrap();
        assert_ne!(replacement, token);
        assert_eq!(
            registry.resolve(replacement).unwrap().display_ordinal(),
            Some(2)
        );
    }
}
