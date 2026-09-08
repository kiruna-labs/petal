//! PLUGIN-BOOT: do plugin frames actually boot in WKWebView under the CSP the
//! desktop app ships? (#37, PR #82.)
//!
//! Plugin frames are `srcdoc` iframes. CSP3 exempts `about:srcdoc` from
//! `frame-src` matching, so `frame-src 'none'` is supposed to refuse a frame's
//! self-navigation without refusing the frame itself. That is proven in
//! Chromium by `web-harness/tests/pluginSelfNavigation.test.ts` -- and Chromium
//! is not what the shipped macOS app renders with. If WebKit instead blocked
//! srcdoc frames, every plugin would silently stop working on the desktop, and
//! the ONLY thing that would say so is a warning the Cockpit never produced,
//! because the Cockpit never loaded a plugin at all: it joins its room through
//! `session::join_room` from Rust and never navigates the main webview to the
//! meeting route that mounts `PluginSurfaces`.
//!
//! So this scenario opens the one route that does mount it
//! (`apps/desktop/src/routes/dev/plugin-boot/+page.svelte`) in a real webview
//! window of this binary, which serves its pages through Tauri's asset
//! protocol with the configured CSP attached, and then asserts on HOST-SIDE
//! evidence: `plugins(host): plugin petal.reactions frame ready`, a line the
//! frame's own scripts have to run and post back to the host to produce.
//!
//! It is deliberately NOT an "absence of the canary" check. A missing warning
//! line proves nothing (#559/#561); the verdict here is a named plugin id
//! having reported ready, and a probe page that never mounted is reported as
//! INFRA-FAIL rather than as a product verdict.

use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// The probe window's label. It is also listed in
/// `src-tauri/capabilities/default.json` so `getVersion()` resolves the real
/// version inside it rather than the "dev" fallback.
pub const PROBE_LABEL: &str = "cockpit-plugin-boot-probe";

/// Loaded via `WebviewUrl::App`, i.e. out of the embedded asset table. Only a
/// `PETAL_INCLUDE_DEV_ROUTES=1` frontend build emits it, which is exactly what
/// `scripts/build-cockpit-primary.sh` produces.
pub const PROBE_ROUTE: &str = "dev/plugin-boot.html";
const PROBE_ASSET_KEY: &str = "/dev/plugin-boot.html";

/// The built-in whose frame must boot. Its manifest id, not its name: the
/// journal line names the id.
pub const REQUIRED_PLUGIN_ID: &str = "petal.reactions";

/// Byte-identical with the probe page's own constants; the page emits these
/// through the ordinary `plugin_host_log` command.
const PROBE_MOUNTED_LINE: &str = "plugin-boot probe: page mounted";
const SELF_NAV_LINE_PREFIX: &str = "plugin-boot probe: srcdoc self-navigation";
/// `PluginSurfaces` logs this once the shared host has loaded its catalog:
/// `host booted (Petal <version>); loaded plugins: <ids|none>`. It separates
/// "the plugin was never loaded" (a disabled built-in, an empty catalog) from
/// "the plugin was loaded and its frame never came up".
const HOST_BOOTED_LINE_PREFIX: &str = "host booted";

/// Generous: a cold webview window plus a SvelteKit hydrate plus two frame
/// boots. The host's own ready warning fires at 5s, so anything healthy is
/// resolved long before this.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The page's self-navigation probe reports after ~2.5s; wait past that before
/// giving up on the (non-gating) CSP-enforcement observation.
const SELF_NAV_GRACE: Duration = Duration::from_secs(5);

/// What the host-side journal says about one probe run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeEvidence {
    /// The probe page reached `onMount` and reported it.
    pub mounted: bool,
    /// Plugin ids that reported `ready` from inside their srcdoc frame.
    pub ready_plugin_ids: Vec<String>,
    /// The host's own 5s "never reported ready" warnings, verbatim. Corroborating
    /// detail for a failure; never the thing a pass is decided on.
    pub never_ready_warnings: Vec<String>,
    /// The page's `frame-src` self-navigation observation, verbatim.
    pub self_nav_line: Option<String>,
    /// The shared host's own boot line, naming the plugins it loaded.
    pub host_booted_line: Option<String>,
}

/// Did the embedder's `frame-src` refuse a sandboxed srcdoc frame's attempt to
/// navigate itself cross-origin? `None` when the page never reported.
pub fn self_navigation_refused(evidence: &ProbeEvidence) -> Option<bool> {
    let line = evidence.self_nav_line.as_deref()?;
    let rest = line.split("observed=").nth(1)?;
    let value = rest.split_whitespace().next()?;
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Read one probe's evidence out of the plugin host journal. Pure, so the
/// interesting cases are unit-testable without a webview.
pub fn collect(journal: &[String]) -> ProbeEvidence {
    let mut evidence = ProbeEvidence::default();
    for line in journal {
        if line.starts_with(PROBE_MOUNTED_LINE) {
            evidence.mounted = true;
        }
        if line.starts_with(SELF_NAV_LINE_PREFIX) {
            evidence.self_nav_line = Some(line.clone());
        }
        if line.starts_with(HOST_BOOTED_LINE_PREFIX) {
            evidence.host_booted_line = Some(line.clone());
        }
        if line.contains("did not report ready") {
            evidence.never_ready_warnings.push(line.clone());
        }
        if let Some(id) = crate::plugins::bus::plugin_frame_ready_id(line) {
            if !evidence.ready_plugin_ids.iter().any(|seen| seen == id) {
                evidence.ready_plugin_ids.push(id.to_string());
            }
        }
    }
    evidence
}

/// The outcomes worth telling apart. Exactly one of them is a verdict about
/// WebKit; the other two are rig conditions and must never be reported as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeConclusion {
    /// The named plugin's frame reported ready: srcdoc frames boot here.
    Booted,
    /// The page mounted and the frame still never reported: the real failure
    /// this scenario exists to catch.
    FrameNeverBooted,
    /// The page itself never ran. Infrastructure, not evidence about frames.
    ProbeNeverMounted,
    /// The host booted without the required plugin in its loaded set -- a
    /// disabled built-in or an empty catalog. Also infrastructure: no frame
    /// was ever asked for, so nothing here is a verdict about frames either.
    PluginNeverLoaded,
}

pub fn conclude(evidence: &ProbeEvidence, required_plugin_id: &str) -> ProbeConclusion {
    if evidence
        .ready_plugin_ids
        .iter()
        .any(|id| id == required_plugin_id)
    {
        return ProbeConclusion::Booted;
    }
    if !evidence.mounted {
        return ProbeConclusion::ProbeNeverMounted;
    }
    match evidence.host_booted_line.as_deref() {
        Some(line) if !line.contains(required_plugin_id) => ProbeConclusion::PluginNeverLoaded,
        _ => ProbeConclusion::FrameNeverBooted,
    }
}

/// The embedder policy actually in force for this build, exactly as the config
/// carries it. Recorded in run.jsonl so a green run says WHICH policy it was
/// green under -- `csp: null` and `frame-src 'none'` are very different claims.
pub fn configured_csp(app: &AppHandle) -> Option<String> {
    app.config()
        .app
        .security
        .csp
        .as_ref()
        .map(ToString::to_string)
}

/// Which directives Tauri was told NOT to inject into the served policy.
/// Serialized rather than matched so this never has to track `tauri-utils`'
/// own enum shape (the crate is not a direct dependency here).
pub fn csp_modification_disabled(app: &AppHandle) -> serde_json::Value {
    serde_json::to_value(
        &app.config()
            .app
            .security
            .dangerous_disable_asset_csp_modification,
    )
    .unwrap_or(serde_json::Value::Null)
}

/// Is the probe route actually in this binary's embedded asset table? A
/// release-shaped frontend build strips `routes/dev/**`, and a `tauri dev`
/// build embeds nothing at all -- both are rig conditions with a specific fix,
/// not evidence that plugins are broken.
pub fn probe_asset_present(app: &AppHandle) -> bool {
    app.asset_resolver().get(PROBE_ASSET_KEY.to_string()).is_some()
}

pub fn open_probe_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(PROBE_LABEL).is_some() {
        return Ok(());
    }
    WebviewWindowBuilder::new(app, PROBE_LABEL, WebviewUrl::App(PROBE_ROUTE.into()))
        .title("Petal — Plugin boot probe (QA)")
        .inner_size(420.0, 260.0)
        .resizable(false)
        // Visible, but never focused: a hidden window is a needless variable in
        // the one measurement this scenario exists to make, and stealing focus
        // would disturb the share scenarios that follow.
        .focused(false)
        .skip_taskbar(true)
        .build()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub fn close_probe_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(PROBE_LABEL) {
        let _ = window.hide();
        let _ = window.close();
    }
}

/// Poll the journal until the required plugin reports ready, or the deadline
/// passes. Always waits out `SELF_NAV_GRACE` before returning a booted result
/// so the (non-gating) CSP-enforcement observation is in the record too.
pub async fn watch_journal(required_plugin_id: &str) -> ProbeEvidence {
    let started = std::time::Instant::now();
    loop {
        let evidence = collect(&crate::plugins::bus::journal::snapshot());
        let booted = conclude(&evidence, required_plugin_id) == ProbeConclusion::Booted;
        if booted && (evidence.self_nav_line.is_some() || started.elapsed() >= SELF_NAV_GRACE) {
            return evidence;
        }
        if started.elapsed() >= BOOT_TIMEOUT {
            return evidence;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(id: &str) -> String {
        format!("plugin {id} frame ready")
    }

    #[test]
    fn a_named_plugin_reporting_ready_is_the_pass() {
        let evidence = collect(&[
            format!("{PROBE_MOUNTED_LINE}; catalog=[petal.reactions]"),
            "host booted (Petal 0.9.9); loaded plugins: petal.reactions".to_string(),
            ready("petal.reactions"),
        ]);

        assert!(evidence.mounted);
        assert_eq!(evidence.ready_plugin_ids, vec!["petal.reactions"]);
        assert_eq!(
            conclude(&evidence, REQUIRED_PLUGIN_ID),
            ProbeConclusion::Booted
        );
    }

    #[test]
    fn another_plugin_booting_does_not_pass_for_the_required_one() {
        let evidence = collect(&[
            PROBE_MOUNTED_LINE.to_string(),
            ready("petal.something-else"),
        ]);

        assert_eq!(
            conclude(&evidence, REQUIRED_PLUGIN_ID),
            ProbeConclusion::FrameNeverBooted
        );
    }

    /// The host loaded nothing (disabled built-in / empty catalog) is a rig
    /// condition, not a WebKit verdict -- no frame was ever asked for.
    #[test]
    fn a_host_that_loaded_no_such_plugin_is_infrastructure_too() {
        let evidence = collect(&[
            PROBE_MOUNTED_LINE.to_string(),
            "host booted (Petal 0.9.9); loaded plugins: none".to_string(),
        ]);

        assert_eq!(
            conclude(&evidence, REQUIRED_PLUGIN_ID),
            ProbeConclusion::PluginNeverLoaded
        );
    }

    /// The whole point of the mounted marker: "WebKit blocked the frame" and
    /// "the probe page never ran" must not produce the same verdict.
    #[test]
    fn a_page_that_never_mounted_is_infrastructure_not_a_frame_verdict() {
        let blocked = collect(&[
            PROBE_MOUNTED_LINE.to_string(),
            "host booted (Petal 0.9.9); loaded plugins: petal.reactions".to_string(),
            "plugin petal.reactions: logic frame did not report ready within 5000 ms (frame never fired load; srcdoc blocked?)".to_string(),
        ]);
        assert_eq!(
            conclude(&blocked, REQUIRED_PLUGIN_ID),
            ProbeConclusion::FrameNeverBooted
        );
        assert_eq!(blocked.never_ready_warnings.len(), 1);

        let never_ran = collect(&["something entirely unrelated".to_string()]);
        assert_eq!(
            conclude(&never_ran, REQUIRED_PLUGIN_ID),
            ProbeConclusion::ProbeNeverMounted
        );
    }

    /// The journal is shared with every other host diagnostic, and plugin ids
    /// are attacker-adjacent text. Only a well-formed id counts.
    #[test]
    fn only_the_exact_ready_wording_and_a_valid_id_are_read_as_evidence() {
        let evidence = collect(&[
            PROBE_MOUNTED_LINE.to_string(),
            "plugin petal.reactions frame activated".to_string(),
            "plugin petal.reactions frame error: boom".to_string(),
            "plugin  frame ready".to_string(),
            "plugin not a plugin id frame ready".to_string(),
        ]);

        assert!(evidence.ready_plugin_ids.is_empty());
        assert_eq!(
            conclude(&evidence, REQUIRED_PLUGIN_ID),
            ProbeConclusion::FrameNeverBooted
        );
    }

    #[test]
    fn the_self_navigation_observation_is_read_in_both_directions() {
        let refused = collect(&[format!(
            "{SELF_NAV_LINE_PREFIX} frame-src violation observed=true blockedUri=https://plugin-boot-probe.invalid/"
        )]);
        assert_eq!(self_navigation_refused(&refused), Some(true));

        let not_refused = collect(&[format!(
            "{SELF_NAV_LINE_PREFIX} frame-src violation observed=false blockedUri=none"
        )]);
        assert_eq!(self_navigation_refused(&not_refused), Some(false));

        assert_eq!(self_navigation_refused(&ProbeEvidence::default()), None);
    }
}
