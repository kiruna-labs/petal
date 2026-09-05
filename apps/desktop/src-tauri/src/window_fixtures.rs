//! Record/replay fixtures for window-tracking characterization (#742).
//!
//! Rung A of the plan's test strategy (`internal/docs/WINDOW_REGISTRY_PLAN.md`
//! §7.1): capture timestamped streams of the REAL `CGWindowList` snapshots +
//! cursor positions from a live, TCC-granted session, check a small curated
//! set into `fixtures/window-tracking/`, and replay them through the seam
//! functions (`hover_tab::platform::hover_snapshots` + hit test,
//! `share_border::border_stack_from_entries`, `telepointer::visible_window_ids`
//! + `frames_to_apply`) asserting the exact decision sequence against a
//! committed golden. The registry (#744) must later pass the SAME goldens via
//! its fixture-source ingest mode.
//!
//! Recording is dev-only, gated on `PETAL_RECORD_WINDOW_FIXTURES=<dir>`
//! (checked once at startup; the thread samples at ~30 Hz for
//! `PETAL_RECORD_WINDOW_FIXTURES_SECS`, default 12, then stops). Recording
//! from an UNGRANTED process captures a TCC-restricted 3-window world (plan
//! §9.5) — the recorder warns loudly if the list looks restricted.

#![cfg(target_os = "macos")]

use serde::{Deserialize, Serialize};

/// Serialized form of one `cg::WindowEntry`. Field-for-field mirror; kept as a
/// separate type so the fixture format is stable even if `WindowEntry` grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureWindow {
    pub number: i64,
    pub owner_pid: i64,
    pub owner_name: String,
    pub name: String,
    pub layer: i64,
    pub alpha: f64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl From<&crate::platform::cg::WindowEntry> for FixtureWindow {
    fn from(e: &crate::platform::cg::WindowEntry) -> Self {
        Self {
            number: e.number,
            owner_pid: e.owner_pid,
            owner_name: e.owner_name.clone(),
            name: e.name.clone(),
            layer: e.layer,
            alpha: e.alpha,
            x: e.x,
            y: e.y,
            w: e.w,
            h: e.h,
        }
    }
}

impl FixtureWindow {
    pub fn to_entry(&self) -> crate::platform::cg::WindowEntry {
        crate::platform::cg::WindowEntry {
            number: self.number,
            owner_pid: self.owner_pid,
            owner_name: self.owner_name.clone(),
            name: self.name.clone(),
            layer: self.layer,
            alpha: self.alpha,
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
        }
    }
}

/// One sampled frame: everything a consumer tick sees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureFrame {
    /// Milliseconds since recording start.
    pub t_ms: u64,
    pub cursor: Option<(f64, f64)>,
    pub windows: Vec<FixtureWindow>,
}

/// Spawn the recorder thread if `PETAL_RECORD_WINDOW_FIXTURES` is set.
/// Called once from setup. Writes `<dir>/capture-<unix_secs>.jsonl`.
pub fn start_if_enabled() {
    let Ok(dir) = std::env::var("PETAL_RECORD_WINDOW_FIXTURES") else {
        return;
    };
    let secs: u64 = std::env::var("PETAL_RECORD_WINDOW_FIXTURES_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    std::thread::Builder::new()
        .name("window-fixture-recorder".into())
        .spawn(move || record(&dir, secs))
        .ok();
}

fn record(dir: &str, secs: u64) {
    use std::io::Write;
    if let Err(e) = std::fs::create_dir_all(dir) {
        log::warn!("window_fixtures: cannot create {dir}: {e}");
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = format!("{dir}/capture-{stamp}.jsonl");
    let Ok(mut file) = std::fs::File::create(&path) else {
        log::warn!("window_fixtures: cannot create {path}");
        return;
    };
    log::info!("window_fixtures: recording ~30Hz for {secs}s -> {path}");
    let start = std::time::Instant::now();
    let mut frames = 0u32;
    let mut max_windows = 0usize;
    while start.elapsed().as_secs() < secs {
        let t_ms = start.elapsed().as_millis() as u64;
        let cursor = crate::platform::cg::cursor_position();
        let windows = crate::platform::cg::onscreen_windows()
            .unwrap_or_default()
            .iter()
            .map(FixtureWindow::from)
            .collect::<Vec<_>>();
        max_windows = max_windows.max(windows.len());
        let frame = FixtureFrame {
            t_ms,
            cursor,
            windows,
        };
        if let Ok(json) = serde_json::to_string(&frame) {
            let _ = writeln!(file, "{json}");
        }
        frames += 1;
        std::thread::sleep(std::time::Duration::from_millis(33));
    }
    log::info!("window_fixtures: DONE — {frames} frames, max {max_windows} windows -> {path}");
    if max_windows <= 4 {
        // Plan §9.5: an ungranted process sees a TCC-restricted 3-entry list.
        log::warn!(
            "window_fixtures: max {max_windows} windows per frame — this looks \
             TCC-RESTRICTED (no Screen Recording); fixture is NOT representative"
        );
    }
}

/// Load a `.jsonl` fixture. Used by the replay tests.
#[cfg(test)]
pub fn load(path: &std::path::Path) -> Vec<FixtureFrame> {
    let data = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()));
    data.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad fixture line: {e}")))
        .collect()
}

/// Directory of checked-in fixtures + goldens.
#[cfg(test)]
pub fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/window-tracking")
}

/// The checked-in fixture basenames every golden replay runs over. `session-a`
/// is a real recorded session (realism, big window count); `synthetic-
/// transitions` is hand-built to guarantee every decision branch appears
/// (mover crossing the cursor, own-chrome, sub-40, transparent, cursor
/// leaving all windows, a None-cursor frame).
#[cfg(test)]
pub const REPLAY_FIXTURES: &[&str] = &["session-a", "synthetic-transitions"];

/// Golden support: compare `actual` (pretty JSON) with the committed golden.
/// `PETAL_BLESS_GOLDENS=1` (re)writes the golden instead of asserting —
/// blessing is a reviewed, deliberate act (plan §0 rule 6: silent golden
/// updates are forbidden).
#[cfg(test)]
pub fn assert_golden(name: &str, actual: &impl Serialize) {
    let golden_path = fixtures_dir().join(format!("{name}.golden.json"));
    let actual_json =
        serde_json::to_string_pretty(actual).expect("golden serialization cannot fail");
    if std::env::var("PETAL_BLESS_GOLDENS").is_ok() {
        std::fs::write(&golden_path, format!("{actual_json}\n"))
            .unwrap_or_else(|e| panic!("cannot bless {}: {e}", golden_path.display()));
        eprintln!("BLESSED {}", golden_path.display());
        return;
    }
    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|_| {
        panic!(
            "missing golden {} — record a fixture, review the output, then run once \
             with PETAL_BLESS_GOLDENS=1",
            golden_path.display()
        )
    });
    assert_eq!(
        actual_json.trim(),
        expected.trim(),
        "golden mismatch for {name}: the decision sequence over the recorded fixture \
         changed. If intentional, re-bless with PETAL_BLESS_GOLDENS=1 and call the \
         change out in the PR (plan §0 rule 6)."
    );
}
