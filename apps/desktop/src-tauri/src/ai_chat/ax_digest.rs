//! Best-effort macOS Accessibility-tree context for the AI chat session (#656).
//!
//! An AI chat session about a shared window sends ~1fps screenshots to the
//! model. A screenshot only carries what is *rendered*: scrolled-away rows, a
//! collapsed outline, a long text field's tail and off-screen list items are
//! all invisible to it. This module walks the shared window's Accessibility
//! tree and serializes it into a compact, budget-bounded text digest that rides
//! along as extra context.
//!
//! Ported from the single-user reference implementation in the sibling `takt`
//! repo (`src-tauri/src/gemini_live/ax_digest.rs`, same author), including every
//! hardening it accumulated. See "Hardening" below before changing any of it.
//!
//! The AX side is deliberately isolated from the serializer. AX calls can block
//! inside the system framework, so the native walk runs on its own OS thread —
//! never inside an async select loop, where a blocking framework call starves
//! the WebSocket — and every handle carries a short messaging timeout. The
//! serializer below is pure Rust so its byte budget, truncation, ordering,
//! redaction and index metadata can be tested without a display or an
//! Accessibility-trusted target application.
//!
//! # Hardening (each of these fixed a real crash or leak — do not drop any)
//! - `AXValue` is type-checked before any CFString cast. For AXCheckBox,
//!   AXSlider, AXScrollBar and friends it is a CFNumber/CFBoolean, and blindly
//!   wrapping it as a CFString dispatches CFString's ObjC methods against a
//!   differently-shaped object.
//! - CFString reads are bounded via `CFStringGetCharacters` into a fixed
//!   buffer, with a UTF-16 surrogate-pair boundary extension.
//! - Child handles are materialized only up to the remaining visit budget.
//! - `AXUIElementSetMessagingTimeout` is installed **and checked** per handle;
//!   an element whose timeout could not be installed is dropped, because
//!   messaging timeouts do not inherit from a parent element.
//! - Three consecutive `kAXErrorCannotComplete` trip a session-level circuit
//!   breaker that stays open for the rest of the session.
//! - Every failure degrades silently to "no digest". None of this is ever
//!   surfaced to the user or to the model.
//!
//! # Petal-specific: secure fields are never digested
//! The shared window's *pixels* are already visible to every meeting
//! participant, but the AX tree can expose text that was never rendered — and a
//! secure text field's contents are exactly that. Any node whose role or
//! subrole reads as a masked input has its `value` dropped, and so does every
//! descendant of such a node. The native walk additionally never *reads*
//! `AXValue` for those elements, so the secret does not enter this process at
//! all. See [`is_masked_input`] and its tests.
//!
//! Digest CONTENT is never logged. Only its length and hash, at debug level.

// The walk and its session plumbing are wired into `session.rs` as of #656.
// `lookup_index` and the generation history it reads are the one deliberate
// exception: they exist so a #658 tool call citing {generation, element_index}
// can be resolved against the generation the model actually saw, or rejected
// as stale. That is a correctness property of the digest itself — retaining
// the maps is why a stale citation can be DETECTED rather than mis-resolved to
// whatever now sits at that index — so it ships with the digest rather than
// being retrofitted. It carries a targeted allow at its definition; there is
// no module-wide allow, so anything else unreachable is a real warning.

use std::collections::{hash_map::DefaultHasher, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

pub(crate) const DIGEST_BYTE_BUDGET: usize = 12 * 1024;
pub(crate) const DIGEST_MAX_DEPTH: usize = 25;
pub(crate) const DIGEST_MAX_NODES: usize = 5_000;
pub(crate) const DIGEST_MAX_TEXT_CHARS: usize = 200;
pub(crate) const DIGEST_REFRESH_BUDGET: u8 = 5;

const DIGEST_REFRESH_INTERVAL_SECS: u64 = 10;
const DIGEST_WALK_DEADLINE_MS: u64 = 2_000;
const DIGEST_GENERATION_HISTORY: usize = 32;
const TRUNCATED_MARKER: &str = "…[digest truncated]";

/// Roles/subroles whose value must never enter the digest. Matched
/// case-insensitively against BOTH `AXRole` and `AXSubrole`: AppKit exposes a
/// password field as role `AXSecureTextField`, while web content commonly
/// exposes role `AXTextField` + subrole `AXSecureTextField`.
const MASKED_INPUT_ROLES: &[&str] = &[
    "axsecuretextfield",
    "axsecuretextarea",
    "axpasswordfield",
    "axsecuretextfieldrole",
];

/// A displayable AX node. The geometry is kept as plain `f64`s rather than a
/// native CGRect so the serializer remains independent of CoreGraphics and easy
/// to test.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DigestNode {
    pub role: String,
    /// `AXSubrole`, when the element publishes one. Carried purely so the pure
    /// serializer can recognize a masked input independently of the walk.
    pub subrole: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    /// Top-left of the element in global screen coordinates, when AX published
    /// one. `None` means "position unreadable" and must deny a click (#658) —
    /// a click needs a point we can prove lies inside the shared window.
    pub origin: Option<(f64, f64)>,
    pub width: f64,
    pub height: f64,
    pub children: Vec<DigestNode>,
}

impl DigestNode {
    #[cfg(test)]
    fn leaf(role: &str, title: Option<&str>, value: Option<&str>) -> Self {
        Self {
            role: role.to_string(),
            subrole: None,
            title: title.map(str::to_string),
            value: value.map(str::to_string),
            origin: Some((0.0, 0.0)),
            width: 100.0,
            height: 20.0,
            children: Vec::new(),
        }
    }
}

/// An indexed element's geometry as of the generation that named it, in global
/// screen coordinates. Carried on the index (not just the node) because #658's
/// `window_click` has to turn a cited `[n]` into an actual point and then prove
/// that point still lies inside the shared window's CURRENT frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DigestRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl DigestRect {
    /// Centre point, which is what a click targets.
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// Whether this rect describes something clickable at all. A zero-area or
    /// non-finite rect yields a meaningless point, so it denies rather than
    /// producing a click at an arbitrary coordinate.
    pub fn is_usable(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.0
            && self.height > 0.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DigestIndex {
    pub role: String,
    pub title: Option<String>,
    pub ancestor_path: Vec<String>,
    /// Where the element was when this generation was produced. `None` when AX
    /// would not report a position — which denies a click.
    pub rect: Option<DigestRect>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SerializedDigest {
    pub text: String,
    pub indexes: HashMap<usize, DigestIndex>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DigestSnapshot {
    pub generation: u64,
    pub text: String,
    pub hash: u64,
}

/// Session-owned history for the later indexed-control consumer. We retain
/// several generations rather than only the latest map so a tool call citing
/// `{generation, element_index}` can be resolved against the generation the
/// model actually saw — or rejected as stale.
#[derive(Debug, Default)]
pub(crate) struct DigestSessionState {
    pub next_generation: u64,
    pub generations: VecDeque<DigestGeneration>,
    pub last_sent_hash: Option<u64>,
    /// Text of the newest generation, retained so a tool call that cites a
    /// generation we no longer hold can be answered with the CURRENT snapshot
    /// instead of leaving the model to retry a dead reference (#658).
    pub last_text: Option<String>,
    pub refreshes_sent: u8,
    pub failure_logged: bool,
    /// Once the target has tripped the AX timeout circuit breaker, do not
    /// spend another bounded walk attempt on it during this session.
    pub circuit_breaker_open: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DigestGeneration {
    pub generation: u64,
    pub hash: u64,
    pub indexes: HashMap<usize, DigestIndex>,
}

/// Resolve an indexed node against the exact digest generation referenced by
/// the model. A generation that has fallen out of the retained history, or an
/// index that was not present in that generation, is stale.
pub(crate) fn lookup_index(
    state: &DigestSessionState,
    generation: u64,
    element_index: usize,
) -> Option<DigestIndex> {
    state
        .generations
        .iter()
        .find(|candidate| candidate.generation == generation)
        .and_then(|candidate| candidate.indexes.get(&element_index))
        .cloned()
}

/// What the target application says it is focused on, read at the moment of
/// execution (#658). Every field is optional because AX may simply decline to
/// answer, and every "we could not tell" has to deny.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FocusedContext {
    pub role: Option<String>,
    pub subrole: Option<String>,
    /// Whether the application's AX *focused window* is the window being
    /// shared. Typing is only ever allowed into the shared window; without this
    /// the model could type into a different document of the same app that
    /// nobody in the meeting can see.
    pub window_matches: bool,
}

/// Why typing into the currently focused element must be refused, if it must.
///
/// Fail-closed on both halves: an unreadable role denies (we cannot prove it is
/// not a password field), and a focused window that is not the shared one
/// denies regardless of role.
pub(crate) fn focused_typing_refusal(context: &FocusedContext) -> Option<&'static str> {
    if !context.window_matches {
        return Some("focused_window_mismatch");
    }
    let role = context
        .role
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty());
    let Some(role) = role else {
        return Some("focused_role_unknown");
    };
    if is_masked_input(role, context.subrole.as_deref()) {
        return Some("focused_field_secure");
    }
    None
}

#[derive(Debug)]
pub(crate) enum DigestEvent {
    /// The first walk always emits an event, including `None`, so the session
    /// can preserve its greeting timing when AX is unavailable.
    First(Option<DigestSnapshot>),
    /// Later events are emitted only for a changed digest and within the resend
    /// budget. Unchanged walks never enter the WebSocket loop.
    Refresh(DigestSnapshot),
}

pub(crate) fn hash_digest(text: &str) -> u64 {
    // The first line is a per-generation label, so exclude it from change
    // detection; otherwise an unchanged tree would resend every ten seconds
    // solely because its diagnostic generation number advanced.
    let stable_content = text.split_once('\n').map_or(text, |(_, body)| body);
    let mut hasher = DefaultHasher::new();
    stable_content.hash(&mut hasher);
    hasher.finish()
}

/// Whether an element's role/subrole identifies a masked input whose value must
/// never be digested.
///
/// Fail-closed by design: alongside the known role list this also matches any
/// role/subrole containing "secure" or "password", so a role we have not seen
/// before loses its value rather than leaking it.
pub(crate) fn is_masked_input(role: &str, subrole: Option<&str>) -> bool {
    is_masked_token(role) || subrole.is_some_and(is_masked_token)
}

fn is_masked_token(token: &str) -> bool {
    let lowered = token.trim().to_ascii_lowercase();
    MASKED_INPUT_ROLES.contains(&lowered.as_str())
        || lowered.contains("secure")
        || lowered.contains("password")
}

fn node_is_masked(node: &DigestNode) -> bool {
    is_masked_input(&node.role, node.subrole.as_deref())
}

/// Serialize a tree in two passes: text-bearing nodes are scored and admitted
/// first, then structural/window-chrome lines consume whatever budget remains.
/// The final output is restored to tree order so the indentation stays useful.
pub(crate) fn serialize_digest(generation: u64, root: &DigestNode) -> SerializedDigest {
    let header = format!(
        "AX accessibility snapshot generation {generation}. Use [n] to refer to indexed nodes.\n"
    );
    let mut flat = Vec::new();
    collect_nodes(root, 0, &[], false, &mut 0, &mut flat);

    let mut content_order: Vec<usize> = flat
        .iter()
        .filter(|node| node.has_own_text)
        .map(|node| node.order)
        .collect();
    content_order.sort_by_key(|order| {
        let node = &flat[*order];
        (std::cmp::Reverse(node.score), node.order)
    });
    let mut chrome_order: Vec<usize> = flat
        .iter()
        .filter(|node| !node.has_own_text)
        .map(|node| node.order)
        .collect();
    chrome_order.sort_by_key(|order| {
        let node = &flat[*order];
        (std::cmp::Reverse(node.score), node.order)
    });

    let mut selected = vec![false; flat.len()];
    let mut used = header.len();
    let mut admit = |order: usize| {
        let node = &flat[order];
        let estimated = node.line(5_000, 0).len();
        if used + estimated <= DIGEST_BYTE_BUDGET {
            selected[order] = true;
            used += estimated;
        }
    };
    for order in content_order.into_iter().chain(chrome_order) {
        admit(order);
    }

    let omitted = selected.iter().filter(|included| !**included).count() > 0;
    let mut text = header;
    let mut indexes = HashMap::new();
    let mut output_index = 0usize;
    for node in flat.iter().filter(|node| selected[node.order]) {
        let line = node.line(output_index, node.depth);
        if text.len() + line.len() > DIGEST_BYTE_BUDGET {
            break;
        }
        text.push_str(&line);
        indexes.insert(
            output_index,
            DigestIndex {
                // Both role and title are truncated but NOT escaped on
                // `node` -- neither is safe to hand to a human-facing surface
                // as-is. describe_element() concatenates them into ONE
                // rendered string ("{role} \"{title}\""): an unescaped role
                // can plant a bidi override that title's own escaping cannot
                // close (the only way to end an override is another Cf pop
                // character, which escape_text strips from title), defeating
                // the visual guarantee for the whole string, not just half
                // of it. Both need the identical filter.
                role: escape_text(&node.role),
                title: node.title.as_deref().map(escape_text),
                ancestor_path: node.ancestor_path.clone(),
                rect: node.rect,
            },
        );
        output_index += 1;
    }

    let mut truncated = omitted || output_index < flat.len();
    if truncated {
        while text.len() + TRUNCATED_MARKER.len() > DIGEST_BYTE_BUDGET {
            let Some(last_newline) = text[..text.len().saturating_sub(1)].rfind('\n') else {
                break;
            };
            text.truncate(last_newline + 1);
            indexes.remove(&output_index.saturating_sub(1));
            output_index = output_index.saturating_sub(1);
        }
        if text.len() + TRUNCATED_MARKER.len() <= DIGEST_BYTE_BUDGET {
            text.push_str(TRUNCATED_MARKER);
        } else {
            // The header is intentionally short enough that this is
            // unreachable, but keep the hard byte guarantee if the constants
            // ever change.
            let mut byte_len = DIGEST_BYTE_BUDGET.min(text.len());
            while byte_len > 0 && !text.is_char_boundary(byte_len) {
                byte_len -= 1;
            }
            text.truncate(byte_len);
            truncated = false;
        }
    }
    let _ = truncated;
    SerializedDigest { text, indexes }
}

#[derive(Debug)]
struct FlatNode {
    order: usize,
    depth: usize,
    role: String,
    title: Option<String>,
    value: Option<String>,
    ancestor_path: Vec<String>,
    rect: Option<DigestRect>,
    score: usize,
    has_own_text: bool,
}

impl FlatNode {
    fn line(&self, index: usize, depth: usize) -> String {
        let role = escape_text(&truncate_text(&self.role));
        let title = self
            .title
            .as_deref()
            .map(|value| format!(" \"{}\"", escape_text(&truncate_text(value))))
            .unwrap_or_default();
        let value = self
            .value
            .as_deref()
            .map(|value| format!(" value=\"{}\"", escape_text(&truncate_text(value))))
            .unwrap_or_default();
        format!("{}[{index}] {role}{title}{value}\n", "  ".repeat(depth))
    }
}

fn collect_nodes(
    node: &DigestNode,
    depth: usize,
    ancestors: &[String],
    masked_ancestor: bool,
    next_order: &mut usize,
    flat: &mut Vec<FlatNode>,
) -> usize {
    if depth > DIGEST_MAX_DEPTH || *next_order >= DIGEST_MAX_NODES {
        return 0;
    }
    // A secure field redacts itself AND its whole subtree: some toolkits hang
    // the editable text off a child element rather than the field itself.
    let masked = masked_ancestor || node_is_masked(node);
    let mut child_score = 0usize;
    for child in &node.children {
        child_score = child_score.saturating_add(estimate_score(child, depth + 1, masked));
    }
    let title = node.title.as_deref().map(truncate_text);
    let value = if masked {
        None
    } else {
        node.value.as_deref().map(truncate_text)
    };
    let has_own_text = title.as_ref().is_some_and(|text| !text.is_empty())
        || value.as_ref().is_some_and(|text| !text.is_empty());
    let renderable = node.width > 0.0
        && node.height > 0.0
        && (has_own_text || (!is_empty_container(node) && child_score > 0));
    let own_score = title.as_ref().map_or(0, String::len) + value.as_ref().map_or(0, String::len);
    let score = own_score.saturating_mul(4).saturating_add(child_score);
    if renderable {
        let order = *next_order;
        *next_order += 1;
        flat.push(FlatNode {
            order,
            depth,
            role: node.role.clone(),
            title,
            value,
            ancestor_path: ancestors.to_vec(),
            rect: node.origin.map(|(x, y)| DigestRect {
                x,
                y,
                width: node.width,
                height: node.height,
            }),
            score,
            has_own_text,
        });
    }
    if depth == DIGEST_MAX_DEPTH {
        return score;
    }
    let mut child_ancestors = ancestors.to_vec();
    child_ancestors.push(node_path_label(node));
    for child in &node.children {
        collect_nodes(
            child,
            depth + 1,
            &child_ancestors,
            masked,
            next_order,
            flat,
        );
    }
    score
}

fn estimate_score(node: &DigestNode, depth: usize, masked_ancestor: bool) -> usize {
    if depth > DIGEST_MAX_DEPTH {
        return 0;
    }
    let masked = masked_ancestor || node_is_masked(node);
    let own = node
        .title
        .as_deref()
        .map(truncate_text)
        .map_or(0, |s| s.len())
        + if masked {
            0
        } else {
            node.value
                .as_deref()
                .map(truncate_text)
                .map_or(0, |s| s.len())
        };
    let children = node
        .children
        .iter()
        .map(|child| estimate_score(child, depth + 1, masked))
        .sum::<usize>();
    let renderable = node.width > 0.0
        && node.height > 0.0
        && (own > 0 || (!is_empty_container(node) && !node.children.is_empty()));
    if renderable && (own > 0 || children > 0) {
        own.saturating_mul(4).saturating_add(children)
    } else {
        children
    }
}

fn is_empty_container(node: &DigestNode) -> bool {
    matches!(
        node.role.as_str(),
        "AXApplication"
            | "AXGroup"
            | "AXScrollArea"
            | "AXWebArea"
            | "AXWindow"
            | "AXSplitGroup"
            | "AXTabGroup"
            | "AXToolbar"
    )
}

fn node_path_label(node: &DigestNode) -> String {
    let role = if node.role.trim().is_empty() {
        "?"
    } else {
        node.role.trim()
    };
    match node
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(title) => format!("{role} \"{}\"", truncate_text(title)),
        None => role.to_string(),
    }
}

fn truncate_text(value: &str) -> String {
    let value = value.trim();
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(DIGEST_MAX_TEXT_CHARS).collect();
    if chars.next().is_some() {
        let mut truncated: String = prefix.chars().take(DIGEST_MAX_TEXT_CHARS - 1).collect();
        truncated.push('…');
        truncated
    } else {
        prefix
    }
}

fn truncate_text_with_known_suffix(value: &str) -> String {
    let value = value.trim();
    let mut truncated: String = value
        .chars()
        .take(DIGEST_MAX_TEXT_CHARS.saturating_sub(1))
        .collect();
    truncated.push('…');
    truncated
}

fn remaining_visit_budget(visited: usize) -> usize {
    DIGEST_MAX_NODES.saturating_sub(visited)
}

fn digest_attempt_allowed(state: &DigestSessionState) -> bool {
    !state.circuit_breaker_open
}

fn trip_digest_circuit_breaker(state: &mut DigestSessionState) {
    state.circuit_breaker_open = true;
}

/// Escapes text read from a shared window's AX tree before it reaches either
/// the model's context or a human-facing surface (the approval card's
/// `element` field, via `DigestIndex.title`).
///
/// Filters Cc (control) AND Cf (format) characters -- the latter includes
/// U+202E RIGHT-TO-LEFT OVERRIDE and the rest of the bidi/invisible-character
/// block. Window content is attacker-controlled input by this module's own
/// threat model (see the module doc comment); without this, a malicious
/// page's accessible text could make the approval card visually display a
/// different action than the one actually being approved, or smuggle
/// invisible instructions into the model's context. Same table
/// `control_policy::validate_text` uses on the model-authored-text side of
/// this same class of gap -- keep both in sync, don't drift.
fn escape_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            '\n' | '\r' | '\t' => " ".to_string(),
            ch if ch.is_control() || super::control_policy::is_unicode_format_character(ch) => {
                " ".to_string()
            }
            ch => ch.to_string(),
        })
        .collect()
}

fn commit_serialized(
    state: &Arc<Mutex<DigestSessionState>>,
    serialized: SerializedDigest,
) -> Option<DigestSnapshot> {
    let hash = hash_digest(&serialized.text);
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = state.next_generation;
    state.next_generation = state.next_generation.saturating_add(1);
    // Retained regardless of whether this walk is SENT: the resend budget and
    // the unchanged-hash short-circuit below suppress traffic, not the fact
    // that this is now the newest snapshot we hold.
    state.last_text = Some(serialized.text.clone());
    state.generations.push_back(DigestGeneration {
        generation,
        hash,
        indexes: serialized.indexes,
    });
    while state.generations.len() > DIGEST_GENERATION_HISTORY {
        state.generations.pop_front();
    }

    if state.last_sent_hash == Some(hash) {
        return None;
    }
    if state.last_sent_hash.is_some() && state.refreshes_sent >= DIGEST_REFRESH_BUDGET {
        return None;
    }
    if state.last_sent_hash.is_some() {
        state.refreshes_sent = state.refreshes_sent.saturating_add(1);
    }
    state.last_sent_hash = Some(hash);
    Some(DigestSnapshot {
        generation,
        text: serialized.text,
        hash,
    })
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use crate::platform::cg::{self, WindowFrame};
    use log::debug;
    use std::ffi::{c_char, c_void};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CFRange {
        location: isize,
        length: isize,
    }

    // Same declarations/conventions as `remote_control.rs`'s `mod input`, which
    // already owns Petal's Accessibility FFI surface. Petal holds the
    // Accessibility grant for remote control, so the digest needs no new
    // permission request of its own.
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
        fn AXUIElementSetMessagingTimeout(element: *const c_void, timeout: f32) -> i32;
        fn AXUIElementCopyAttributeValue(
            element: *const c_void,
            attribute: *const c_void,
            value: *mut *const c_void,
        ) -> i32;
        fn AXUIElementGetTypeID() -> usize;
        fn AXValueGetValue(value: *const c_void, the_type: i32, value_ptr: *mut c_void) -> bool;
        fn AXValueGetTypeID() -> usize;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRetain(cf: *const c_void) -> *const c_void;
        fn CFRelease(cf: *const c_void);
        fn CFGetTypeID(cf: *const c_void) -> usize;
        fn CFArrayGetCount(the_array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(the_array: *const c_void, idx: isize) -> *const c_void;
        fn CFArrayGetTypeID() -> usize;
        fn CFNumberGetValue(number: *const c_void, the_type: i64, value_ptr: *mut c_void) -> bool;
        fn CFNumberGetTypeID() -> usize;
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFStringGetLength(the_string: *const c_void) -> isize;
        fn CFStringGetCharacters(the_string: *const c_void, range: CFRange, buffer: *mut u16);
        fn CFStringGetTypeID() -> usize;
    }

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_SINT64: i64 = 4;
    const K_AX_VALUE_TYPE_CG_POINT: i32 = 1;
    const K_AX_VALUE_TYPE_CG_SIZE: i32 = 2;
    const K_AX_ERROR_SUCCESS: i32 = 0;
    const K_AX_ERROR_CANNOT_COMPLETE: i32 = -25204;
    const K_AX_ERROR_NO_VALUE: i32 = -25212;
    /// Matches `remote_control.rs`'s `AX_APP_MESSAGING_TIMEOUT_SECONDS`.
    const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.35;
    /// Window frame match tolerance, in logical points.
    const FRAME_MATCH_TOLERANCE: f64 = 4.0;

    /// An owned CoreFoundation reference (create rule): released exactly once.
    struct CfOwned {
        ptr: *const c_void,
    }

    // CF handles are immutable references to OS-managed objects; the digest
    // walk owns them on one worker thread at a time and never shares Rust
    // memory through them.
    unsafe impl Send for CfOwned {}

    impl CfOwned {
        fn from_create(ptr: *const c_void) -> Option<Self> {
            (!ptr.is_null()).then_some(Self { ptr })
        }

        fn as_ptr(&self) -> *const c_void {
            self.ptr
        }
    }

    impl Drop for CfOwned {
        fn drop(&mut self) {
            // SAFETY: `ptr` came from a CF create/retain and is released once.
            unsafe { CFRelease(self.ptr) };
        }
    }

    /// An `AXUIElement` handle that carries its own messaging timeout.
    struct AxElement {
        object: CfOwned,
    }

    impl AxElement {
        /// Adopt a create-rule `AXUIElementRef` and install the messaging
        /// timeout on it.
        ///
        /// The install result is CHECKED: messaging timeouts do not inherit
        /// from a parent element, so a handle whose timeout failed to install
        /// could block the walk inside the framework for the system default.
        /// Such a handle is dropped rather than walked.
        fn adopt(ptr: *const c_void) -> Option<Self> {
            let object = CfOwned::from_create(ptr)?;
            // SAFETY: `object` holds a live AXUIElementRef.
            let installed = unsafe {
                AXUIElementSetMessagingTimeout(object.as_ptr(), AX_MESSAGING_TIMEOUT_SECONDS)
            };
            (installed == K_AX_ERROR_SUCCESS).then_some(Self { object })
        }

        /// Retain a borrowed `AXUIElementRef` (e.g. an entry of a CFArray the
        /// caller still owns) and install its own messaging timeout.
        ///
        /// # Safety
        /// `ptr` must be a live `AXUIElementRef` borrowed from a container that
        /// outlives this call.
        unsafe fn retain(ptr: *const c_void) -> Option<Self> {
            if ptr.is_null() {
                return None;
            }
            // SAFETY: caller guarantees `ptr` is a live CF object.
            Self::adopt(unsafe { CFRetain(ptr) })
        }

        fn as_ptr(&self) -> *const c_void {
            self.object.as_ptr()
        }
    }

    /// Attribute-name CFStrings, created once per walk and borrowed as raw
    /// pointers by the context so attribute reads never re-allocate them.
    struct AttrStorage {
        role: CfOwned,
        subrole: CfOwned,
        title: CfOwned,
        value: CfOwned,
        children: CfOwned,
        windows: CfOwned,
        position: CfOwned,
        size: CfOwned,
        focused_window: CfOwned,
        focused_element: CfOwned,
        window_number: CfOwned,
    }

    impl AttrStorage {
        fn new() -> Option<Self> {
            Some(Self {
                role: cf_string(b"AXRole\0")?,
                subrole: cf_string(b"AXSubrole\0")?,
                title: cf_string(b"AXTitle\0")?,
                value: cf_string(b"AXValue\0")?,
                children: cf_string(b"AXChildren\0")?,
                windows: cf_string(b"AXWindows\0")?,
                position: cf_string(b"AXPosition\0")?,
                size: cf_string(b"AXSize\0")?,
                focused_window: cf_string(b"AXFocusedWindow\0")?,
                focused_element: cf_string(b"AXFocusedUIElement\0")?,
                window_number: cf_string(b"AXWindowNumber\0")?,
            })
        }

        fn keys(&self) -> AttrKeys {
            AttrKeys {
                role: self.role.as_ptr(),
                subrole: self.subrole.as_ptr(),
                title: self.title.as_ptr(),
                value: self.value.as_ptr(),
                children: self.children.as_ptr(),
                windows: self.windows.as_ptr(),
                position: self.position.as_ptr(),
                size: self.size.as_ptr(),
                focused_window: self.focused_window.as_ptr(),
                focused_element: self.focused_element.as_ptr(),
                window_number: self.window_number.as_ptr(),
            }
        }
    }

    /// Borrowed attribute-name pointers. `Copy` so a key can be read out of the
    /// context before a `&mut self` method call.
    #[derive(Clone, Copy)]
    struct AttrKeys {
        role: *const c_void,
        subrole: *const c_void,
        title: *const c_void,
        value: *const c_void,
        children: *const c_void,
        windows: *const c_void,
        position: *const c_void,
        size: *const c_void,
        focused_window: *const c_void,
        focused_element: *const c_void,
        window_number: *const c_void,
    }

    fn cf_string(bytes_with_nul: &'static [u8]) -> Option<CfOwned> {
        // SAFETY: `bytes_with_nul` is a static NUL-terminated ASCII literal.
        CfOwned::from_create(unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                bytes_with_nul.as_ptr().cast::<c_char>(),
                K_CF_STRING_ENCODING_UTF8,
            )
        })
    }

    #[derive(Clone, Copy, Debug)]
    enum WalkFailure {
        Stopped,
        Deadline,
        CircuitBreak,
        VisitCap,
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    struct Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    struct WalkContext {
        stop: Arc<AtomicBool>,
        is_shared: Arc<dyn Fn() -> bool + Send + Sync>,
        deadline: Instant,
        visited: usize,
        consecutive_timeouts: usize,
        keys: AttrKeys,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum WindowMatch {
        ExactIdentity,
        SameFrameFallback,
        None,
    }

    pub(super) fn window_match(
        ax_window_number: Option<i64>,
        same_frame: bool,
        wanted_window_id: u32,
    ) -> WindowMatch {
        if ax_window_number == Some(wanted_window_id as i64) {
            WindowMatch::ExactIdentity
        } else if ax_window_number.is_none() && same_frame {
            WindowMatch::SameFrameFallback
        } else {
            WindowMatch::None
        }
    }

    impl WalkContext {
        fn check(&self) -> Result<(), WalkFailure> {
            if self.stop.load(Ordering::SeqCst) || !(self.is_shared)() {
                Err(WalkFailure::Stopped)
            } else if Instant::now() >= self.deadline {
                Err(WalkFailure::Deadline)
            } else {
                Ok(())
            }
        }

        fn call<T, F>(&mut self, operation: F) -> Result<Option<T>, WalkFailure>
        where
            F: FnOnce() -> Result<T, i32>,
        {
            self.check()?;
            match operation() {
                Ok(value) => {
                    self.consecutive_timeouts = 0;
                    Ok(Some(value))
                }
                Err(error) if error == K_AX_ERROR_CANNOT_COMPLETE => {
                    self.consecutive_timeouts += 1;
                    if self.consecutive_timeouts >= 3 {
                        Err(WalkFailure::CircuitBreak)
                    } else {
                        Ok(None)
                    }
                }
                Err(_) => {
                    self.consecutive_timeouts = 0;
                    Ok(None)
                }
            }
        }

        fn string(
            &mut self,
            element: &AxElement,
            attribute: *const c_void,
        ) -> Result<Option<String>, WalkFailure> {
            let ptr = element.as_ptr();
            let raw = self.call(|| copy_attribute(ptr, attribute))?;
            let Some(raw) = raw else { return Ok(None) };
            // `AXValue` (and several other attributes) is a `CFString` only for
            // text-bearing roles — for extremely common controls (AXCheckBox,
            // AXSlider, AXProgressIndicator, AXScrollBar, ...) it is a CFNumber
            // or CFBoolean instead. Blindly wrapping an arbitrary CFTypeRef as
            // a CFString and reading it dispatches CFString's ObjC methods
            // against a differently-shaped object — anywhere from garbage text
            // (best case) to a debug-assert panic to an out-of-bounds read /
            // unrecognized-selector crash (release), none of which
            // `catch_unwind` can save since it is not a Rust panic. Reject
            // anything that is not actually a CFString before touching it.
            // SAFETY: `raw` is a live CF object owned by `CfOwned`.
            if unsafe { CFGetTypeID(raw.as_ptr()) } != unsafe { CFStringGetTypeID() } {
                return Ok(None);
            }
            Ok(Some(bounded_string_prefix(raw.as_ptr())))
        }

        fn rect(&mut self, element: &AxElement) -> Result<Option<Rect>, WalkFailure> {
            let ptr = element.as_ptr();
            let position_key = self.keys.position;
            let size_key = self.keys.size;
            let position = self.call(|| copy_ax_point(ptr, position_key))?;
            let size = self.call(|| copy_ax_size(ptr, size_key))?;
            Ok(position.zip(size).map(|(position, size)| Rect {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            }))
        }

        fn integer(
            &mut self,
            element: &AxElement,
            attribute: *const c_void,
        ) -> Result<Option<i64>, WalkFailure> {
            let ptr = element.as_ptr();
            let raw = self.call(|| copy_attribute(ptr, attribute))?;
            let Some(raw) = raw else { return Ok(None) };
            if unsafe { CFGetTypeID(raw.as_ptr()) } != unsafe { CFNumberGetTypeID() } {
                return Ok(None);
            }
            let mut value = 0i64;
            // SAFETY: type-checked as CFNumber and `value` is writable.
            let ok = unsafe {
                CFNumberGetValue(
                    raw.as_ptr(),
                    K_CF_NUMBER_SINT64,
                    &mut value as *mut i64 as *mut c_void,
                )
            };
            Ok(ok.then_some(value))
        }

        fn elements(
            &mut self,
            element: &AxElement,
            attribute: *const c_void,
        ) -> Result<Vec<AxElement>, WalkFailure> {
            self.check()?;
            // Cap materialization at the remaining visit budget rather than
            // retaining every child of a 50k-row table up front.
            let remaining = remaining_visit_budget(self.visited);
            if remaining == 0 {
                return Ok(Vec::new());
            }
            let ptr = element.as_ptr();
            Ok(self
                .call(|| copy_elements_limited(ptr, attribute, remaining))?
                .unwrap_or_default())
        }
    }

    fn copy_attribute(element: *const c_void, attribute: *const c_void) -> Result<CfOwned, i32> {
        let mut out: *const c_void = std::ptr::null();
        // SAFETY: `element` is a live AXUIElementRef, `attribute` a live
        // CFStringRef, and `out` a valid writable slot for the create-rule
        // result.
        let error = unsafe { AXUIElementCopyAttributeValue(element, attribute, &mut out) };
        if error != K_AX_ERROR_SUCCESS {
            return Err(error);
        }
        CfOwned::from_create(out).ok_or(K_AX_ERROR_NO_VALUE)
    }

    fn copy_ax_point(element: *const c_void, attribute: *const c_void) -> Result<CGPoint, i32> {
        let value = copy_attribute(element, attribute)?;
        // Type-check before unwrapping: an attribute that is not an AXValue at
        // all must never be handed to AXValueGetValue.
        // SAFETY: `value` is a live CF object.
        if unsafe { CFGetTypeID(value.as_ptr()) } != unsafe { AXValueGetTypeID() } {
            return Err(K_AX_ERROR_NO_VALUE);
        }
        let mut point = CGPoint::default();
        // SAFETY: `value` is an AXValueRef and `point` a valid CGPoint slot.
        let ok = unsafe {
            AXValueGetValue(
                value.as_ptr(),
                K_AX_VALUE_TYPE_CG_POINT,
                &mut point as *mut CGPoint as *mut c_void,
            )
        };
        ok.then_some(point).ok_or(K_AX_ERROR_NO_VALUE)
    }

    fn copy_ax_size(element: *const c_void, attribute: *const c_void) -> Result<CGSize, i32> {
        let value = copy_attribute(element, attribute)?;
        // SAFETY: `value` is a live CF object.
        if unsafe { CFGetTypeID(value.as_ptr()) } != unsafe { AXValueGetTypeID() } {
            return Err(K_AX_ERROR_NO_VALUE);
        }
        let mut size = CGSize::default();
        // SAFETY: `value` is an AXValueRef and `size` a valid CGSize slot.
        let ok = unsafe {
            AXValueGetValue(
                value.as_ptr(),
                K_AX_VALUE_TYPE_CG_SIZE,
                &mut size as *mut CGSize as *mut c_void,
            )
        };
        ok.then_some(size).ok_or(K_AX_ERROR_NO_VALUE)
    }

    fn copy_elements_limited(
        element: *const c_void,
        attribute: *const c_void,
        limit: usize,
    ) -> Result<Vec<AxElement>, i32> {
        let array = copy_attribute(element, attribute)?;
        // SAFETY: `array` is a live CF object.
        if unsafe { CFGetTypeID(array.as_ptr()) } != unsafe { CFArrayGetTypeID() } {
            return Ok(Vec::new());
        }
        // SAFETY: type-checked as a CFArray immediately above.
        let count = unsafe { CFArrayGetCount(array.as_ptr()) };
        let mut out = Vec::new();
        for index in 0..count {
            if out.len() >= limit {
                break;
            }
            // SAFETY: `index` is within the array's checked count.
            let value = unsafe { CFArrayGetValueAtIndex(array.as_ptr(), index) };
            if !is_ax_ui_element(value) {
                continue;
            }
            // SAFETY: `value` is borrowed from `array`, which is alive here.
            if let Some(child) = unsafe { AxElement::retain(value) } {
                out.push(child);
            }
        }
        Ok(out)
    }

    fn is_ax_ui_element(ptr: *const c_void) -> bool {
        // SAFETY: null is rejected first; otherwise `ptr` is a CF object
        // borrowed from a live container.
        !ptr.is_null() && unsafe { CFGetTypeID(ptr) } == unsafe { AXUIElementGetTypeID() }
    }

    /// Read at most `DIGEST_MAX_TEXT_CHARS` UTF-16 units of a CFString into a
    /// fixed stack buffer.
    ///
    /// CFString ranges are UTF-16 code units. Reading a bounded prefix avoids
    /// asking CFString to allocate storage for a multi-megabyte `AXValue` (a
    /// whole document's text) only to throw all but 200 characters away. The
    /// spare unit extends the read by one when the boundary lands inside a
    /// surrogate pair.
    fn bounded_string_prefix(value: *const c_void) -> String {
        // SAFETY: caller type-checked `value` as a CFStringRef.
        let total_units = unsafe { CFStringGetLength(value) }.max(0) as usize;
        let mut units = [0u16; DIGEST_MAX_TEXT_CHARS + 1];
        let mut read_units = total_units.min(DIGEST_MAX_TEXT_CHARS);
        if read_units > 0 {
            // SAFETY: the range is within `total_units` and the buffer holds
            // DIGEST_MAX_TEXT_CHARS + 1 units.
            unsafe {
                CFStringGetCharacters(
                    value,
                    CFRange {
                        location: 0,
                        length: read_units as isize,
                    },
                    units.as_mut_ptr(),
                );
            }
            if read_units < total_units && (0xD800..=0xDBFF).contains(&units[read_units - 1]) {
                read_units += 1;
                // SAFETY: `read_units <= DIGEST_MAX_TEXT_CHARS + 1` (the buffer
                // length) and `<= total_units`.
                unsafe {
                    CFStringGetCharacters(
                        value,
                        CFRange {
                            location: 0,
                            length: read_units as isize,
                        },
                        units.as_mut_ptr(),
                    );
                }
            }
        }
        let prefix = String::from_utf16_lossy(&units[..read_units]);
        if total_units > read_units {
            truncate_text_with_known_suffix(&prefix)
        } else {
            truncate_text(&prefix)
        }
    }

    /// Run the periodic digest walk on its own OS thread.
    ///
    /// Never inline this into the session's async select loop: AX calls block
    /// inside the system framework, which starves the WebSocket.
    pub(crate) fn spawn_digest_timer(
        window_id: u32,
        stop: Arc<AtomicBool>,
        is_shared: Arc<dyn Fn() -> bool + Send + Sync>,
        digest_tx: mpsc::Sender<DigestEvent>,
        state: Arc<Mutex<DigestSessionState>>,
    ) {
        std::thread::spawn(move || {
            let mut first = true;
            while !stop.load(Ordering::SeqCst) && is_shared() {
                if !first && !attempt_allowed(&state) {
                    break;
                }
                let snapshot = build_digest(window_id, &stop, &is_shared, &state);
                let event = if first {
                    first = false;
                    DigestEvent::First(snapshot)
                } else if let Some(snapshot) = snapshot {
                    DigestEvent::Refresh(snapshot)
                } else {
                    if !attempt_allowed(&state) {
                        break;
                    }
                    wait_for_next_refresh(&stop, &is_shared);
                    continue;
                };
                if digest_tx.blocking_send(event).is_err() {
                    break;
                }
                wait_for_next_refresh(&stop, &is_shared);
            }
        });
    }

    fn attempt_allowed(state: &Arc<Mutex<DigestSessionState>>) -> bool {
        digest_attempt_allowed(
            &state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    fn wait_for_next_refresh(
        stop: &Arc<AtomicBool>,
        is_shared: &Arc<dyn Fn() -> bool + Send + Sync>,
    ) {
        for _ in 0..DIGEST_REFRESH_INTERVAL_SECS * 10 {
            if stop.load(Ordering::SeqCst) || !is_shared() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn build_digest(
        window_id: u32,
        stop: &Arc<AtomicBool>,
        is_shared: &Arc<dyn Fn() -> bool + Send + Sync>,
        state: &Arc<Mutex<DigestSessionState>>,
    ) -> Option<DigestSnapshot> {
        if !is_shared() || !attempt_allowed(state) {
            return None;
        }
        let Some(pid) = cg::owner_pid_for_window_id(window_id) else {
            log_failure(state, "no owning process");
            return None;
        };
        let Some(cg_frame) = cg::frame_for_window_id(window_id) else {
            log_failure(state, "no current window frame");
            return None;
        };
        // SAFETY: `AXUIElementCreateApplication` follows the create rule; the
        // handle is adopted (and its messaging timeout installed) or dropped.
        let Some(application) = AxElement::adopt(unsafe { AXUIElementCreateApplication(pid) })
        else {
            log_failure(state, "no AX application");
            return None;
        };
        let Some(storage) = AttrStorage::new() else {
            log_failure(state, "AX attribute keys unavailable");
            return None;
        };
        let mut context = WalkContext {
            stop: stop.clone(),
            is_shared: is_shared.clone(),
            deadline: Instant::now() + Duration::from_millis(DIGEST_WALK_DEADLINE_MS),
            visited: 0,
            consecutive_timeouts: 0,
            keys: storage.keys(),
        };

        let windows_key = context.keys.windows;
        let windows = match context.elements(&application, windows_key) {
            Ok(windows) => windows,
            Err(failure) => {
                log_walk_failure(state, failure);
                return None;
            }
        };
        let window_number_key = context.keys.window_number;
        let mut exact_identity_match = None;
        let mut frame_match = None;
        for window in windows {
            if context.check().is_err() {
                log_failure(state, "window matching bound");
                return None;
            }
            let ax_window_number = match context.integer(&window, window_number_key) {
                Ok(number) => number,
                Err(failure) => {
                    log_walk_failure(state, failure);
                    return None;
                }
            };
            if window_match(ax_window_number, false, window_id) == WindowMatch::ExactIdentity {
                exact_identity_match = Some(window);
                break;
            }
            let frame = match context.rect(&window) {
                Ok(frame) => frame,
                Err(failure) => {
                    log_walk_failure(state, failure);
                    return None;
                }
            };
            match window_match(
                ax_window_number,
                frame.is_some_and(|frame| frame_matches(frame, cg_frame)),
                window_id,
            ) {
                WindowMatch::SameFrameFallback if frame_match.is_none() => {
                    frame_match = Some(window);
                }
                WindowMatch::ExactIdentity | WindowMatch::SameFrameFallback | WindowMatch::None => {}
            }
        }
        // AXWindowNumber is the stable CG window identity. Some applications
        // do not expose it, so same-process frame matching remains a fallback;
        // title-only matching is deliberately forbidden because a newly-opened
        // same-titled window is not the shared window.
        let Some(window) = exact_identity_match.or(frame_match) else {
            log_failure(state, "no matching AX window");
            return None;
        };
        let tree = match walk_tree(&mut context, &window, 0, false) {
            Ok(tree) => tree,
            Err(failure) => {
                log_walk_failure(state, failure);
                return None;
            }
        };
        let generation = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .next_generation;
        let serialized = serialize_digest(generation, &tree);
        if serialized.indexes.is_empty() {
            log_failure(state, "empty AX tree");
            return None;
        }
        let snapshot = commit_serialized(state, serialized);
        if let Some(snapshot) = &snapshot {
            // Length + hash only. Digest CONTENT is never logged.
            debug!(
                "ai_chat: AX digest ready (generation={}, bytes={}, hash={:016x})",
                snapshot.generation,
                snapshot.text.len(),
                snapshot.hash
            );
        }
        snapshot
    }

    fn walk_tree(
        context: &mut WalkContext,
        element: &AxElement,
        depth: usize,
        masked_ancestor: bool,
    ) -> Result<DigestNode, WalkFailure> {
        context.check()?;
        if context.visited >= DIGEST_MAX_NODES {
            return Err(WalkFailure::VisitCap);
        }
        context.visited += 1;
        let keys = context.keys;
        let role = context.string(element, keys.role)?;
        let subrole = context.string(element, keys.subrole)?;
        // Fail closed: an element whose role could not be read is treated as
        // masked, because we cannot prove it is not a secure field.
        let masked = masked_ancestor
            || role.is_none()
            || is_masked_input(role.as_deref().unwrap_or_default(), subrole.as_deref());
        let title = context.string(element, keys.title)?;
        // Defense in depth: a secure field's text never enters this process at
        // all — the pure serializer redacts it too, but this is the read that
        // would have exposed it.
        let value = if masked {
            None
        } else {
            context.string(element, keys.value)?
        };
        let frame = context.rect(element)?;
        let mut children = Vec::new();
        if depth < DIGEST_MAX_DEPTH {
            for child in context.elements(element, keys.children)? {
                context.check()?;
                children.push(walk_tree(context, &child, depth + 1, masked)?);
            }
        }
        Ok(DigestNode {
            role: role.unwrap_or_else(|| "AXUnknown".to_string()),
            subrole,
            title,
            value,
            // Global screen origin, carried so #658's window_click can turn a
            // cited `[n]` into a point and prove it is inside the shared
            // window. An unreadable position stays `None` and denies.
            origin: frame.map(|frame| (frame.x, frame.y)),
            width: frame.map_or(0.0, |frame| frame.width),
            height: frame.map_or(0.0, |frame| frame.height),
            children,
        })
    }

    /// Read one `AXUIElement`-valued attribute (e.g. `AXFocusedWindow`).
    fn copy_element(
        element: *const c_void,
        attribute: *const c_void,
    ) -> Result<Option<AxElement>, i32> {
        let raw = copy_attribute(element, attribute)?;
        if !is_ax_ui_element(raw.as_ptr()) {
            return Ok(None);
        }
        // SAFETY: `raw` owns a live AXUIElementRef for the duration of this
        // call; `retain` takes its own reference.
        Ok(unsafe { AxElement::retain(raw.as_ptr()) })
    }

    /// How long the focus probe may block inside the AX framework. Much shorter
    /// than a full walk: this runs on the execution path, immediately before an
    /// action, and a slow answer must fail closed rather than delay a click.
    const FOCUS_PROBE_DEADLINE_MS: u64 = 400;

    /// What the target app is focused on right now. Every failure yields the
    /// default (`window_matches: false`, no role), which
    /// [`focused_typing_refusal`] treats as a denial.
    pub(crate) fn focused_context(pid: i32, target: WindowFrame) -> FocusedContext {
        let mut out = FocusedContext::default();
        if pid <= 0 {
            return out;
        }
        // SAFETY: create-rule handle, adopted or dropped.
        let Some(application) = AxElement::adopt(unsafe { AXUIElementCreateApplication(pid) })
        else {
            return out;
        };
        let Some(storage) = AttrStorage::new() else {
            return out;
        };
        let mut context = WalkContext {
            stop: Arc::new(AtomicBool::new(false)),
            is_shared: Arc::new(|| true),
            deadline: Instant::now() + Duration::from_millis(FOCUS_PROBE_DEADLINE_MS),
            visited: 0,
            consecutive_timeouts: 0,
            keys: storage.keys(),
        };
        let keys = context.keys;

        let application_ptr = application.as_ptr();
        let focused_window = context
            .call(|| copy_element(application_ptr, keys.focused_window))
            .ok()
            .flatten()
            .flatten();
        if let Some(window) = focused_window {
            if let Ok(Some(rect)) = context.rect(&window) {
                out.window_matches = frame_matches(rect, target);
            }
        }

        let focused_element = context
            .call(|| copy_element(application_ptr, keys.focused_element))
            .ok()
            .flatten()
            .flatten();
        if let Some(element) = focused_element {
            out.role = context.string(&element, keys.role).ok().flatten();
            out.subrole = context.string(&element, keys.subrole).ok().flatten();
        }
        out
    }

    fn frame_matches(frame: Rect, target: WindowFrame) -> bool {
        (frame.x - target.x as f64).abs() <= FRAME_MATCH_TOLERANCE
            && (frame.y - target.y as f64).abs() <= FRAME_MATCH_TOLERANCE
            && (frame.width - target.width as f64).abs() <= FRAME_MATCH_TOLERANCE
            && (frame.height - target.height as f64).abs() <= FRAME_MATCH_TOLERANCE
    }

    fn log_failure(state: &Arc<Mutex<DigestSessionState>>, reason: &str) {
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.failure_logged {
            state.failure_logged = true;
            debug!("ai_chat: AX digest skipped ({reason})");
        }
    }

    fn log_walk_failure(state: &Arc<Mutex<DigestSessionState>>, failure: WalkFailure) {
        let reason = match failure {
            WalkFailure::Stopped => "session stopped",
            WalkFailure::Deadline => "walk deadline",
            WalkFailure::CircuitBreak => "AX timeout circuit breaker",
            WalkFailure::VisitCap => "walk visit cap",
        };
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(failure, WalkFailure::CircuitBreak) {
            trip_digest_circuit_breaker(&mut state);
        }
        if !state.failure_logged {
            state.failure_logged = true;
            debug!("ai_chat: AX digest skipped ({reason})");
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(unused_imports)] // see the module-level `dead_code` note (#656 wiring)
pub(crate) use native::{focused_context, spawn_digest_timer};

/// Off macOS there is no accessibility to read, so the probe reports the
/// all-denying default rather than pretending focus is fine.
#[cfg(not(target_os = "macos"))]
pub(crate) fn focused_context(
    _pid: i32,
    _target: crate::platform::cg::WindowFrame,
) -> FocusedContext {
    FocusedContext::default()
}

#[cfg(target_os = "windows")]
pub(crate) use native_windows::spawn_digest_timer;

/// Windows accessibility digest via UI Automation (parity with the macOS AX
/// walk). Same contract as the macOS native walk: runs on its own OS thread
/// (UIA calls can block), bounded by the shared deadline/node caps, feeds the
/// same `DigestEvent` channel, and produces the same generation-indexed
/// serialized text so `protocol.rs` / `session.rs` are platform-agnostic.
#[cfg(target_os = "windows")]
mod native_windows {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
        IUIAutomationValuePattern, UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId,
        UIA_ComboBoxControlTypeId, UIA_EditControlTypeId, UIA_GroupControlTypeId,
        UIA_HeaderItemControlTypeId, UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId,
        UIA_ListItemControlTypeId, UIA_MenuControlTypeId, UIA_MenuItemControlTypeId,
        UIA_PaneControlTypeId, UIA_ProgressBarControlTypeId, UIA_RadioButtonControlTypeId,
        UIA_ScrollBarControlTypeId, UIA_SeparatorControlTypeId, UIA_SliderControlTypeId,
        UIA_StatusBarControlTypeId, UIA_TabControlTypeId, UIA_TabItemControlTypeId,
        UIA_TextControlTypeId, UIA_ToolBarControlTypeId, UIA_ToolTipControlTypeId,
        UIA_TreeControlTypeId, UIA_TreeItemControlTypeId, UIA_ValuePatternId,
        UIA_WindowControlTypeId, UIA_CONTROLTYPE_ID,
    };
    use windows::core::Interface;

    /// Walk budget shared down the recursion (mirrors the macOS `WalkContext`).
    struct WalkContext {
        stop: Arc<AtomicBool>,
        is_shared: Arc<dyn Fn() -> bool + Send + Sync>,
        deadline: std::time::Instant,
        visited: usize,
    }

    impl WalkContext {
        fn check(&self) -> bool {
            !self.stop.load(Ordering::SeqCst)
                && (self.is_shared)()
                && std::time::Instant::now() < self.deadline
        }
    }

    /// Run the periodic UIA walk on its own thread (UIA can block; never
    /// inline into the session's async select loop — same rule as macOS AX).
    pub(crate) fn spawn_digest_timer(
        window_id: u32,
        stop: Arc<AtomicBool>,
        is_shared: Arc<dyn Fn() -> bool + Send + Sync>,
        digest_tx: mpsc::Sender<DigestEvent>,
        state: Arc<Mutex<DigestSessionState>>,
    ) {
        thread::spawn(move || {
            // UIA is a COM apartment; initialize it on this thread.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut first = true;
            while !stop.load(Ordering::SeqCst) && is_shared() {
                if !first && !attempt_allowed(&state) {
                    break;
                }
                let snapshot = build_digest(window_id, &stop, &is_shared, &state);
                let event = if first {
                    first = false;
                    DigestEvent::First(snapshot)
                } else if let Some(snapshot) = snapshot {
                    DigestEvent::Refresh(snapshot)
                } else {
                    if !attempt_allowed(&state) {
                        break;
                    }
                    wait_for_next_refresh(&stop, &is_shared);
                    continue;
                };
                if digest_tx.blocking_send(event).is_err() {
                    break;
                }
                wait_for_next_refresh(&stop, &is_shared);
            }
            unsafe {
                let _ = CoUninitialize();
            }
        });
    }

    fn attempt_allowed(state: &Arc<Mutex<DigestSessionState>>) -> bool {
        digest_attempt_allowed(&state.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn log_failure(state: &Arc<Mutex<DigestSessionState>>, reason: &str) {
        let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.failure_logged {
            state.failure_logged = true;
            log::debug!("ai_chat: UIA digest failed: {reason}");
        }
    }

    fn wait_for_next_refresh(
        stop: &Arc<AtomicBool>,
        is_shared: &Arc<dyn Fn() -> bool + Send + Sync>,
    ) {
        for _ in 0..DIGEST_REFRESH_INTERVAL_SECS * 10 {
            if stop.load(Ordering::SeqCst) || !is_shared() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn build_digest(
        window_id: u32,
        stop: &Arc<AtomicBool>,
        is_shared: &Arc<dyn Fn() -> bool + Send + Sync>,
        state: &Arc<Mutex<DigestSessionState>>,
    ) -> Option<DigestSnapshot> {
        if !is_shared() || !attempt_allowed(state) {
            return None;
        }
        let target = crate::windows_capture_target::resolve(window_id).ok()?;
        let hwnd = HWND(target.raw_handle() as *mut core::ffi::c_void);
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL) }.ok()?;
        let Ok(element) = (unsafe { automation.ElementFromHandle(hwnd) }) else {
            log_failure(state, "no UIA element for window");
            return None;
        };
        let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
            log_failure(state, "no UIA control-view walker");
            return None;
        };
        let mut context = WalkContext {
            stop: stop.clone(),
            is_shared: is_shared.clone(),
            deadline: Instant::now() + Duration::from_millis(DIGEST_WALK_DEADLINE_MS),
            visited: 0,
        };
        let Some(tree) = walk_tree(&walker, &element, &mut context, 0, false) else {
            log_failure(state, "UIA walk produced no tree");
            return None;
        };
        let generation = state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_generation;
        let serialized = serialize_digest(generation, &tree);
        if serialized.indexes.is_empty() {
            log_failure(state, "empty UIA tree");
            return None;
        }
        let snapshot = commit_serialized(state, serialized);
        if let Some(snapshot) = &snapshot {
            log::debug!(
                "ai_chat: UIA digest ready (generation={}, bytes={}, hash={:016x})",
                snapshot.generation,
                snapshot.text.len(),
                snapshot.hash
            );
        }
        snapshot
    }

    fn walk_tree(
        walker: &IUIAutomationTreeWalker,
        element: &IUIAutomationElement,
        context: &mut WalkContext,
        depth: usize,
        masked_ancestor: bool,
    ) -> Option<DigestNode> {
        if !context.check() || context.visited >= DIGEST_MAX_NODES {
            return None;
        }
        context.visited += 1;

        let role = unsafe { element.CurrentControlType() }
            .ok()
            .and_then(control_type_name)
            .unwrap_or_else(|| "UIAUnknown".to_string());
        // UIA marks secure edit fields with IsPassword (the ControlType is
        // still Edit, so the shared role-string redaction cannot see it).
        let is_password = unsafe { element.CurrentIsPassword() }
            .ok()
            .is_some_and(|b| b.as_bool());
        let masked = masked_ancestor || is_password;
        let title = unsafe { element.CurrentName() }.ok().map(|b| b.to_string());
        // Defense in depth: a secure field's text never enters this process
        // at all (same read-time redaction as the macOS walk).
        let value = if masked {
            None
        } else {
            current_value(element)
        };
        let rect = unsafe { element.CurrentBoundingRectangle() }.ok();
        let origin = rect.map(|r| (r.left as f64, r.top as f64));
        let (width, height) = rect
            .map(|r| ((r.right - r.left) as f64, (r.bottom - r.top) as f64))
            .unwrap_or((0.0, 0.0));

        let mut children = Vec::new();
        if depth < DIGEST_MAX_DEPTH {
            let mut child = unsafe { walker.GetFirstChildElement(element) }.ok();
            while let Some(c) = child {
                if !context.check() {
                    break;
                }
                if let Some(node) = walk_tree(walker, &c, context, depth + 1, masked) {
                    children.push(node);
                }
                child = unsafe { walker.GetNextSiblingElement(&c) }.ok();
            }
        }

        Some(DigestNode {
            role,
            subrole: None,
            title,
            value,
            origin,
            width,
            height,
            children,
        })
    }

    /// The UIA Value pattern's text, when the element exposes one (Edit,
    /// ComboBox, ScrollBar, …). `None` when the pattern is absent or empty.
    fn current_value(element: &IUIAutomationElement) -> Option<String> {
        let pattern = unsafe { element.GetCurrentPattern(UIA_ValuePatternId) }.ok()?;
        let value_pattern = pattern.cast::<IUIAutomationValuePattern>().ok()?;
        unsafe { value_pattern.CurrentValue() }
            .ok()
            .map(|b| b.to_string())
    }

    /// Friendly role name for a UIA control-type id (the model reads these
    /// like the macOS AX role strings; the exact taxonomy is not contractual).
    #[allow(non_upper_case_globals)]
    fn control_type_name(id: UIA_CONTROLTYPE_ID) -> Option<String> {
        Some(
            match id {
                UIA_ButtonControlTypeId => "Button",
                UIA_CheckBoxControlTypeId => "CheckBox",
                UIA_ComboBoxControlTypeId => "ComboBox",
                UIA_EditControlTypeId => "Edit",
                UIA_GroupControlTypeId => "Group",
                UIA_HeaderItemControlTypeId => "HeaderItem",
                UIA_HyperlinkControlTypeId => "Hyperlink",
                UIA_ImageControlTypeId => "Image",
                UIA_ListItemControlTypeId => "ListItem",
                UIA_MenuControlTypeId => "Menu",
                UIA_MenuItemControlTypeId => "MenuItem",
                UIA_PaneControlTypeId => "Pane",
                UIA_ProgressBarControlTypeId => "ProgressBar",
                UIA_RadioButtonControlTypeId => "RadioButton",
                UIA_ScrollBarControlTypeId => "ScrollBar",
                UIA_SeparatorControlTypeId => "Separator",
                UIA_SliderControlTypeId => "Slider",
                UIA_StatusBarControlTypeId => "StatusBar",
                UIA_TabControlTypeId => "Tab",
                UIA_TabItemControlTypeId => "TabItem",
                UIA_TextControlTypeId => "Text",
                UIA_ToolBarControlTypeId => "ToolBar",
                UIA_ToolTipControlTypeId => "ToolTip",
                UIA_TreeControlTypeId => "Tree",
                UIA_TreeItemControlTypeId => "TreeItem",
                UIA_WindowControlTypeId => "Window",
                _ => return None,
            }
            .to_string(),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn common_control_types_map_to_friendly_names() {
            assert_eq!(control_type_name(UIA_ButtonControlTypeId), Some("Button".into()));
            assert_eq!(control_type_name(UIA_EditControlTypeId), Some("Edit".into()));
            assert_eq!(control_type_name(UIA_TextControlTypeId), Some("Text".into()));
            assert_eq!(control_type_name(UIA_ListItemControlTypeId), Some("ListItem".into()));
            // An unmapped id falls back to None -> the walker emits UIAUnknown.
            assert_eq!(control_type_name(UIA_CONTROLTYPE_ID(99999)), None);
        }

        #[test]
        fn uia_style_tree_serializes_through_the_shared_generation_indexed_format() {
            // A tree exactly as the UIA walk produces (roles from
            // `control_type_name`, UIA Name in title, ValuePattern text in
            // value) must serialize through the SHARED `serialize_digest` to
            // the same generation-indexed text the macOS walk produces — so
            // `protocol.rs`/`session.rs` never see a platform difference.
            let root = DigestNode {
                role: "Window".to_string(),
                subrole: None,
                title: Some("Untitled - Notepad".to_string()),
                value: None,
                origin: Some((0.0, 0.0)),
                width: 800.0,
                height: 600.0,
                children: vec![DigestNode {
                    role: "Edit".to_string(),
                    subrole: None,
                    title: Some("Text editor".to_string()),
                    value: Some("hello world".to_string()),
                    origin: Some((10.0, 40.0)),
                    width: 780.0,
                    height: 400.0,
                    children: vec![],
                }],
            };
            let serialized = serialize_digest(4, &root);
            assert!(serialized.text.contains("generation 4"));
            assert!(serialized
                .text
                .contains("Edit \"Text editor\" value=\"hello world\""));
            assert!(serialized
                .text
                .contains("Window \"Untitled - Notepad\""));
            assert!(serialized.indexes.contains_key(&0));
        }
    }
}

/// Other non-macOS/non-Windows targets have no accessibility to read, so the
/// digest timer is a no-op (the pure serializer still compiles for them).
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn spawn_digest_timer(
    _window_id: u32,
    _stop: Arc<std::sync::atomic::AtomicBool>,
    _is_shared: Arc<dyn Fn() -> bool + Send + Sync>,
    _digest_tx: tokio::sync::mpsc::Sender<DigestEvent>,
    _state: Arc<Mutex<DigestSessionState>>,
) {
    // No accessibility to read off macOS/Windows; keep the session compiling.
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use std::sync::atomic::AtomicBool;
    #[cfg(target_os = "macos")]
    use std::time::Duration;
    #[cfg(target_os = "macos")]
    use tokio::sync::mpsc;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn digest_timer_never_walks_or_emits_after_publication_ends() {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::channel(1);
        let state = Arc::new(Mutex::new(DigestSessionState::default()));
        // An invalid id makes the pre-fix walker return First(None) promptly;
        // the publication gate must prevent even that real timer path from
        // entering build_digest or emitting an event.
        spawn_digest_timer(u32::MAX, stop, Arc::new(|| false), tx, state);
        // A correctly-gated timer never enters its loop body at all, so it
        // drops `tx` immediately and the channel closes -- `recv()` resolves
        // to `Ok(None)` right away rather than hanging until the timeout.
        // Only `Ok(Some(event))` (a real digest event) is the failure this
        // test exists to catch; a bare `is_err()` timeout check is the wrong
        // shape for that and fails even when the gate works correctly.
        let outcome = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            !matches!(outcome, Ok(Some(_))),
            "the AX timer produced a digest event after is_shared became false: {outcome:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ax_window_selection_requires_identity_or_same_process_frame() {
        use super::native::{window_match, WindowMatch};

        assert_eq!(
            window_match(Some(41), true, 42),
            WindowMatch::None,
            "a known-different window must not win even with the same title or frame"
        );
        assert_eq!(
            window_match(Some(42), false, 42),
            WindowMatch::ExactIdentity
        );
        assert_eq!(
            window_match(None, true, 42),
            WindowMatch::SameFrameFallback
        );
    }

    fn container(children: Vec<DigestNode>) -> DigestNode {
        DigestNode {
            role: "AXGroup".to_string(),
            subrole: None,
            title: None,
            value: None,
            origin: Some((0.0, 0.0)),
            width: 500.0,
            height: 500.0,
            children,
        }
    }

    fn node(
        role: &str,
        subrole: Option<&str>,
        title: Option<&str>,
        value: Option<&str>,
        children: Vec<DigestNode>,
    ) -> DigestNode {
        DigestNode {
            role: role.to_string(),
            subrole: subrole.map(str::to_string),
            title: title.map(str::to_string),
            value: value.map(str::to_string),
            origin: Some((0.0, 0.0)),
            width: 200.0,
            height: 24.0,
            children,
        }
    }

    #[test]
    fn serializer_is_stable_and_tracks_ancestor_paths() {
        let root = container(vec![DigestNode::leaf(
            "AXStaticText",
            Some("Hello"),
            Some("World"),
        )]);
        let a = serialize_digest(7, &root);
        let b = serialize_digest(7, &root);
        assert_eq!(a, b);
        assert!(a.text.contains("generation 7"));
        assert!(a
            .text
            .contains("[0] AXStaticText \"Hello\" value=\"World\""));
        assert_eq!(a.indexes[&0].ancestor_path, vec!["AXGroup"]);
    }

    #[test]
    fn bidi_override_in_window_content_cannot_spoof_the_approval_card_or_smuggle_into_the_model() {
        // A malicious/compromised shared window can set an element's
        // accessible title to arbitrary text. U+202E (RIGHT-TO-LEFT OVERRIDE)
        // reverses the DISPLAY order of everything after it without changing
        // the underlying bytes -- exactly the class of attack this repo's
        // security audit found reaching both the model's context (via the
        // text digest) and the human-facing approval card (via
        // DigestIndex.title -> control_gate::describe_element, verbatim
        // string interpolation, no further escaping downstream).
        let hostile_title = "Cancel\u{202E}etelеD"; // displays reversed in an RTL-aware renderer
        let root = container(vec![DigestNode::leaf(
            "AXButton",
            Some(hostile_title),
            None,
        )]);
        let output = serialize_digest(3, &root);

        // 1. The model-facing digest text must not carry the raw override
        // character -- it must not be able to smuggle bidi/invisible
        // instructions into what the model reads.
        assert!(
            !output.text.contains('\u{202E}'),
            "bidi override character reached the model's digest text: {:?}",
            output.text
        );

        // 2. DigestIndex.title -- the exact field describe_element() renders
        // verbatim on the approval card -- must also be sanitized. Before
        // this fix it was only truncated, never escaped, so the raw override
        // character reached the human-facing card unmodified.
        let index = &output.indexes[&0];
        assert!(
            index
                .title
                .as_deref()
                .is_some_and(|title| !title.contains('\u{202E}')),
            "DigestIndex.title still carries the raw bidi override: {:?}",
            index.title
        );

        // 3. Drive the REAL approval-card rendering path, not just the
        // sanitizer in isolation -- this is what a user actually reads
        // before approving an AI-driven action.
        let card_text = crate::ai_chat::control_gate::describe_element(index);
        assert!(
            !card_text.contains('\u{202E}'),
            "approval card text still carries the raw bidi override: {card_text:?}"
        );
    }

    #[test]
    fn bidi_override_in_the_role_field_cannot_spoof_the_approval_card_either() {
        // describe_element() concatenates role and title into ONE rendered
        // string: `format!("{role} \"{title}\"")`. An override planted in
        // role is never closed within role's own text -- the only way to end
        // a bidi override is another Cf pop character, which escape_text
        // strips from title -- so an unescaped role can hijack the display
        // order of the ALREADY-escaped title that follows it in the same
        // string. Sanitizing title alone is not sufficient; role needs the
        // identical filter (caught by adversarial review after the first
        // pass of this fix escaped title but left role untouched).
        let hostile_role = "AXButton\u{202E}deraeD"; // opens an override, never closes it
        let root = container(vec![DigestNode::leaf(
            hostile_role,
            Some("Delete"),
            None,
        )]);
        let output = serialize_digest(4, &root);

        assert!(
            !output.text.contains('\u{202E}'),
            "bidi override in role reached the model's digest text: {:?}",
            output.text
        );

        let index = &output.indexes[&0];
        assert!(
            !index.role.contains('\u{202E}'),
            "DigestIndex.role still carries the raw bidi override: {:?}",
            index.role
        );

        let card_text = crate::ai_chat::control_gate::describe_element(index);
        assert!(
            !card_text.contains('\u{202E}'),
            "approval card text still carries a bidi override planted via role: {card_text:?}"
        );
    }

    #[test]
    fn serializer_truncates_node_text_and_hard_caps_bytes() {
        let long = "x".repeat(500);
        let many = (0..200)
            .map(|i| DigestNode::leaf("AXStaticText", Some(&format!("{i}-{long}")), None))
            .collect();
        let output = serialize_digest(1, &container(many));
        assert!(output.text.len() <= DIGEST_BYTE_BUDGET);
        assert!(output.text.contains(TRUNCATED_MARKER));
        assert!(output.text.lines().any(|line| line.contains('…')));
    }

    #[test]
    fn text_truncation_has_exact_character_boundaries() {
        let exact = "x".repeat(DIGEST_MAX_TEXT_CHARS);
        assert_eq!(truncate_text(&exact).chars().count(), DIGEST_MAX_TEXT_CHARS);
        assert!(!truncate_text(&exact).contains('…'));

        let over = "x".repeat(DIGEST_MAX_TEXT_CHARS + 1);
        let truncated = truncate_text(&over);
        assert_eq!(truncated.chars().count(), DIGEST_MAX_TEXT_CHARS);
        assert!(truncated.ends_with('…'));

        let unicode = "😀".repeat(DIGEST_MAX_TEXT_CHARS + 1);
        let truncated = truncate_text(&unicode);
        assert_eq!(truncated.chars().count(), DIGEST_MAX_TEXT_CHARS);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn known_suffix_truncation_marks_a_bounded_read() {
        // The native reader knows it stopped early, so it appends the ellipsis
        // without re-checking the (already-cut) prefix length.
        let prefix = "y".repeat(DIGEST_MAX_TEXT_CHARS);
        let truncated = truncate_text_with_known_suffix(&prefix);
        assert_eq!(truncated.chars().count(), DIGEST_MAX_TEXT_CHARS);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn child_budget_is_reserved_before_materializing_children() {
        assert_eq!(remaining_visit_budget(0), DIGEST_MAX_NODES);
        assert_eq!(remaining_visit_budget(1), DIGEST_MAX_NODES - 1);
        assert_eq!(remaining_visit_budget(DIGEST_MAX_NODES), 0);
        assert_eq!(remaining_visit_budget(DIGEST_MAX_NODES + 1), 0);
    }

    #[test]
    fn serializer_skips_zero_size_and_empty_containers() {
        let mut zero_size = DigestNode::leaf("AXStaticText", Some("hidden"), None);
        zero_size.width = 0.0;
        let output = serialize_digest(
            3,
            &container(vec![
                zero_size,
                DigestNode {
                    role: "AXGroup".to_string(),
                    subrole: None,
                    title: None,
                    value: None,
                    origin: Some((0.0, 0.0)),
                    width: 100.0,
                    height: 100.0,
                    children: Vec::new(),
                },
                DigestNode::leaf("AXStaticText", Some("visible"), None),
            ]),
        );
        assert!(!output.text.contains("hidden"));
        assert!(!output.text.contains("AXGroup"));
        assert!(output.text.contains("visible"));
    }

    #[test]
    fn serializer_enforces_depth_and_node_caps() {
        let mut deep = DigestNode::leaf("AXStaticText", Some("too deep"), None);
        for _ in 0..=DIGEST_MAX_DEPTH {
            deep = container(vec![deep]);
        }
        let deep_output = serialize_digest(4, &deep);
        assert!(!deep_output.text.contains("too deep"));

        let many: Vec<DigestNode> = (0..(DIGEST_MAX_NODES + 100))
            .map(|i| DigestNode::leaf("AXStaticText", Some(&format!("node-{i}")), None))
            .collect();
        let many_output = serialize_digest(5, &container(many));
        assert!(many_output.indexes.len() <= DIGEST_MAX_NODES);
        assert!(many_output.text.contains(TRUNCATED_MARKER));
    }

    #[test]
    fn serializer_prioritizes_text_over_window_chrome() {
        let chrome: Vec<DigestNode> = (0..200)
            .map(|i| DigestNode::leaf("AXButton", Some(&format!("chrome-{i}")), None))
            .collect();
        let root = container(
            chrome
                .into_iter()
                .chain([DigestNode::leaf(
                    "AXStaticText",
                    Some("Important content"),
                    None,
                )])
                .collect(),
        );
        let output = serialize_digest(2, &root);
        assert!(output.text.contains("Important content"));
        assert!(!output.text.contains("AXGroup"));
    }

    #[test]
    fn serializer_escapes_quotes_backslashes_and_control_characters() {
        let root = container(vec![DigestNode::leaf(
            "AXStaticText",
            Some("say \"hi\" \\ now"),
            Some("line1\nline2\ttabbed\u{7}bell"),
        )]);
        let output = serialize_digest(0, &root);
        let line = output
            .text
            .lines()
            .find(|line| line.contains("AXStaticText"))
            .expect("the text node should be admitted");
        assert!(line.contains("say \\\"hi\\\" \\\\ now"), "line: {line}");
        assert!(line.contains("line1 line2 tabbed bell"), "line: {line}");
        // Exactly one line: no raw newline survived into the output.
        assert_eq!(
            output.text.lines().filter(|l| l.contains("line1")).count(),
            1
        );
        assert!(!output.text.contains('\u{7}'));
    }

    #[test]
    fn hash_is_stable_for_identical_serialized_text() {
        let first = hash_digest("same");
        assert_eq!(first, hash_digest("same"));
        assert_ne!(first, hash_digest("different"));
    }

    #[test]
    fn hash_excludes_the_generation_header_line() {
        let root = container(vec![DigestNode::leaf("AXStaticText", Some("body"), None)]);
        let first = serialize_digest(1, &root);
        let second = serialize_digest(99, &root);
        assert_ne!(
            first.text, second.text,
            "the header should carry the generation"
        );
        assert_eq!(
            hash_digest(&first.text),
            hash_digest(&second.text),
            "a bumped generation alone must not force a resend"
        );
    }

    #[test]
    fn session_retains_generations_and_limits_refreshes() {
        let state = Arc::new(Mutex::new(DigestSessionState::default()));
        let make = |generation: u64| {
            serialize_digest(
                generation,
                &DigestNode::leaf("AXTextField", Some("x"), None),
            )
        };
        assert!(commit_serialized(&state, make(0)).is_some());
        assert!(commit_serialized(&state, make(1)).is_none());
        for n in 0..DIGEST_REFRESH_BUDGET {
            let mut node = DigestNode::leaf("AXTextField", Some("x"), None);
            node.value = Some(n.to_string());
            assert!(commit_serialized(&state, serialize_digest(n as u64 + 2, &node)).is_some());
        }
        let mut state_guard = state.lock().unwrap();
        assert_eq!(state_guard.refreshes_sent, DIGEST_REFRESH_BUDGET);
        assert!(state_guard.generations.len() >= 2);
        assert!(state_guard
            .generations
            .iter()
            .all(|generation| !generation.indexes.is_empty()));
        state_guard.generations.clear();
    }

    #[test]
    fn refresh_budget_stops_further_resends() {
        let state = Arc::new(Mutex::new(DigestSessionState::default()));
        let distinct = |n: usize| {
            let mut node = DigestNode::leaf("AXStaticText", Some("row"), None);
            node.value = Some(format!("v{n}"));
            serialize_digest(n as u64, &node)
        };
        // Initial send does not consume the refresh budget.
        assert!(commit_serialized(&state, distinct(0)).is_some());
        for n in 1..=DIGEST_REFRESH_BUDGET as usize {
            assert!(
                commit_serialized(&state, distinct(n)).is_some(),
                "refresh {n} should be within budget"
            );
        }
        assert!(
            commit_serialized(&state, distinct(99)).is_none(),
            "the refresh past the budget must be dropped even though it changed"
        );
    }

    #[test]
    fn generation_history_is_capped() {
        let state = Arc::new(Mutex::new(DigestSessionState::default()));
        for n in 0..(DIGEST_GENERATION_HISTORY + 10) {
            let mut node = DigestNode::leaf("AXStaticText", Some("row"), None);
            node.value = Some(format!("v{n}"));
            let _ = commit_serialized(&state, serialize_digest(n as u64, &node));
        }
        let guard = state.lock().unwrap();
        assert_eq!(guard.generations.len(), DIGEST_GENERATION_HISTORY);
        // The oldest generations were evicted, the newest retained.
        assert!(lookup_index(&guard, 0, 0).is_none());
        let newest = guard.next_generation - 1;
        assert!(lookup_index(&guard, newest, 0).is_some());
    }

    #[test]
    fn an_indexed_element_carries_the_geometry_a_click_needs() {
        // #658's window_click turns `[n]` into a point; without the rect on the
        // index there is nothing to prove lies inside the shared window.
        let mut leaf = DigestNode::leaf("AXButton", Some("Send"), None);
        leaf.origin = Some((40.0, 60.0));
        leaf.width = 80.0;
        leaf.height = 24.0;
        let output = serialize_digest(1, &container(vec![leaf]));
        let rect = output.indexes[&0].rect.expect("indexed element needs a rect");
        assert_eq!(rect.center(), (80.0, 72.0));
        assert!(rect.is_usable());
    }

    #[test]
    fn an_element_without_a_position_has_no_rect_rather_than_a_guessed_one() {
        let mut leaf = DigestNode::leaf("AXButton", Some("Send"), None);
        leaf.origin = None;
        let output = serialize_digest(1, &container(vec![leaf]));
        assert!(output.indexes[&0].rect.is_none());
    }

    #[test]
    fn typing_is_refused_unless_focus_is_known_and_in_the_shared_window() {
        // The all-denying default is what every native failure returns.
        assert_eq!(
            focused_typing_refusal(&FocusedContext::default()),
            Some("focused_window_mismatch")
        );
        assert_eq!(
            focused_typing_refusal(&FocusedContext {
                role: None,
                subrole: None,
                window_matches: true,
            }),
            Some("focused_role_unknown")
        );
        assert_eq!(
            focused_typing_refusal(&FocusedContext {
                role: Some("AXTextField".into()),
                subrole: Some("AXSecureTextField".into()),
                window_matches: true,
            }),
            Some("focused_field_secure")
        );
        assert_eq!(
            focused_typing_refusal(&FocusedContext {
                role: Some("AXTextField".into()),
                subrole: None,
                window_matches: true,
            }),
            None
        );
    }

    #[test]
    fn the_newest_snapshot_is_retained_for_a_stale_citation_to_be_answered_with() {
        let state = Arc::new(Mutex::new(DigestSessionState::default()));
        let node = container(vec![DigestNode::leaf("AXStaticText", Some("hi"), None)]);
        let _ = commit_serialized(&state, serialize_digest(0, &node));
        let text = state.lock().unwrap().last_text.clone();
        assert!(text.is_some_and(|text| text.contains("[0] AXStaticText")));
    }

    #[test]
    fn indexed_lookup_uses_the_referenced_generation() {
        let mut state = DigestSessionState::default();
        state.generations.push_back(DigestGeneration {
            generation: 7,
            hash: 0,
            indexes: HashMap::from([(
                0,
                DigestIndex {
                    role: "AXButton".into(),
                    title: Some("Old".into()),
                    ancestor_path: Vec::new(),
                    rect: None,
                },
            )]),
        });
        state.generations.push_back(DigestGeneration {
            generation: 8,
            hash: 0,
            indexes: HashMap::from([(
                0,
                DigestIndex {
                    role: "AXButton".into(),
                    title: Some("New".into()),
                    ancestor_path: Vec::new(),
                    rect: None,
                },
            )]),
        });

        assert_eq!(
            lookup_index(&state, 7, 0).unwrap().title.as_deref(),
            Some("Old")
        );
        assert_eq!(
            lookup_index(&state, 8, 0).unwrap().title.as_deref(),
            Some("New")
        );
        assert!(lookup_index(&state, 7, 1).is_none());
        assert!(lookup_index(&state, 6, 0).is_none());
    }

    #[test]
    fn circuit_breaker_remains_open_for_the_session() {
        let mut state = DigestSessionState::default();
        assert!(digest_attempt_allowed(&state));

        trip_digest_circuit_breaker(&mut state);

        assert!(!digest_attempt_allowed(&state));
        assert!(state.circuit_breaker_open);
    }

    // --- Petal: secure fields are never digested -------------------------

    #[test]
    fn masked_input_detection_covers_roles_and_subroles() {
        assert!(is_masked_input("AXSecureTextField", None));
        assert!(is_masked_input("AXSecureTextArea", None));
        assert!(is_masked_input("AXPasswordField", None));
        // Web content: plain role, secure subrole.
        assert!(is_masked_input("AXTextField", Some("AXSecureTextField")));
        // Case and whitespace insensitive.
        assert!(is_masked_input("  axsecuretextfield  ", None));
        assert!(is_masked_input("AXTextField", Some("AXPasswordFieldRole")));
        // Fail-closed heuristic for roles we have not catalogued.
        assert!(is_masked_input("AXCustomSecureEntry", None));

        assert!(!is_masked_input("AXTextField", None));
        assert!(!is_masked_input("AXStaticText", Some("AXSearchField")));
        assert!(!is_masked_input("AXButton", None));
    }

    #[test]
    fn secure_text_field_value_is_never_serialized() {
        let root = container(vec![
            node(
                "AXSecureTextField",
                None,
                Some("Password"),
                Some("hunter2-super-secret"),
                Vec::new(),
            ),
            DigestNode::leaf("AXStaticText", Some("Sign in"), None),
        ]);
        let output = serialize_digest(0, &root);
        assert!(!output.text.contains("hunter2-super-secret"));
        assert!(!output.text.contains("value="));
        // The field itself is still described — only its value is withheld.
        assert!(output.text.contains("AXSecureTextField \"Password\""));
        assert!(output.text.contains("Sign in"));
    }

    #[test]
    fn secure_subrole_on_a_plain_role_is_also_redacted() {
        let root = container(vec![node(
            "AXTextField",
            Some("AXSecureTextField"),
            Some("Passphrase"),
            Some("correct-horse-battery"),
            Vec::new(),
        )]);
        let output = serialize_digest(0, &root);
        assert!(!output.text.contains("correct-horse-battery"));
        assert!(!output.text.contains("value="));
        assert!(output.text.contains("AXTextField \"Passphrase\""));
    }

    #[test]
    fn descendants_of_a_secure_field_are_redacted_too() {
        let root = container(vec![node(
            "AXSecureTextField",
            None,
            Some("Password"),
            None,
            vec![node(
                "AXStaticText",
                None,
                None,
                Some("leaked-inner-secret"),
                Vec::new(),
            )],
        )]);
        let output = serialize_digest(0, &root);
        assert!(
            !output.text.contains("leaked-inner-secret"),
            "digest: {}",
            output.text
        );
    }

    #[test]
    fn redaction_does_not_affect_sibling_values() {
        let root = container(vec![
            node(
                "AXSecureTextField",
                None,
                Some("Password"),
                Some("do-not-leak"),
                Vec::new(),
            ),
            node(
                "AXTextField",
                None,
                Some("Email"),
                Some("user@example.com"),
                Vec::new(),
            ),
        ]);
        let output = serialize_digest(0, &root);
        assert!(!output.text.contains("do-not-leak"));
        assert!(output.text.contains("value=\"user@example.com\""));
    }

    #[test]
    fn redacted_value_does_not_inflate_admission_score() {
        // A secure field whose value is huge must not win budget it no longer
        // needs: its score is computed from the redacted (empty) value.
        let secret = "s".repeat(DIGEST_MAX_TEXT_CHARS);
        let masked = node("AXSecureTextField", None, None, Some(&secret), Vec::new());
        let plain = node("AXTextField", None, None, Some(&secret), Vec::new());
        let masked_output = serialize_digest(0, &container(vec![masked]));
        let plain_output = serialize_digest(0, &container(vec![plain]));
        assert!(!masked_output.text.contains(&secret));
        assert!(plain_output.text.contains(&secret));
        // With no title and no admissible value the masked node has no text at
        // all, so it never reaches the output.
        assert!(masked_output.indexes.is_empty());
    }
}
