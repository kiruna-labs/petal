//! Local room-metadata layer (SPEC.md §4.6 "Persistent rooms + join flow";
//! SPEC.md §3's "Room/presence service" note).
//!
//! ## Honest scope boundary -- read this before extending it
//!
//! SPEC.md §3 calls for "a small stateful service holding persistent room
//! records, membership, and 'who's sharing what,' rid[ing] on the managed
//! platform's room service + a thin metadata layer of your own." That
//! describes a real **cross-machine, multi-user, always-on** service -- e.g.
//! a small backend all Petal clients talk to, so "does `eng-sync` exist" and
//! "who's in it right now" mean the same thing on every machine.
//!
//! This module is **NOT that service.** It is a single-machine, single-user,
//! on-disk stand-in scoped to this dev-stage app (no shared backend exists
//! anywhere in this codebase yet, and building one is a much bigger task than
//! this one). What it genuinely IS:
//!
//! - **Real local persistence.** Room records (name, `created_at`, `open`
//!   knock-vs-open setting) are written to a JSON file under this app's Tauri
//!   `app_data_dir()` (`rooms.json`) via `list_rooms`/`create_room`. This
//!   survives an app restart -- confirmed live, see CLAUDE.md's verification
//!   notes for this task. It is NOT an in-memory `Vec` dressed up as a
//!   backend, and NOT another hardcoded mock array like `mockRooms.ts` was.
//! - **A durable LiveKit room name derived from the local record.** Joining a
//!   locally-known room connects to a real LiveKit room whose name is derived
//!   deterministically from the local record's id (see `livekit_room_name`),
//!   not a literal `"petal-dev-room"` constant. LiveKit's *own* rooms are
//!   still ephemeral (they exist only while someone is joined and disappear
//!   server-side once empty) -- the durability SPEC.md asks for lives in
//!   *this* local record, which persists independent of whether the LiveKit
//!   room currently has anyone in it. That's the literal gap SPEC.md §3
//!   flags ("thin metadata layer of your own" on top of the platform's own
//!   ephemeral room service) -- this module is exactly that thin layer, at
//!   single-machine scope.
//!
//! What this module explicitly does **NOT** provide, and would need a real
//! backend to provide:
//! - **No cross-machine sync.** Two Petal installs on two different Macs
//!   each have their own independent `rooms.json` -- creating a room on one
//!   machine does not make it appear on another. A real shared room
//!   directory needs a server both clients talk to; this is a local cache/
//!   stand-in for that, not a preview of it with sync missing by accident.
//! - **No durable cross-machine membership/presence record.** "Who's in a
//!   room right now" is answered live from LiveKit's own room-participant
//!   list (`RoomEvent::ParticipantConnected`/`Disconnected`, surfaced via
//!   `presence.rs`) for whichever machine is asking, not from a
//!   server-of-record multiple machines could consistently query.
//! - **No auth/identity directory.** "Knock to join" (`open: false`) records
//!   the setting per-room but only enforces a trivial local waiting-state
//!   stand-in (see `join_room` in `session.rs`) -- there's no real
//!   approval/notify-the-room-owner flow, which would need a real multi-user
//!   backend to mean anything (there's no second machine to approve from).
//!
//! In short: this is real local durability for a single-user dev machine,
//! deliberately NOT a multi-user shared room/presence service -- that
//! service is a real backend-deployment task for later, per SPEC.md's own
//! phasing.
//!
//! ## Why a JSON file, not `rusqlite`
//!
//! Checked `Cargo.lock`/`Cargo.toml` first: no SQL/embedded-DB crate
//! (`rusqlite`, `sqlx`, etc.) is a dependency anywhere in this workspace
//! today. The data shape here is a small, infrequently-written list of room
//! records (create/list/join -- no queries, no joins, no concurrent-writer
//! contention beyond this one process's own mutex) -- exactly the case where
//! adding a new embedded-SQL dependency (and its own linking surface, on a
//! codebase that has already fought hard for a clean link -- see
//! `transport/mod.rs`'s M0 blocker writeup) buys nothing a
//! `serde_json`-serialized `Vec<RoomRecord>` behind a `Mutex` + atomic file
//! write doesn't already give us at this scale. `serde`/`serde_json` are
//! already dependencies. Revisit if/when this needs real queries, indexes, or
//! multi-process concurrent writers -- none of which apply yet.

use crate::sync_ext::MutexExt;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum RoomsError {
    #[error("failed to write rooms file: {0}")]
    Write(String),
    #[error("room name must not be empty")]
    EmptyName,
    #[error("room not found: {0}")]
    NotFound(String),
    #[error("autotest room ownership is invalid: {0}")]
    AutotestOwnership(String),
}

/// One durable room record. `name`/`slug` are the full join credential
/// (`<human-slug>-<128-bit hex capability>`), not just a human label. The
/// optional `display_name` keeps the friendly label local while the credential
/// remains the accountless authorization boundary (#126).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomRecord {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub slug: String,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_joined_ms: Option<u64>,
    /// "Knock to join" vs "open" (SPEC.md §4.6). `true` = open (default for
    /// internal eng rooms, per spec). `false` = knock-to-join -- enforced
    /// today only as a trivial local "waiting for approval" stand-in (see
    /// `session::join_room`), not a real approval workflow.
    pub open: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomOccupancyParticipant {
    pub identity: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomOccupancy {
    pub room_name: String,
    pub livekit_room: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub available: bool,
    pub participants: Vec<RoomOccupancyParticipant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    // Backend status fields: populated from `POST /api/rooms/status` for the
    // credentials this machine presented. Never a join credential (#83) and
    // never a cross-machine discovery source (the public directory is gone).
    /// Backend human display label (e.g. "eng-sync"). Not the join credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Backend join credential (`<human-slug>-<hex>`), i.e. `room_name` here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occupancy: Option<usize>,
}

/// On-disk shape of `rooms.json`. Wrapped in a struct (rather than a bare
/// top-level array) so future fields (e.g. a schema version) can be added
/// without breaking the file format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutotestRoomOwnership {
    /// Opaque QA identity from `PETAL_AUTOTEST_ROOM`, never a display-name
    /// lookup key.
    qa_key: String,
    /// Full persisted room capability this QA identity is allowed to reuse.
    canonical_credential: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomsFile {
    #[serde(default)]
    rooms: Vec<RoomRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    autotest_ownership: Vec<AutotestRoomOwnership>,
}

/// Tauri-managed state: an in-memory cache of the on-disk `rooms.json`,
/// guarded by a mutex (single-process, single-user -- see module doc comment
/// on why this doesn't need more than that). Every mutating call
/// (`create_room`) re-persists the whole file after updating the in-memory
/// copy, so the two never diverge.
pub struct RoomsState {
    path: PathBuf,
    file: Mutex<RoomsFile>,
    // A corrupt ownership section must never look like a fresh machine to the
    // autotest resolver, which would otherwise overwrite it while creating a
    // new mapping. A missing section is a valid pre-#609 legacy file.
    autotest_ownership_load_error: Option<String>,
}

impl RoomsState {
    /// Load `rooms.json` from `app_data_dir` if it exists, or start with an
    /// empty room list (first launch) -- either way this never fails loudly
    /// for a missing file, only for a genuinely unreadable/corrupt one being
    /// impossible to recover, in which case we still start empty rather than
    /// block app startup on a metadata-store problem.
    pub fn load(app_data_dir: PathBuf) -> Self {
        let path = app_data_dir.join("rooms.json");
        let (mut file, autotest_ownership_load_error) = match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let ownership_error = match serde_json::from_str::<serde_json::Value>(&contents) {
                    Ok(serde_json::Value::Object(object)) => match object.get("autotestOwnership") {
                        Some(serde_json::Value::Array(_)) => None,
                        Some(_) => Some("autotestOwnership must be an array".to_string()),
                        None => None,
                    },
                    Ok(_) => Some("rooms file must be a JSON object".to_string()),
                    Err(error) => Some(format!("rooms file is invalid JSON: {error}")),
                };
                let (file, schema_error) = match serde_json::from_str::<RoomsFile>(&contents) {
                    Ok(file) => (file, None),
                    Err(error) => {
                        log::warn!("rooms: rooms.json exists but failed to parse ({error}), starting empty");
                        (RoomsFile::default(), Some(format!("rooms file schema is invalid: {error}")))
                    }
                };
                (file, ownership_error.or(schema_error))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (RoomsFile::default(), None),
            Err(e) => {
                log::warn!("rooms: failed to read rooms.json ({e}), starting empty");
                (RoomsFile::default(), Some(format!("rooms file could not be read: {e}")))
            }
        };
        let (rooms, migrated) = normalize_room_records(file.rooms);
        file.rooms = rooms;
        if migrated && autotest_ownership_load_error.is_none() {
            match persist_rooms_to_path(&path, &file) {
                Ok(()) => log::info!(
                    "rooms: migrated persisted room names/slugs in {}",
                    path.display()
                ),
                Err(e) => log::warn!("rooms: failed to persist room migration: {e}"),
            }
        }
        log::info!(
            "rooms: loaded {} persisted room(s) from {}",
            file.rooms.len(),
            path.display()
        );
        Self {
            path,
            file: Mutex::new(file),
            autotest_ownership_load_error,
        }
    }

    fn persist(&self, file: &RoomsFile) -> Result<(), RoomsError> {
        persist_rooms_to_path(&self.path, file)
    }

    pub fn list(&self) -> Vec<RoomRecord> {
        let guard = self.file.lock_unpoisoned();
        dedupe_room_records(&guard.rooms).0
    }

    pub fn reset_local(&self) -> Result<(), RoomsError> {
        let mut guard = self.file.lock_unpoisoned();
        guard.rooms.clear();
        guard.autotest_ownership.clear();
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(RoomsError::Write(format!(
                "removing {}: {e}",
                self.path.display()
            ))),
        }
    }

    pub fn find(&self, name: &str) -> Option<RoomRecord> {
        let credential = normalize_room_credential(name)?;
        self.file
            .lock_unpoisoned()
            .rooms
            .iter()
            .find(|r| normalize_room_credential(&r.name).as_deref() == Some(credential.as_str()))
            .cloned()
    }

    /// Create a new durable room record. If `name` is already a full
    /// credential (from an invite link), persist that exact capability.
    /// Otherwise generate a new unguessable credential with the human slug as
    /// its label prefix.
    pub fn create(&self, name: &str, open: bool) -> Result<RoomRecord, RoomsError> {
        self.create_with_display(name, open, None)
    }

    pub fn create_with_display(
        &self,
        name: &str,
        open: bool,
        display_name: Option<&str>,
    ) -> Result<RoomRecord, RoomsError> {
        let mut guard = self.file.lock_unpoisoned();
        let cleaned_display = display_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        // A bare credential that already names a KNOWN room (e.g. the app's
        // own "rename this room" flow, which touches a room by its internal
        // name rather than re-typing its access code) is just a rename/touch
        // of that existing record -- it needs no new access code at all, so
        // this must run before room_credential_for_input's stricter Case-2
        // rejection below, which correctly rejects a bare credential the
        // caller has never actually joined by its real access code (#421).
        if let Some(raw_credential) = normalize_room_credential(name) {
            if let Some(idx) = guard.rooms.iter().position(|r| {
                normalize_room_credential(&r.name).as_deref() == Some(raw_credential.as_str())
            }) {
                guard.rooms[idx].last_joined_ms = Some(now_ms());
                if let Some(display) = cleaned_display {
                    guard.rooms[idx].display_name = Some(display);
                }
                let record = guard.rooms[idx].clone();
                self.persist(&guard)?;
                return Ok(record);
            }
        }
        let existing_codes: Vec<String> = guard
            .rooms
            .iter()
            .filter_map(|room| room.access_code.clone())
            .collect();
        let (credential, label, access_code) =
            room_credential_for_input(name, &existing_codes).ok_or(RoomsError::EmptyName)?;
        let joined_at_ms = now_ms();

        if let Some(idx) = guard.rooms.iter().position(|r| {
            normalize_room_credential(&r.name).as_deref() == Some(credential.as_str())
        }) {
            guard.rooms[idx].last_joined_ms = Some(joined_at_ms);
            // A previous paste-join could have persisted this credential with
            // a fabricated access code. A later join by the real code must
            // repair that record before it can be shown or shared again.
            if guard.rooms[idx].access_code.as_deref() != Some(access_code.as_str()) {
                guard.rooms[idx].access_code = Some(access_code.clone());
            }
            if let Some(display) = cleaned_display {
                guard.rooms[idx].display_name = Some(display);
            }
            let record = guard.rooms[idx].clone();
            self.persist(&guard)?;
            return Ok(record);
        }

        let record = RoomRecord {
            id: new_room_id(),
            name: credential.clone(),
            access_code: Some(access_code),
            // Prefer an explicit display name; else the typed name's slug label
            // (Some only for name input). A blank/access-code create has label
            // None, so it stores no display name and the UI shows the "Petal
            // meeting" default instead of the generic "room" slug (#42).
            display_name: cleaned_display.or(label),
            slug: credential,
            created_at_ms: joined_at_ms,
            last_joined_ms: Some(joined_at_ms),
            open,
        };
        guard.rooms.push(record.clone());
        self.persist(&guard)?;
        Ok(record)
    }

    /// Resolve the one durable room capability owned by an autotest QA key.
    ///
    /// This intentionally does not use a room label as an identity.  A first
    /// opt-in creates and persists a dedicated mapping; later launches may
    /// reuse only that exact mapping.  A missing, stale, duplicate, or
    /// conflicting mapping is an error rather than a guess at a user room.
    pub fn resolve_autotest_room(
        &self,
        qa_key: &str,
        allow_fresh_room: bool,
    ) -> Result<RoomRecord, RoomsError> {
        let qa_key = qa_key.trim();
        if qa_key.is_empty() {
            return Err(RoomsError::AutotestOwnership(
                "PETAL_AUTOTEST_ROOM must contain a QA key".to_string(),
            ));
        }
        if normalize_access_code(qa_key).is_some() || normalize_room_credential(qa_key).is_some() {
            return Err(RoomsError::AutotestOwnership(
                "PETAL_AUTOTEST_ROOM must be an opaque QA key, not a room credential or access code"
                    .to_string(),
            ));
        }
        if let Some(error) = &self.autotest_ownership_load_error {
            return Err(RoomsError::AutotestOwnership(format!(
                "cannot resolve QA ownership from this rooms file: {error}"
            )));
        }

        let mut guard = self.file.lock_unpoisoned();
        let matching: Vec<&AutotestRoomOwnership> = guard
            .autotest_ownership
            .iter()
            .filter(|ownership| ownership.qa_key == qa_key)
            .collect();
        match matching.as_slice() {
            [ownership] => {
                let credential = normalize_room_credential(&ownership.canonical_credential)
                    .filter(|normalized| normalized == &ownership.canonical_credential)
                    .ok_or_else(|| {
                        RoomsError::AutotestOwnership(format!(
                            "QA key '{qa_key}' has a non-canonical room credential"
                        ))
                    })?;
                if guard
                    .autotest_ownership
                    .iter()
                    .filter(|candidate| candidate.canonical_credential == credential)
                    .count()
                    != 1
                {
                    return Err(RoomsError::AutotestOwnership(format!(
                        "QA key '{qa_key}' shares a room capability with another ownership record"
                    )));
                }
                let matching_rooms: Vec<usize> = guard
                    .rooms
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, room)| {
                        (normalize_room_credential(&room.name).as_deref()
                            == Some(credential.as_str()))
                        .then_some(idx)
                    })
                    .collect();
                let [idx] = matching_rooms.as_slice() else {
                    return Err(RoomsError::AutotestOwnership(format!(
                        "QA key '{qa_key}' maps to a missing or ambiguous room record"
                    )));
                };
                guard.rooms[*idx].last_joined_ms = Some(now_ms());
                let record = guard.rooms[*idx].clone();
                self.persist(&guard)?;
                Ok(record)
            }
            [] => {
                if !allow_fresh_room {
                    return Err(RoomsError::AutotestOwnership(format!(
                        "QA key '{qa_key}' has no ownership record; set PETAL_AUTOTEST_FRESH_ROOM=1 only to create a fresh dedicated test room"
                    )));
                }
                // This is conflict detection only.  It never selects or
                // modifies a user room by its display label.
                if guard
                    .rooms
                    .iter()
                    .any(|room| {
                        room.display_name
                            .as_deref()
                            .and_then(canonical_room_slug)
                            == canonical_room_slug(qa_key)
                    })
                {
                    return Err(RoomsError::AutotestOwnership(format!(
                        "QA key '{qa_key}' conflicts with an existing room label"
                    )));
                }
                let existing_codes: Vec<String> = guard
                    .rooms
                    .iter()
                    .filter_map(|room| room.access_code.clone())
                    .collect();
                let access_code = generate_access_code(&existing_codes)?;
                let credential = internal_credential_for_access_code(&access_code).ok_or_else(|| {
                    RoomsError::AutotestOwnership(
                        "could not derive a fresh autotest room capability".to_string(),
                    )
                })?;
                let record = RoomRecord {
                    id: new_room_id(),
                    name: credential.clone(),
                    access_code: Some(access_code),
                    display_name: Some("Autotest room".to_string()),
                    slug: credential.clone(),
                    created_at_ms: now_ms(),
                    last_joined_ms: Some(now_ms()),
                    open: true,
                };
                guard.rooms.push(record.clone());
                guard.autotest_ownership.push(AutotestRoomOwnership {
                    qa_key: qa_key.to_string(),
                    canonical_credential: credential,
                });
                self.persist(&guard)?;
                Ok(record)
            }
            _ => Err(RoomsError::AutotestOwnership(format!(
                "QA key '{qa_key}' has multiple ownership records"
            ))),
        }
    }

    /// Set or clear a local-only display label for a room without changing
    /// the access code (`name`) or canonical slug used for LiveKit.
    pub fn rename_display(
        &self,
        id_or_code: &str,
        display_name: Option<&str>,
    ) -> Result<RoomRecord, RoomsError> {
        let query = id_or_code.trim();
        if query.is_empty() {
            return Err(RoomsError::EmptyName);
        }
        let cleaned_display = display_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);

        let mut guard = self.file.lock_unpoisoned();
        let Some(idx) = guard
            .rooms
            .iter()
            .position(|room| room_matches_id_or_code(room, query))
        else {
            return Err(RoomsError::NotFound(query.to_string()));
        };

        guard.rooms[idx].display_name = cleaned_display;
        let record = guard.rooms[idx].clone();
        self.persist(&guard)?;
        Ok(record)
    }

    /// Remove one local durable room record from this machine. This is a
    /// local-only "forget saved room" operation: it does not delete a backend
    /// room or affect other machines that know the same credential.
    pub fn forget(&self, id_or_code: &str) -> Result<RoomRecord, RoomsError> {
        let query = id_or_code.trim();
        if query.is_empty() {
            return Err(RoomsError::EmptyName);
        }

        let mut guard = self.file.lock_unpoisoned();
        let Some(idx) = guard
            .rooms
            .iter()
            .position(|room| room_matches_id_or_code(room, query))
        else {
            return Err(RoomsError::NotFound(query.to_string()));
        };

        let removed = guard.rooms.remove(idx);
        self.persist(&guard)?;
        Ok(removed)
    }
}

/// Derive the durable LiveKit room name from a local room record.
///
/// **This is derived from the room's human NAME, not its local `id`, and that
/// is load-bearing for multi-participant to work at all.** The LiveKit room a
/// client connects to must be a deterministic function of the *shared* room
/// identity every participant knows (the human name / meeting code) -- NOT of
/// anything machine-local. The earlier implementation derived it from
/// `record.id` (a per-machine `timestamp-counter`), which meant two different
/// machines creating "eng-sync" produced two *different* LiveKit rooms and
/// could never meet -- defeating the entire point of the app. It also meant
/// the browser test harness (which joins the meeting code verbatim) landed in
/// a third room. Deriving from the normalized name makes any client -- native
/// on machine A, native on machine B, or the web harness -- that joins the
/// same name land in the same LiveKit room. The web harness derives the
/// identical `petal-room-<slug>` string from the typed code (see
/// `web-harness/src/meetingCode.ts::livekitRoomName`), so the two stay in
/// lockstep; change one and you must change the other.
///
/// The `petal-room-` prefix keeps LiveKit-side room listings/logs legible as
/// belonging to this app (vs. some other LiveKit tenant use).
///
/// Tradeoff vs. the old id-based scheme: renaming a room WOULD now change its
/// LiveKit room name (no rename UI exists today, so this is theoretical). That
/// is the correct tradeoff -- a rename that didn't change the room everyone
/// joins by that name would be the surprising behavior; cross-machine
/// meet-in-the-same-room is the property that actually matters.
pub fn livekit_room_name(record: &RoomRecord) -> String {
    format!("petal-room-{}", room_credential(record))
}

/// The canonical credential -> LiveKit-room-name mapping, callable with just a
/// room code. A full credential maps to `petal-room-<credential>`. Bare human
/// labels fail closed: callers that need labels must first create/persist a
/// room credential instead of deriving a guessable LiveKit room (#86).
pub fn livekit_room_name_for(name: &str) -> String {
    let room =
        normalize_room_credential(name).expect("room credential must include a capability suffix");
    format!("petal-room-{room}")
}

fn canonical_room_slug(name: &str) -> Option<String> {
    if name.trim().chars().any(|ch| ch.is_ascii_alphanumeric()) {
        Some(slugify(name))
    } else {
        None
    }
}

pub fn normalize_room_credential(code: &str) -> Option<String> {
    let normalized = code.trim().to_ascii_lowercase();
    let suffix = normalized.strip_prefix("room-")?;
    if suffix.len() != 32 || !suffix.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    Some(normalized)
}

pub fn room_label_from_credential(code: &str) -> Option<String> {
    let _ = normalize_room_credential(code)?;
    None
}

pub fn normalize_access_code(input: &str) -> Option<String> {
    let compact: String = input
        .trim()
        .chars()
        .filter(|ch| *ch != '-')
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    if compact.len() != 10 || !compact.chars().all(|ch| ch.is_ascii_lowercase()) {
        return None;
    }
    Some(format!(
        "{}-{}-{}",
        &compact[0..3],
        &compact[3..7],
        &compact[7..10]
    ))
}

fn generate_access_code(existing: &[String]) -> Result<String, RoomsError> {
    let mut bytes = [0u8; 10];
    for _ in 0..100 {
        getrandom::fill(&mut bytes)
            .map_err(|e| RoomsError::Write(format!("generating room access code: {e}")))?;
        let letters: String = bytes
            .iter()
            .map(|b| char::from(ACCESS_CODE_ALPHABET[usize::from(*b) % ACCESS_CODE_ALPHABET.len()]))
            .collect();
        let code = format!("{}-{}-{}", &letters[0..3], &letters[3..7], &letters[7..10]);
        if !existing
            .iter()
            .any(|value| normalize_access_code(value).as_deref() == Some(&code))
        {
            return Ok(code);
        }
    }
    Err(RoomsError::Write(
        "could not generate a unique access code".to_string(),
    ))
}

const ACCESS_CODE_ALPHABET: &[u8] = b"abcdefghjkmnopqrstuvwxyz";

pub fn internal_credential_for_access_code(access_code: &str) -> Option<String> {
    let code = normalize_access_code(access_code)?;
    Some(format!("room-{:032x}", fnv1a128(code.as_bytes())))
}

const FNV_128_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_128_PRIME: u128 = 0x0000000001000000000000000000013b;

fn fnv1a128(bytes: &[u8]) -> u128 {
    let mut hash = FNV_128_OFFSET;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(FNV_128_PRIME);
    }
    hash
}

fn public_room_id_for_livekit_room(livekit_room: &str) -> String {
    let hash = fnv1a128(livekit_room.as_bytes());
    format!("room_{hash:032x}")
}

/// Returns `(credential, label, access_code)`. `label` is `None` when the input
/// carried no human-meaningful name (a bare access code or credential) — the
/// caller must NOT invent a generic "room" display name from it (#42); it's
/// `Some(slug)` only when the user typed an actual name.
fn room_credential_for_input(
    input: &str,
    existing_access_codes: &[String],
) -> Option<(String, Option<String>, String)> {
    if let Some(access_code) = normalize_access_code(input) {
        let credential = internal_credential_for_access_code(&access_code)?;
        return Some((credential, None, access_code));
    }
    // A bare internal credential has no recoverable access code. Never invent
    // a second live capability for it; callers must supply the real code.
    if normalize_room_credential(input).is_some() {
        return None;
    }
    let label = canonical_room_slug(input)?;
    let access_code = generate_access_code(existing_access_codes).ok()?;
    let credential = internal_credential_for_access_code(&access_code)?;
    Some((credential, Some(label), access_code))
}

fn display_label_from_legacy_room_identity(value: &str) -> Option<String> {
    if normalize_access_code(value).is_some() || normalize_room_credential(value).is_some() {
        return None;
    }

    // Pre-access-code public credentials were shaped like
    // `<human-slug>-<32hex>`. Preserve only the human label during migration;
    // the hex suffix was technical join material and must not be shown (#42).
    let trimmed = value.trim();
    let label_source = trimmed
        .rsplit_once('-')
        .filter(|(_, suffix)| suffix.len() == 32 && suffix.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(|(label, _)| label)
        .unwrap_or(trimmed);
    let label = canonical_room_slug(label_source)?;
    if label == "room" {
        None
    } else {
        Some(label)
    }
}

fn room_credential(record: &RoomRecord) -> String {
    normalize_room_credential(&record.name)
        .or_else(|| normalize_room_credential(&record.slug))
        .unwrap_or_else(|| {
            // Legacy pre-#126 records had only a guessable slug. Migrations
            // rewrite them on load; this fallback keeps pure unit fixtures
            // from producing a bare `petal-room-`.
            "room-00000000000000000000000000000000".to_string()
        })
}

fn room_matches_id_or_code(room: &RoomRecord, query: &str) -> bool {
    if room.id == query {
        return true;
    }
    if normalize_access_code(query).is_some_and(|access_code| {
        room.access_code
            .as_deref()
            .and_then(normalize_access_code)
            .as_deref()
            == Some(access_code.as_str())
    }) {
        return true;
    }
    normalize_room_credential(query).is_some_and(|credential| {
        normalize_room_credential(&room.name).as_deref() == Some(credential.as_str())
            || normalize_room_credential(&room.slug).as_deref() == Some(credential.as_str())
    })
}

fn normalize_room_records(rooms: Vec<RoomRecord>) -> (Vec<RoomRecord>, bool) {
    let original_len = rooms.len();
    let mut changed = false;

    // Step 1: collapse duplicates keyed on whatever identity the raw record
    // already carries (an existing access code, an old credential, or as a
    // last resort the literal name) BEFORE minting any new access code.
    // Minting first would let two records that used to share one identity
    // diverge onto two different fresh codes and never merge.
    let mut by_legacy_key: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<RoomRecord> = Vec::with_capacity(original_len);
    for room in rooms {
        let legacy_key = room
            .access_code
            .as_deref()
            .and_then(normalize_access_code)
            .or_else(|| normalize_room_credential(&room.name))
            .or_else(|| normalize_room_credential(&room.slug))
            .unwrap_or_else(|| room.name.clone());
        if let Some(&idx) = by_legacy_key.get(&legacy_key) {
            changed = true;
            if room.created_at_ms < deduped[idx].created_at_ms {
                deduped[idx] = room;
            }
            continue;
        }
        by_legacy_key.insert(legacy_key, deduped.len());
        deduped.push(room);
    }
    changed |= deduped.len() != original_len;

    // Step 2: assign a real access code + derived internal credential to
    // each surviving room -- reuse an existing valid code, adopt the raw
    // name/slug if it already happens to look like one, or mint a fresh one.
    let mut normalized = Vec::with_capacity(deduped.len());
    for mut room in deduped {
        let room_credential =
            normalize_room_credential(&room.name).or_else(|| normalize_room_credential(&room.slug));
        let mut stored_access_code = room
            .access_code
            .as_deref()
            .and_then(normalize_access_code)
            .or_else(|| normalize_access_code(&room.name))
            .or_else(|| normalize_access_code(&room.slug));

        // Case 2 used to retain a correct internal credential while pairing it
        // with an unrelated random code. The real code cannot be recovered
        // from the one-way credential hash, so discard the poisoned hint. A
        // subsequent join by the real code adopts it in create_with_display.
        if let (Some(credential), Some(access_code)) =
            (room_credential.as_deref(), stored_access_code.as_deref())
        {
            if internal_credential_for_access_code(access_code).as_deref() != Some(credential) {
                stored_access_code = None;
                if room.access_code.is_some() {
                    room.access_code = None;
                    changed = true;
                }
            }
        }

        let access_code = stored_access_code.or_else(|| {
            if room_credential.is_some() {
                None
            } else {
                generate_access_code(
                    &normalized
                        .iter()
                        .filter_map(|r: &RoomRecord| r.access_code.clone())
                        .collect::<Vec<_>>(),
                )
                .ok()
            }
        });
        if room.access_code != access_code {
            room.access_code = access_code.clone();
            changed = true;
        }
        let credential = access_code
            .as_deref()
            .and_then(internal_credential_for_access_code)
            .or(room_credential)
            .unwrap_or_else(|| "room-00000000000000000000000000000000".to_string());
        if room.display_name.is_none() {
            room.display_name = display_label_from_legacy_room_identity(&room.name)
                .or_else(|| display_label_from_legacy_room_identity(&room.slug));
            changed = true;
        }
        if room.name != credential {
            room.name = credential.clone();
            changed = true;
        }
        if room.slug != credential {
            room.slug = credential;
            changed = true;
        }
        normalized.push(room);
    }

    (normalized, changed)
}

fn dedupe_room_records(rooms: &[RoomRecord]) -> (Vec<RoomRecord>, bool) {
    let mut by_credential: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<RoomRecord> = Vec::with_capacity(rooms.len());
    let mut changed = false;

    for room in rooms {
        let credential = normalize_room_credential(&room.name)
            .or_else(|| normalize_room_credential(&room.slug))
            .unwrap_or_else(|| room.name.clone());
        if let Some(&idx) = by_credential.get(&credential) {
            changed = true;
            if room.created_at_ms < out[idx].created_at_ms {
                let mut replacement = room.clone();
                replacement.slug = credential.clone();
                replacement.name = credential.clone();
                out[idx] = replacement;
            }
            continue;
        }

        let mut normalized = room.clone();
        if normalized.slug != credential || normalized.name != credential {
            changed = true;
            normalized.slug = credential.clone();
            normalized.name = credential.clone();
        }
        by_credential.insert(credential, out.len());
        out.push(normalized);
    }

    (out, changed)
}

fn persist_rooms_to_path(path: &Path, file: &RoomsFile) -> Result<(), RoomsError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| RoomsError::Write(format!("creating {}: {e}", parent.display())))?;
    }
    let contents =
        serde_json::to_string_pretty(file).map_err(|e| RoomsError::Write(e.to_string()))?;

    // Write-to-temp-then-rename so a crash mid-write can't leave
    // `rooms.json` truncated/corrupt -- the rename is atomic on the same
    // filesystem, which `app_data_dir`'s temp file (same directory) is.
    let tmp_path = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp_path)
            .map_err(|e| RoomsError::Write(format!("creating {}: {e}", tmp_path.display())))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| RoomsError::Write(e.to_string()))?;
    }
    std::fs::rename(&tmp_path, path).map_err(|e| RoomsError::Write(e.to_string()))?;
    Ok(())
}

/// Normalize an arbitrary human room name / meeting code into a stable,
/// LiveKit-safe slug: lowercase, and every run of non-`[a-z0-9]` characters
/// collapsed to a single `-`, with leading/trailing `-` trimmed. So
/// "Design Review", "design-review", and "  design   review  " all map to the
/// same `design-review` -- i.e. trivially-different typings of the same room
/// name still meet. Falls back to `"room"` if the input has no alphanumerics
/// at all (so the derived name is never a bare `petal-room-`).
// LOCKSTEP: room slug/LiveKit-name behavior is documented in docs/CONTRACTS.md.
// Keep this file in sync with backend/lib/slug.ts and web-harness/src/meetingCode.ts.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "room".to_string()
    } else {
        trimmed.to_string()
    }
}

fn new_room_id() -> String {
    // No `uuid` crate dependency exists in this workspace yet, and adding one
    // for a single random-id call would be the same "new dependency for a
    // tiny need" tradeoff the module doc comment already reasons through for
    // rusqlite. A timestamp + process-local counter is unique enough for a
    // single-user local store where ids are only ever generated by this one
    // process (never merged with another machine's ids -- see the module doc
    // comment's cross-machine-sync scope boundary).
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:x}-{:x}", now_ms(), n)
}

use crate::time_util::now_ms;

// =============================================================================
// Tauri commands
// =============================================================================

/// List every durable room record known to this machine (SPEC.md §4.6).
/// Presence (who's actually in each room right now) is NOT part of this
/// record -- see `presence.rs`'s `room_presence`/`presence-update` for the
/// live membership view, kept separate since presence is inherently
/// ephemeral/live while this list is durable/at-rest.
#[tauri::command]
pub fn list_rooms(state: tauri::State<'_, RoomsState>) -> Vec<RoomRecord> {
    state.list()
}

/// Create a new durable room record (SPEC.md §4.6: "Rooms are durable
/// records, not ad-hoc sessions"). `open` defaults to `true` (open, the
/// spec's stated default for internal eng rooms) at the frontend call site,
/// not hidden inside this command, so the choice is visible wherever it's
/// invoked from.
#[tauri::command]
pub fn create_room(
    state: tauri::State<'_, RoomsState>,
    name: String,
    open: bool,
    display_name: Option<String>,
) -> Result<RoomRecord, RoomsError> {
    state.create_with_display(&name, open, display_name.as_deref())
}

/// Set or clear the optional local display label for a room. The room's
/// access code (`name`) and LiveKit slug are intentionally unchanged.
#[tauri::command]
pub fn rename_room(
    state: tauri::State<'_, RoomsState>,
    id_or_code: String,
    display_name: Option<String>,
) -> Result<RoomRecord, RoomsError> {
    state.rename_display(&id_or_code, display_name.as_deref())
}

/// Forget a saved room from this machine's local room list. The returned record
/// is the removed row, useful for optimistic UI rollback or logging.
#[tauri::command]
pub fn forget_room(
    state: tauri::State<'_, RoomsState>,
    id_or_code: String,
) -> Result<RoomRecord, RoomsError> {
    state.forget(&id_or_code)
}

/// Clear this device's local room metadata and delete `rooms.json`. This does
/// not touch backend/LiveKit rooms; it is part of Settings' local factory reset.
#[tauri::command]
pub fn reset_local_rooms(state: tauri::State<'_, RoomsState>) -> Result<(), RoomsError> {
    state.reset_local()
}

/// Server-side occupancy for every durable room this machine knows about,
/// without joining any room. Proof-of-possession: the backend is sent this
/// machine's room credentials (plus the access code for knock-to-join rooms)
/// and answers ONLY for those -- there is no public directory to browse
/// (`GET /api/rooms` is 410; see docs/CONTRACTS.md "Room status").
#[tauri::command]
pub async fn list_room_occupancy(
    state: tauri::State<'_, RoomsState>,
) -> Result<Vec<RoomOccupancy>, String> {
    let rooms = state.list();
    Ok(query_room_occupancy(rooms).await)
}

/// Max credentials per status request; mirrors the backend's
/// `ROOM_STATUS_MAX_ROOMS` (docs/CONTRACTS.md "Room status").
const ROOM_STATUS_MAX_ROOMS: usize = 64;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomStatusRequestEntry {
    room: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    access_code: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomStatusRequest {
    rooms: Vec<RoomStatusRequestEntry>,
}

fn room_status_request(rooms: &[RoomRecord]) -> RoomStatusRequest {
    RoomStatusRequest {
        rooms: rooms
            .iter()
            .take(ROOM_STATUS_MAX_ROOMS)
            .map(|room| RoomStatusRequestEntry {
                room: room_credential(room),
                // The access code is only needed for `open:false` rooms, but
                // the local `open` flag is a stale initial value (the server
                // preserves its own), so always send it when held.
                access_code: room.access_code.clone(),
            })
            .collect(),
    }
}

async fn query_room_occupancy(rooms: Vec<RoomRecord>) -> Vec<RoomOccupancy> {
    // Nothing held -> nothing to ask: the lookup can only answer for
    // credentials this machine presents, so an empty rooms.json never hits
    // the network.
    if rooms.is_empty() {
        return Vec::new();
    }
    let base = match crate::transport::token::backend_base_url() {
        Ok(base) => base,
        Err(err) => return unavailable_for_rooms(&rooms, err.to_string()),
    };
    let url = format!("{base}/api/rooms/status");
    let response = match crate::transport::backend_http::send_with_retry(
        crate::transport::backend_http::client()
            .post(&url)
            .json(&room_status_request(&rooms)),
    )
    .await
    {
        Ok(response) => response,
        Err(err) => {
            return unavailable_for_rooms(&rooms, format!("rooms backend unavailable: {err}"))
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let reason = if body.trim().is_empty() {
            format!("rooms backend returned {status}")
        } else {
            format!("rooms backend returned {status}: {body}")
        };
        return unavailable_for_rooms(&rooms, reason);
    }
    let backend = match response.json::<BackendRoomsResponse>().await {
        Ok(body) => body,
        Err(err) => {
            return unavailable_for_rooms(&rooms, format!("invalid rooms backend response: {err}"))
        }
    };
    merge_room_status(rooms, backend)
}

/// Pure merge of the backend's answer onto the local records. Rows come back
/// in local order; a room the backend omitted (not live, or closed and the
/// code we hold is wrong) keeps `available: true` with no status fields so
/// the UI renders it as empty rather than errored.
fn merge_room_status(rooms: Vec<RoomRecord>, backend: BackendRoomsResponse) -> Vec<RoomOccupancy> {
    let mut out = Vec::with_capacity(rooms.len());
    for room in rooms {
        let room_name = room.name.clone();
        let livekit_room = livekit_room_name(&room);
        let public_id = public_room_id_for_livekit_room(&livekit_room);
        let active = backend.rooms.iter().find(|candidate| {
            candidate.id == public_id
                || room
                    .display_name
                    .as_deref()
                    .map(|label| label == candidate.name)
                    .unwrap_or(false)
        });
        out.push(RoomOccupancy {
            room_name,
            livekit_room,
            id: Some(public_id),
            available: true,
            participants: Vec::new(),
            unavailable_reason: None,
            name: active.map(|candidate| candidate.name.clone()),
            slug: None,
            open: active.map(|candidate| candidate.open),
            occupancy: active.map(|candidate| candidate.occupancy),
        });
    }
    out
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackendRoomsResponse {
    rooms: Vec<BackendRoomView>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackendRoomView {
    id: String,
    name: String,
    open: bool,
    occupancy: usize,
}

fn unavailable_for_rooms(rooms: &[RoomRecord], reason: String) -> Vec<RoomOccupancy> {
    rooms
        .iter()
        .map(|room| RoomOccupancy {
            room_name: room.name.clone(),
            livekit_room: livekit_room_name(room),
            id: Some(public_room_id_for_livekit_room(&livekit_room_name(room))),
            available: false,
            participants: Vec::new(),
            unavailable_reason: Some(reason.clone()),
            name: None,
            slug: None,
            open: None,
            occupancy: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ContractFixture {
        slugify: Vec<SlugifyVector>,
        room_credentials: Vec<RoomCredentialVector>,
        #[serde(default)]
        room_status_request: serde_json::Value,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SlugifyVector {
        input: String,
        slug: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RoomCredentialVector {
        input: String,
        normalized: String,
        livekit_room: String,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    fn temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("petal-rooms-test-{}", new_room_id()));
        dir
    }

    fn wait_until_after_ms(timestamp_ms: u64) {
        while now_ms() <= timestamp_ms {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn create_then_list_round_trips() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let created = state.create("eng-sync", true).unwrap();
        assert!(created.name.starts_with("room-"));
        assert!(created
            .access_code
            .as_deref()
            .is_some_and(|code| normalize_access_code(code).is_some()));
        assert_eq!(
            normalize_room_credential(&created.name),
            Some(created.name.clone())
        );
        assert_eq!(created.slug, created.name);
        assert_eq!(created.display_name.as_deref(), Some("eng-sync"));
        assert!(created.last_joined_ms.is_some());
        assert!(created.open);

        let listed = state.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].last_joined_ms, created.last_joined_ms);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn autotest_ownership_reuses_one_persisted_capability_across_three_loads() {
        let dir = temp_dir();
        let mut canonical = None;

        for reload in 0..3 {
            let state = RoomsState::load(dir.clone());
            let resolved = state
                .resolve_autotest_room("qa-reload-key", reload == 0)
                .unwrap();
            if let Some(expected) = &canonical {
                assert_eq!(
                    &resolved.name, expected,
                    "reload {reload} must reuse the explicitly owned capability"
                );
            } else {
                canonical = Some(resolved.name.clone());
            }
            assert_eq!(state.list().len(), 1, "reload {reload}");

            let persisted: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("rooms.json")).unwrap())
                    .unwrap();
            assert_eq!(persisted["autotestOwnership"].as_array().unwrap().len(), 1);
            assert_eq!(
                persisted["autotestOwnership"][0]["canonicalCredential"],
                serde_json::Value::String(resolved.name)
            );
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn autotest_ownership_never_selects_or_modifies_a_same_label_user_room() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let user_room = state.create("QA User Label", true).unwrap();
        let before = std::fs::read_to_string(dir.join("rooms.json")).unwrap();

        let error = state
            .resolve_autotest_room(" qa   user LABEL ", true)
            .unwrap_err();
        assert!(matches!(error, RoomsError::AutotestOwnership(_)));
        assert_eq!(state.list().len(), 1);
        assert_eq!(state.list()[0].id, user_room.id);
        assert_eq!(
            std::fs::read_to_string(dir.join("rooms.json")).unwrap(),
            before
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_rooms_file_bootstraps_explicit_autotest_ownership_and_preserves_user_room() {
        let dir = temp_dir();
        let user_room = {
            let state = RoomsState::load(dir.clone());
            state.create("Legacy user room", true).unwrap()
        };
        let legacy_path = dir.join("rooms.json");
        let legacy_contents = std::fs::read_to_string(&legacy_path).unwrap();
        assert!(
            !legacy_contents.contains("autotestOwnership"),
            "the regression must begin from the pre-#609 file shape"
        );

        let created = {
            let state = RoomsState::load(dir.clone());
            let created = state.resolve_autotest_room("qa-legacy", true).unwrap();
            assert!(state.list().iter().any(|room| room.id == user_room.id));
            assert_eq!(state.list().len(), 2);
            created
        };
        let reloaded = RoomsState::load(dir.clone());
        let reused = reloaded.resolve_autotest_room("qa-legacy", false).unwrap();
        assert_eq!(reused.name, created.name);
        assert!(reloaded.list().iter().any(|room| room.id == user_room.id));
        assert_eq!(reloaded.list().len(), 2);

        let persisted = std::fs::read_to_string(&legacy_path).unwrap();
        assert!(persisted.contains("autotestOwnership"));
        assert!(persisted.contains(&user_room.name));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_autotest_ownership_fails_closed_without_creating_a_room() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rooms.json");
        std::fs::write(
            &path,
            r#"{
                "rooms": [],
                "autotestOwnership": [
                    { "qaKey": "qa-bad", "canonicalCredential": "room-0123456789abcdef0123456789abcdef" },
                    { "qaKey": "qa-bad", "canonicalCredential": "room-fedcba9876543210fedcba9876543210" }
                ]
            }"#,
        )
        .unwrap();
        let state = RoomsState::load(dir.clone());

        assert!(matches!(
            state.resolve_autotest_room("qa-bad", true),
            Err(RoomsError::AutotestOwnership(_))
        ));
        assert!(state.list().is_empty());
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("autotestOwnership"),
            true
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_autotest_ownership_fails_closed_without_creating_a_room() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rooms.json"),
            r#"{
                "rooms": [],
                "autotestOwnership": [
                    { "qaKey": "qa-stale", "canonicalCredential": "room-0123456789abcdef0123456789abcdef" }
                ]
            }"#,
        )
        .unwrap();
        let state = RoomsState::load(dir.clone());

        assert!(matches!(
            state.resolve_autotest_room("qa-stale", true),
            Err(RoomsError::AutotestOwnership(_))
        ));
        assert!(state.list().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_or_incomplete_autotest_ownership_never_becomes_a_fresh_mapping() {
        let fixtures = [
            "not JSON",
            r#"{ "rooms": [], "autotestOwnership": {} }"#,
            r#"{
                "rooms": [],
                "autotestOwnership": [
                    { "qaKey": 7, "canonicalCredential": "room-0123456789abcdef0123456789abcdef" }
                ]
            }"#,
        ];

        for contents in fixtures {
            let dir = temp_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("rooms.json");
            std::fs::write(&path, contents).unwrap();
            let state = RoomsState::load(dir.clone());

            assert!(matches!(
                state.resolve_autotest_room("qa-corrupt", true),
                Err(RoomsError::AutotestOwnership(_))
            ));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);

            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn autotest_qa_key_cannot_be_a_room_credential_or_access_code() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());

        for key in ["abc-defg-hjk", "room-0123456789abcdef0123456789abcdef"] {
            assert!(matches!(
                state.resolve_autotest_room(key, true),
                Err(RoomsError::AutotestOwnership(_))
            ));
        }
        assert!(state.list().is_empty());
        assert!(!dir.join("rooms.json").exists());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reset_local_clears_memory_and_removes_rooms_file() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let path = dir.join("rooms.json");

        state.create("reset-me", true).unwrap();
        assert!(!state.list().is_empty());
        assert!(path.exists());

        state.reset_local().unwrap();

        assert!(state.list().is_empty());
        assert!(!path.exists());
        state.reset_local().unwrap();

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn creating_same_credential_twice_is_idempotent_not_duplicated() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let access_code = "abc-defg-hjk";
        let first = state.create(access_code, false).unwrap();
        let first_joined = first.last_joined_ms.expect("first join timestamp");
        wait_until_after_ms(first_joined);
        let second = state.create(access_code, false).unwrap();
        assert_eq!(
            first.id, second.id,
            "same credential should return the same record, not a duplicate"
        );
        assert!(
            second.last_joined_ms.expect("second join timestamp") > first_joined,
            "re-joining an existing room should bump last_joined_ms"
        );
        assert_eq!(state.list().len(), 1);
        let reloaded = RoomsState::load(dir.clone());
        let reloaded_rooms = reloaded.list();
        assert_eq!(reloaded_rooms.len(), 1);
        assert_eq!(reloaded_rooms[0].last_joined_ms, second.last_joined_ms);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn create_from_access_code_round_trips_credential() {
        // #107: creating from an access code must store THAT code and derive the
        // credential from it, so an invited peer typing the code reaches the
        // exact same room. (The old path minted a mismatched random code.)
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let created = state.create("abc-defg-hjk", true).unwrap();
        let code = created.access_code.clone().expect("access code");
        assert_eq!(
            internal_credential_for_access_code(&code).as_deref(),
            Some(created.name.as_str()),
            "the stored access code must hash back to the room's credential"
        );
        // A bare-code create carries no human name -> UI shows "Petal meeting".
        assert_eq!(created.display_name, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bare_credential_is_rejected_instead_of_getting_a_fabricated_code() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        assert!(matches!(
            state.create(&credential, true),
            Err(RoomsError::EmptyName)
        ));
        assert!(state.list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_migration_clears_poisoned_code_and_real_join_repairs_it() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        let poisoned = serde_json::json!({
            "rooms": [{
                "id": "poisoned",
                "name": credential,
                "slug": credential,
                "accessCode": "joa-uozn-rxt",
                "createdAtMs": 1,
                "open": true
            }]
        });
        std::fs::write(
            dir.join("rooms.json"),
            serde_json::to_string(&poisoned).unwrap(),
        )
        .unwrap();

        let state = RoomsState::load(dir.clone());
        assert_eq!(state.list()[0].access_code, None);
        let persisted = std::fs::read_to_string(dir.join("rooms.json")).unwrap();
        assert!(!persisted.contains("joa-uozn-rxt"));

        let repaired = state.create("abc-defg-hjk", true).unwrap();
        assert_eq!(repaired.name, credential);
        assert_eq!(repaired.access_code.as_deref(), Some("abc-defg-hjk"));
        assert_eq!(state.list().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// #421 regression, launch-to-launch: a record poisoned by an older build
    /// (correct credential paired with an unrelated random code) must have its
    /// SAVED ROOM IDENTITY preserved on every subsequent load, not rewritten to
    /// the bogus code's room. Repeating the load is the load-bearing half: a
    /// migration that rewrites `name`/`slug` silently changes which room the
    /// saved entry points at, and does so on next launch rather than during the
    /// session that caused it.
    #[test]
    fn poisoned_record_keeps_its_saved_room_identity_across_repeated_loads() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        let fabricated = "joa-uozn-rxt";
        let fabricated_room = internal_credential_for_access_code(fabricated).unwrap();
        assert_ne!(credential, fabricated_room);
        let poisoned = serde_json::json!({
            "rooms": [{
                "id": "poisoned",
                "name": credential,
                "slug": credential,
                "displayName": "rctest",
                "accessCode": fabricated,
                "createdAtMs": 1,
                "open": true
            }]
        });
        std::fs::write(
            dir.join("rooms.json"),
            serde_json::to_string(&poisoned).unwrap(),
        )
        .unwrap();

        for load in 1..=3 {
            let state = RoomsState::load(dir.clone());
            let rooms = state.list();
            assert_eq!(rooms.len(), 1, "load {load}");
            assert_eq!(rooms[0].name, credential, "load {load}: credential rewritten");
            assert_eq!(rooms[0].slug, credential, "load {load}: slug rewritten");
            assert_eq!(rooms[0].access_code, None, "load {load}");
            assert_eq!(rooms[0].display_name.as_deref(), Some("rctest"), "load {load}");
            assert_eq!(
                livekit_room_name(&rooms[0]),
                livekit_room_name_for(&credential),
                "load {load}: the saved entry now points at a different meeting"
            );
            let persisted = std::fs::read_to_string(dir.join("rooms.json")).unwrap();
            assert!(persisted.contains(&credential), "load {load}");
            assert!(!persisted.contains(fabricated), "load {load}");
            assert!(!persisted.contains(&fabricated_room), "load {load}");
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    /// #421 round-trip property: the access code IS the pre-image of the join
    /// capability, so a code that goes in must be the code that is stored and
    /// the code that can be re-shared -- for every code the generator can emit,
    /// through every spelling a user can paste (spacing, case, missing dashes).
    #[test]
    fn generated_access_codes_round_trip_through_join_and_re_share() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let mut seen_credentials: HashMap<String, String> = HashMap::new();

        for _ in 0..64 {
            let code = generate_access_code(&[]).expect("generated access code");
            let credential =
                internal_credential_for_access_code(&code).expect("credential for generated code");
            assert_eq!(normalize_access_code(&code).as_deref(), Some(code.as_str()));
            assert!(code.chars().all(|ch| ch == '-'
                || ACCESS_CODE_ALPHABET.contains(&(ch as u8))));

            // Every spelling a pasted code can arrive in resolves to the same
            // capability -- a joiner never lands in a different room.
            for spelling in [
                code.clone(),
                code.to_ascii_uppercase(),
                format!("  {code}  "),
                code.replace('-', ""),
            ] {
                assert_eq!(
                    internal_credential_for_access_code(&spelling).as_deref(),
                    Some(credential.as_str()),
                    "spelling '{spelling}' resolved to a different room"
                );
            }

            if let Some(other) = seen_credentials.insert(credential.clone(), code.clone()) {
                assert_eq!(other, code, "two distinct codes collided onto one room");
            }

            // Joining by the pasted code stores THAT code back, so the invite
            // link this peer can share re-derives the room it is actually in.
            let joined = state.create(&code, true).expect("join by pasted code");
            assert_eq!(joined.name, credential);
            assert_eq!(joined.access_code.as_deref(), Some(code.as_str()));
            assert_eq!(
                internal_credential_for_access_code(joined.access_code.as_deref().unwrap())
                    .as_deref(),
                Some(joined.name.as_str())
            );
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn creating_same_label_twice_generates_distinct_capabilities() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let first = state.create("Webtest", true).unwrap();
        let second = state.create("webtest", true).unwrap();

        assert_ne!(first.id, second.id);
        assert_ne!(first.name, second.name);
        assert!(first.name.starts_with("room-"));
        assert!(second.name.starts_with("room-"));
        assert_eq!(state.list().len(), 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn friendly_label_is_kept_separate_from_credential() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let first = state.create("Design Review!", true).unwrap();

        assert!(first.name.starts_with("room-"));
        assert_eq!(first.slug, first.name);
        assert_eq!(first.display_name.as_deref(), Some("design-review"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn create_with_display_keeps_typed_label_separate_from_credential() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let created = state
            .create_with_display("abc-defg-hjk", true, Some("Design Review!"))
            .unwrap();

        assert_eq!(
            created.name,
            internal_credential_for_access_code("abc-defg-hjk").unwrap()
        );
        assert_eq!(created.slug, created.name);
        assert_eq!(created.display_name.as_deref(), Some("Design Review!"));

        let renamed = state
            .create_with_display(&created.name, true, Some("Planning Sync"))
            .unwrap();
        assert_eq!(renamed.name, created.name);
        assert_eq!(renamed.display_name.as_deref(), Some("Planning Sync"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_name_is_rejected() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        assert!(matches!(
            state.create("   ", true),
            Err(RoomsError::EmptyName)
        ));
        assert!(matches!(
            state.create(" !!! ", true),
            Err(RoomsError::EmptyName)
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persists_across_a_fresh_load_same_path() {
        let dir = temp_dir();
        {
            let state = RoomsState::load(dir.clone());
            state.create("standup", true).unwrap();
        }
        // Simulate an app restart: a brand-new RoomsState loading the same path.
        let reloaded = RoomsState::load(dir.clone());
        let listed = reloaded.list();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].name.starts_with("room-"));
        assert_eq!(listed[0].slug, listed[0].name);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn access_code_created_room_stays_unlabeled_after_reload() {
        let dir = temp_dir();
        let created = {
            let state = RoomsState::load(dir.clone());
            state.create("abc-defg-hjk", true).unwrap()
        };
        assert_eq!(created.display_name, None);

        let reloaded = RoomsState::load(dir.clone());
        let listed = reloaded.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, created.name);
        assert_eq!(listed[0].access_code.as_deref(), Some("abc-defg-hjk"));
        assert_eq!(
            listed[0].display_name, None,
            "reload migration must not persist room credentials as display names"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_time_migration_preserves_only_human_legacy_labels() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rooms.json");
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        let contents = serde_json::json!({
            "rooms": [
                { "id": "credential-only", "name": credential, "createdAtMs": 100, "open": true },
                { "id": "legacy-public", "name": "eng-sync-0123456789abcdef0123456789abcdef", "createdAtMs": 200, "open": true },
                { "id": "human", "name": "Design Review!", "createdAtMs": 300, "open": true }
            ]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&contents).unwrap()).unwrap();

        let state = RoomsState::load(dir.clone());
        let listed = state.list();
        let by_id: HashMap<_, _> = listed.iter().map(|room| (room.id.as_str(), room)).collect();

        assert_eq!(by_id["credential-only"].display_name, None);
        assert_eq!(
            by_id["legacy-public"].display_name.as_deref(),
            Some("eng-sync")
        );
        assert_eq!(
            by_id["human"].display_name.as_deref(),
            Some("design-review")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn display_name_renames_local_label_without_changing_access_code() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let created = state.create("webtest", true).unwrap();

        let renamed = state
            .rename_display(&created.name, Some("Web Test Room"))
            .unwrap();
        assert!(renamed.name.starts_with("room-"));
        assert_eq!(renamed.slug, renamed.name);
        assert_eq!(renamed.display_name.as_deref(), Some("Web Test Room"));

        let reloaded = RoomsState::load(dir.clone());
        let listed = reloaded.list();
        assert_eq!(listed[0].name, created.name);
        assert_eq!(listed[0].slug, created.name);
        assert_eq!(listed[0].display_name.as_deref(), Some("Web Test Room"));
        assert_eq!(
            livekit_room_name(&listed[0]),
            format!("petal-room-{}", created.name)
        );

        let cleared = reloaded.rename_display(&created.name, Some("   ")).unwrap();
        assert_eq!(cleared.display_name, None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn display_name_can_be_renamed_by_record_id() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let created = state.create("design-review", true).unwrap();

        let renamed = state
            .rename_display(&created.id, Some("Design critique"))
            .unwrap();
        assert_eq!(renamed.name, created.name);
        assert_eq!(renamed.display_name.as_deref(), Some("Design critique"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forget_removes_room_by_normalized_credential_and_persists() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let kept = state.create("standup", true).unwrap();
        let forgotten = state.create("Design Review", false).unwrap();
        let upper_code = forgotten.name.to_ascii_uppercase();

        let removed = state.forget(&format!("  {upper_code}  ")).unwrap();
        assert_eq!(removed.id, forgotten.id);
        assert_eq!(removed.name, forgotten.name);

        let listed = state.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, kept.id);

        let reloaded = RoomsState::load(dir.clone());
        let listed_after_reload = reloaded.list();
        assert_eq!(listed_after_reload.len(), 1);
        assert_eq!(listed_after_reload[0].id, kept.id);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forget_removes_room_by_record_id() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        let created = state.create("webtest", true).unwrap();

        let removed = state.forget(&created.id).unwrap();
        assert_eq!(removed.name, created.name);
        assert!(state.list().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forget_reports_empty_and_missing_queries() {
        let dir = temp_dir();
        let state = RoomsState::load(dir.clone());
        assert!(matches!(state.forget("   "), Err(RoomsError::EmptyName)));
        assert!(matches!(
            state.forget("missing-abc-defg-hjk"),
            Err(RoomsError::NotFound(_))
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_time_merge_collapses_existing_slug_duplicates_and_keeps_earliest_created_at() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rooms.json");
        let contents = serde_json::json!({
            "rooms": [
                { "id": "late", "name": "abc-defg-hjk", "createdAtMs": 200, "open": true },
                { "id": "early", "name": "abc-defg-hjk", "createdAtMs": 100, "open": false },
                { "id": "design", "name": "Design Review!", "createdAtMs": 300, "open": true }
            ]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&contents).unwrap()).unwrap();

        let state = RoomsState::load(dir.clone());
        let listed = state.list();
        assert_eq!(listed.len(), 2);
        let webtest = listed
            .iter()
            .find(|room| room.access_code.as_deref() == Some("abc-defg-hjk"))
            .unwrap();
        assert_eq!(webtest.id, "early");
        assert_eq!(
            webtest.name,
            internal_credential_for_access_code("abc-defg-hjk").unwrap()
        );
        assert_eq!(webtest.created_at_ms, 100);
        assert!(!webtest.open);

        let reloaded = RoomsState::load(dir.clone());
        assert_eq!(
            reloaded.list().len(),
            2,
            "migration should persist the merge"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn livekit_room_name_is_derived_from_full_capability_not_id_or_bare_label() {
        let credential = internal_credential_for_access_code("abc-defg-hjk").unwrap();
        let a = RoomRecord {
            id: "machine-a-19f1-0".to_string(),
            name: credential.clone(),
            access_code: Some("abc-defg-hjk".to_string()),
            display_name: None,
            slug: credential.clone(),
            created_at_ms: 0,
            last_joined_ms: None,
            open: true,
        };
        let b = RoomRecord {
            id: "machine-b-abcd-7".to_string(),
            name: credential.clone(),
            access_code: Some("abc-defg-hjk".to_string()),
            display_name: None,
            slug: credential.clone(),
            created_at_ms: 999,
            last_joined_ms: Some(1234),
            open: true,
        };
        assert_eq!(livekit_room_name(&a), format!("petal-room-{credential}"));
        assert_eq!(
            livekit_room_name(&a),
            livekit_room_name(&b),
            "same capability, different machine-local ids -> same LiveKit room"
        );
        assert_ne!(livekit_room_name(&a), "petal-room-eng-sync");
        assert_ne!(livekit_room_name(&a), "petal-dev-room");
    }

    #[test]
    fn slugify_normalizes_trivially_different_typings_to_the_same_room() {
        assert_eq!(slugify("Design Review"), "design-review");
        assert_eq!(slugify("design-review"), "design-review");
        assert_eq!(slugify("  design   review  "), "design-review");
        assert_eq!(slugify("Design Review!"), "design-review");
        assert_eq!(slugify("eng-sync"), "eng-sync");
        assert_eq!(slugify("quick-mr2dzrhh"), "quick-mr2dzrhh");
        // No alphanumerics -> stable fallback, never a bare `petal-room-`.
        assert_eq!(slugify("---"), "room");
    }

    #[test]
    fn slugify_matches_shared_contract_fixture() {
        let fixture = contract_fixture();
        for vector in fixture.slugify {
            assert_eq!(slugify(&vector.input), vector.slug, "{}", vector.input);
        }
    }

    #[test]
    #[should_panic(expected = "room credential must include a capability suffix")]
    fn livekit_room_name_for_bare_label_fails_closed() {
        let _ = livekit_room_name_for("Eng Sync");
    }

    #[test]
    #[should_panic(expected = "room credential must include a capability suffix")]
    fn livekit_room_name_for_empty_slug_fallback_fails_closed() {
        let _ = livekit_room_name_for("!!!");
    }

    #[test]
    fn access_code_alphabet_excludes_visually_ambiguous_i_l() {
        assert!(!ACCESS_CODE_ALPHABET.contains(&b'i'));
        assert!(!ACCESS_CODE_ALPHABET.contains(&b'l'));
        assert_eq!(
            normalize_access_code(" ABC-DEFG-HJK "),
            Some("abc-defg-hjk".to_string())
        );
        assert_eq!(
            normalize_access_code("abc-defg-hij"),
            Some("abc-defg-hij".to_string())
        );
        assert_eq!(
            normalize_access_code("abc-defg-hlj"),
            Some("abc-defg-hlj".to_string())
        );
        assert_eq!(normalize_access_code("myq-xfkw-azrp"), None);

        let generated: Vec<String> = (0..200)
            .map(|_| generate_access_code(&[]).expect("generated access code"))
            .collect();
        assert!(generated
            .iter()
            .all(|code| normalize_access_code(code).is_some()));
        assert!(generated
            .iter()
            .all(|code| !code.contains('i') && !code.contains('l')));
    }

    #[test]
    fn room_credentials_are_internal_128_bit_hex_capabilities() {
        let credential = "room-8535e993a1b76ed8a9ee59b265f53dfc";
        assert_eq!(
            normalize_room_credential(credential),
            Some(credential.to_string())
        );
        assert_eq!(normalize_room_credential("design-review"), None);
        assert_eq!(normalize_room_credential("design-review-xyz"), None);
        assert_eq!(room_label_from_credential(credential), None);
        assert_eq!(
            livekit_room_name_for(credential),
            format!("petal-room-{credential}")
        );
    }

    #[test]
    fn public_room_id_matches_backend_directory_contract() {
        assert_eq!(
            public_room_id_for_livekit_room(
                "petal-room-design-review-0123456789abcdef0123456789abcdef"
            ),
            "room_3832459f4c9db01b920f212bb706d9bc"
        );
        assert_ne!(
            public_room_id_for_livekit_room("petal-room-design-review"),
            "petal-room-design-review"
        );
    }

    #[test]
    fn room_credentials_match_shared_contract_fixture() {
        let fixture = contract_fixture();
        for vector in fixture.room_credentials {
            assert_eq!(
                normalize_room_credential(&vector.input),
                Some(vector.normalized.clone()),
                "{}",
                vector.input
            );
            assert_eq!(
                livekit_room_name_for(&vector.normalized),
                vector.livekit_room,
                "{}",
                vector.input
            );
        }
    }

    fn status_record(id: &str, name: &str, access_code: Option<&str>, open: bool) -> RoomRecord {
        RoomRecord {
            id: id.into(),
            name: name.into(),
            access_code: access_code.map(str::to_string),
            display_name: None,
            slug: String::new(),
            created_at_ms: 0,
            last_joined_ms: None,
            open,
        }
    }

    #[test]
    fn room_status_request_sends_credentials_and_held_access_codes_only() {
        let rooms = vec![
            status_record("a", "room-8535e993a1b76ed8a9ee59b265f53dfc", Some("abc-defg-hjk"), false),
            status_record("b", "room-00000000000000000000000000000001", None, true),
        ];
        let json = serde_json::to_value(room_status_request(&rooms)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "rooms": [
                    { "room": "room-8535e993a1b76ed8a9ee59b265f53dfc", "accessCode": "abc-defg-hjk" },
                    { "room": "room-00000000000000000000000000000001" }
                ]
            })
        );
    }

    #[test]
    fn room_status_request_matches_shared_contract_fixture() {
        let fixture = contract_fixture().room_status_request;
        assert_eq!(fixture["maxRooms"], serde_json::json!(ROOM_STATUS_MAX_ROOMS));
        let rooms = vec![
            status_record("a", "room-8535e993a1b76ed8a9ee59b265f53dfc", Some("abc-defg-hjk"), false),
            status_record("b", "room-00000000000000000000000000000001", None, true),
        ];
        assert_eq!(serde_json::to_value(room_status_request(&rooms)).unwrap(), fixture["request"]);
    }

    #[test]
    fn room_status_request_is_capped_at_the_backend_limit() {
        let rooms: Vec<RoomRecord> = (0..(ROOM_STATUS_MAX_ROOMS + 5))
            .map(|i| status_record(&i.to_string(), &format!("room-{i:032x}"), None, true))
            .collect();
        assert_eq!(room_status_request(&rooms).rooms.len(), ROOM_STATUS_MAX_ROOMS);
    }

    #[test]
    fn merge_room_status_only_enriches_rooms_the_backend_answered_for() {
        let held = status_record("a", "room-8535e993a1b76ed8a9ee59b265f53dfc", Some("abc-defg-hjk"), true);
        let omitted = status_record("b", "room-00000000000000000000000000000001", None, true);
        let public_id = public_room_id_for_livekit_room(&livekit_room_name(&held));
        let backend = BackendRoomsResponse {
            rooms: vec![
                BackendRoomView { id: public_id.clone(), name: "Eng meeting".into(), open: true, occupancy: 3 },
                // A row we never asked for must NOT become a local room.
                BackendRoomView { id: "room_deadbeef".into(), name: "Stranger".into(), open: true, occupancy: 9 },
            ],
        };
        let rows = merge_room_status(vec![held, omitted], backend);
        assert_eq!(rows.len(), 2, "exactly the local records, nothing discovered");
        assert_eq!(rows[0].id.as_deref(), Some(public_id.as_str()));
        assert_eq!(rows[0].name.as_deref(), Some("Eng meeting"));
        assert_eq!(rows[0].occupancy, Some(3));
        assert!(rows[0].available);
        assert_eq!(rows[1].name, None);
        assert_eq!(rows[1].occupancy, None);
        assert!(rows[1].available, "an omitted room renders empty, not errored");
    }
}
