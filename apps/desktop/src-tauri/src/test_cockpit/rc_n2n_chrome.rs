//! RC-N2N-CHROME (#404): the RC-N2N native controller against a host sharing a
//! GOOGLE CHROME window whose keyboard focus sits in the ADDRESS BAR.
//!
//! The incident behind #404: a controller clicked into a web text field in a
//! Chrome window shared by a native host and typed, and nothing appeared. The
//! T1 measurement on that issue found why: keys the host posts to Chrome's pid
//! go to whichever view holds Chrome's OWN keyboard focus (the address bar, the
//! find bar, ...), and the host's AX click could move DOM focus onto the web
//! field without moving Chrome's view focus into the page (T1 case 8).
//!
//! RC-N2N's TextEdit target masks all of that: the scenario activates TextEdit
//! and sets `AXFocused` on the text area itself before driving. This variant
//! deliberately does neither. It launches its own Chrome (isolated
//! `--user-data-dir`, DevTools on an ephemeral port), loads a local page whose
//! only control is one `<input>` that fills the viewport, puts Chrome's
//! keyboard focus in the address bar, and then lets the controller's click and
//! typing do all the work through the real native controller route.
//!
//! The oracle is the page itself, read over the DevTools protocol:
//! `input.value` must equal the keystone exactly, and the address bar (read
//! over AX) must not have received it. Everything in this file that can be
//! decided without a live rig is pure and unit-tested; the live helpers are
//! macOS-only and live at the bottom.

use super::rc_n2n::{DrivenInput, GrantStatus, HostEffect, RcVerdict};

/// Element id of the page's single text field. The DevTools oracle reads it.
pub(crate) const INPUT_ELEMENT_ID: &str = "petal-rc-target";

/// Prefix of the per-run title marker. The peer finds the window by it, so it
/// must be unique per run (a Chrome left behind by a killed run can never be
/// picked up by the next one).
pub(crate) const MARKER_PREFIX: &str = "petal-rc-n2n-chrome-";

/// AX description Chrome gives its omnibox. Used to prefer the right text
/// field if Chrome ever exposes a second native one (find bar, side panel).
pub(crate) const OMNIBOX_AX_DESCRIPTION: &str = "Address and search bar";

/// How long the run waits, after the typing step, for the page field to reach
/// the keystone before it reads the verdict inputs. The host replay queue is
/// asynchronous; a single fixed settle would turn rig timing into a verdict.
pub(crate) const TYPED_VALUE_POLL_MS: u64 = 6_000;

/// The ordered drive plan: click into the field, type, release. No Command
/// chords on purpose: a Cmd+V would paste whatever the machine's clipboard
/// holds into the very field the oracle reads, and Cmd+A/C prove nothing here
/// that RC-N2N does not already prove.
pub(crate) fn keystone_steps() -> Vec<super::rc_n2n::DriveStep> {
    use super::rc_n2n::{DriveStep, KEYSTONE_TEXT};
    vec![
        DriveStep::Click,
        DriveStep::Type(KEYSTONE_TEXT.to_string()),
        DriveStep::ReleaseViaEscape,
    ]
}

/// The run's title marker, unique per run.
pub(crate) fn marker_for_run(run_id: &str) -> String {
    format!("{MARKER_PREFIX}{run_id}")
}

fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The local target page. One `<input>` that fills the whole viewport, so the
/// controller's click at the centre of the shared window (the RC-N2N click
/// plan) always lands inside it, below Chrome's toolbar. No `autofocus`: the
/// whole point is that nothing but the controller's click puts focus there.
/// A small CSS animation keeps the window drawing, because a static window
/// starves ScreenCaptureKit and the share would never read ready.
pub(crate) fn target_page_html(marker: &str) -> String {
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{title}</title>\n\
         <style>\n\
         html, body {{ margin: 0; height: 100%; background: #fff; }}\n\
         #{id} {{ position: fixed; inset: 0; width: 100vw; height: 100vh; box-sizing: border-box; \
         border: 0; padding: 0 24px; font: 40px/1.2 -apple-system, sans-serif; }}\n\
         #petal-rc-tick {{ position: fixed; right: 6px; bottom: 6px; width: 14px; height: 14px; \
         background: #c33; pointer-events: none; animation: petal-rc-spin 1s linear infinite; }}\n\
         @keyframes petal-rc-spin {{ to {{ transform: rotate(360deg); }} }}\n\
         </style></head>\n\
         <body><input id=\"{id}\" type=\"text\" autocomplete=\"off\" spellcheck=\"false\">\
         <div id=\"petal-rc-tick\"></div></body></html>\n",
        title = html_escape(marker),
        id = INPUT_ELEMENT_ID,
    )
}

/// Chrome's command line, minus the binary and the page URL. Headed (the host
/// must share a real on-screen window), its own profile, DevTools on an
/// ephemeral port (`0`: Chrome writes the port to `DevToolsActivePort` in the
/// profile, so two runs or another harness on 9222 can never collide), and the
/// flags that keep a backgrounded or occluded window rendering.
pub(crate) fn chrome_args(user_data_dir: &str) -> Vec<String> {
    vec![
        format!("--user-data-dir={user_data_dir}"),
        "--remote-debugging-port=0".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        // A fresh profile otherwise can raise the "Chrome Safe Storage"
        // keychain prompt and hang an unattended run.
        "--use-mock-keychain".to_string(),
        "--disable-renderer-backgrounding".to_string(),
        "--disable-background-timer-throttling".to_string(),
        "--disable-backgrounding-occluded-windows".to_string(),
        "--window-size=900,700".to_string(),
        "--window-position=80,80".to_string(),
    ]
}

/// Parse Chrome's `DevToolsActivePort` file: the port on the first line, the
/// browser target path on the second.
pub(crate) fn parse_devtools_active_port(contents: &str) -> Option<u16> {
    let port: u16 = contents.lines().next()?.trim().parse().ok()?;
    (port > 0).then_some(port)
}

/// Pick the page target carrying `marker` in its title from `/json/list`.
pub(crate) fn page_websocket_url(list: &serde_json::Value, marker: &str) -> Option<String> {
    list.as_array()?
        .iter()
        .filter(|target| target.get("type").and_then(|v| v.as_str()) == Some("page"))
        .find(|target| {
            target
                .get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|title| title.contains(marker))
        })
        .and_then(|target| target.get("webSocketDebuggerUrl"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The expression the oracle evaluates in the page.
pub(crate) fn page_state_expression() -> String {
    format!(
        "JSON.stringify((() => {{ const el = document.getElementById('{INPUT_ELEMENT_ID}'); \
         return {{ ready: document.readyState, hasInput: !!el, value: el ? el.value : null, \
         activeId: document.activeElement ? document.activeElement.id : null }}; }})())"
    )
}

/// What the page reported over DevTools.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PageState {
    pub ready: String,
    pub has_input: bool,
    pub value: Option<String>,
    pub active_id: Option<String>,
}

/// Decode a `Runtime.evaluate` response for `page_state_expression`. Any
/// protocol error or exception is an `Err`: "could not read the field" must
/// never collapse into "the field is empty".
pub(crate) fn parse_page_state_response(response: &serde_json::Value) -> Result<PageState, String> {
    if let Some(error) = response.get("error") {
        return Err(format!("DevTools error: {error}"));
    }
    let result = response
        .get("result")
        .ok_or("DevTools response carried no result")?;
    if let Some(exception) = result.get("exceptionDetails") {
        return Err(format!("page threw while reading the field: {exception}"));
    }
    let encoded = result
        .get("result")
        .and_then(|inner| inner.get("value"))
        .and_then(|value| value.as_str())
        .ok_or("DevTools result carried no string value")?;
    serde_json::from_str(encoded).map_err(|error| format!("page state was not JSON: {error}"))
}

/// Pick Chrome's BROWSER process for `user_data_dir` out of `ps -axo
/// pid=,command=` output: the line carrying our profile flag and no
/// `--type=` (renderer, GPU and utility helpers all carry one).
pub(crate) fn browser_pid_from_ps(ps_output: &str, user_data_dir: &str) -> Option<i32> {
    let flag = format!("--user-data-dir={user_data_dir}");
    ps_output.lines().find_map(|line| {
        let line = line.trim_start();
        if !line.split_whitespace().any(|token| token == flag) || line.contains("--type=") {
            return None;
        }
        line.split_whitespace().next()?.parse().ok()
    })
}

/// Every process (browser and helpers) still carrying our profile flag.
pub(crate) fn profile_pids_from_ps(ps_output: &str, user_data_dir: &str) -> Vec<i32> {
    let flag = format!("--user-data-dir={user_data_dir}");
    ps_output
        .lines()
        .filter(|line| line.split_whitespace().any(|token| token == flag))
        .filter_map(|line| line.trim_start().split_whitespace().next()?.parse().ok())
        .collect()
}

/// Everything the Chrome oracle looks at.
pub(crate) struct ChromeObservations<'a> {
    pub driven: &'a [DrivenInput],
    pub statuses: &'a [GrantStatus],
    pub host_effects: &'a [HostEffect],
    pub pressed_inputs_after: usize,
    pub sessions_after: usize,
    /// The page field's value after the typing settled; `Err` = unreadable.
    pub input_value: Result<&'a str, &'a str>,
    /// The address bar's AX value before the drive and after the typing.
    pub omnibox_before: &'a str,
    pub omnibox_after: Result<&'a str, &'a str>,
    pub expected_text: &'a str,
}

fn is_input(kind: &str) -> bool {
    matches!(kind, "pointer" | "key" | "text" | "wheel")
}

/// The RC-N2N-CHROME pass bar. Ordered so the most diagnostic failure wins:
/// an instrument that never drove anything is never reported as "the text did
/// not land", and keys that went to the address bar say so by name.
pub(crate) fn evaluate(obs: ChromeObservations<'_>) -> RcVerdict {
    if obs.driven.is_empty() {
        return RcVerdict::InfraFail(
            "the controller publish ledger is empty; the ledger never armed".to_string(),
        );
    }
    if !obs.driven.iter().any(|input| input.kind == "request") {
        return RcVerdict::InfraFail(
            "no control request reached the ledger; the scenario never asked for control"
                .to_string(),
        );
    }
    if !obs
        .statuses
        .iter()
        .any(|status| status.status == "active" && status.has_grant_token)
    {
        let seen: Vec<&str> = obs.statuses.iter().map(|s| s.status.as_str()).collect();
        return RcVerdict::TestFail(format!(
            "the host never granted control: no 'active' status carrying a grant token (saw: {})",
            if seen.is_empty() { "nothing".to_string() } else { seen.join(", ") }
        ));
    }
    let inputs: Vec<&DrivenInput> = obs.driven.iter().filter(|i| is_input(&i.kind)).collect();
    let pointer = |action: &str| {
        inputs
            .iter()
            .any(|i| i.kind == "pointer" && i.action.as_deref() == Some(action))
    };
    if !(pointer("down") && pointer("up")) {
        return RcVerdict::InfraFail(
            "the click gesture did not reach the control route (no pointer down+up pair in the \
             publish ledger)"
                .to_string(),
        );
    }
    let typed_keys = inputs
        .iter()
        .filter(|i| i.kind == "key" && i.action.as_deref() == Some("down") && !i.meta)
        .count();
    if typed_keys < obs.expected_text.chars().count() {
        return RcVerdict::InfraFail(format!(
            "the control route published {typed_keys} key downs for a {}-character keystone; \
             the typing drive did not run to completion",
            obs.expected_text.chars().count()
        ));
    }
    if !obs.driven.iter().any(|input| input.kind == "release") {
        return RcVerdict::TestFail(
            "Escape did not release control: the route published no release message".to_string(),
        );
    }
    if obs.host_effects.is_empty() {
        return RcVerdict::TestFail(format!(
            "the controller published {} inputs under a live grant and the host replayed none",
            inputs.len()
        ));
    }
    let omnibox_after = match obs.omnibox_after {
        Ok(value) => value,
        Err(error) => {
            return RcVerdict::InfraFail(format!(
                "could not read Chrome's address bar after the typing, so 'the keys did not go \
                 there' is unmeasured: {error}"
            ))
        }
    };
    let input_value = match obs.input_value {
        Ok(value) => value,
        Err(error) => {
            return RcVerdict::InfraFail(format!(
                "could not read the page field over DevTools: {error}"
            ))
        }
    };
    // The #404 signature first: the address bar took the keystrokes.
    if !obs.omnibox_before.contains(obs.expected_text) && omnibox_after.contains(obs.expected_text) {
        return RcVerdict::TestFail(format!(
            "the typed keys went to Chrome's ADDRESS BAR, not the web field the controller \
             clicked (#404): address bar read '{omnibox_after}', field read '{input_value}'"
        ));
    }
    if let Some(bad) = obs.host_effects.iter().find(|e| e.outcome != "applied") {
        return RcVerdict::TestFail(format!(
            "the host refused an input it had granted: kind={} action={:?} outcome={}",
            bad.kind, bad.action, bad.outcome
        ));
    }
    for input in &inputs {
        if !obs.host_effects.iter().any(|effect| {
            effect.kind == input.kind && effect.action.as_deref() == input.action.as_deref()
        }) {
            return RcVerdict::TestFail(format!(
                "the host never replayed a published {} {:?} input",
                input.kind, input.action
            ));
        }
    }
    if input_value != obs.expected_text {
        return RcVerdict::TestFail(format!(
            "the web field does not hold the typed text: expected exactly '{}', read \
             '{input_value}' (address bar read '{omnibox_after}'); the host reported every \
             input applied, which is the silent-success shape of #404",
            obs.expected_text
        ));
    }
    if obs.pressed_inputs_after > 0 {
        return RcVerdict::TestFail(format!(
            "{} pressed input(s) were still held on the host after the release",
            obs.pressed_inputs_after
        ));
    }
    if obs.sessions_after > 0 {
        return RcVerdict::TestFail(format!(
            "{} control session(s) survived the release on the host",
            obs.sessions_after
        ));
    }
    RcVerdict::Pass(format!(
        "with Chrome's keyboard focus in the address bar and no pre-activation, the native \
         controller's click and {} typed keys landed in the web field: input.value is exactly \
         '{}', the address bar never received it, and the release left no held input or session",
        typed_keys, obs.expected_text
    ))
}

// ---------------------------------------------------------------------------
// Live helpers (macOS only).
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(crate) mod live {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::string::CFString;
    use std::os::raw::c_void;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    type AXUIElementRef = *const c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFTypeRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementSetAttributeValue(
            element: AXUIElementRef,
            attribute: CFTypeRef,
            value: CFTypeRef,
        ) -> i32;
        fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout: f32) -> i32;
    }

    /// An owned AX element (released on drop).
    struct AxElement(CFType);

    impl AxElement {
        fn application(pid: i32) -> Option<Self> {
            // SAFETY: Create rule -- we own the returned reference.
            let raw = unsafe { AXUIElementCreateApplication(pid) };
            if raw.is_null() {
                return None;
            }
            let element = unsafe { CFType::wrap_under_create_rule(raw) };
            unsafe { AXUIElementSetMessagingTimeout(element.as_CFTypeRef(), 2.0) };
            Some(Self(element))
        }

        fn copy(&self, attribute: &str) -> Result<CFType, i32> {
            let name = CFString::new(attribute);
            let mut out: CFTypeRef = std::ptr::null();
            // SAFETY: Copy rule -- on success we own `out`.
            let err = unsafe {
                AXUIElementCopyAttributeValue(self.0.as_CFTypeRef(), name.as_CFTypeRef(), &mut out)
            };
            if err != 0 || out.is_null() {
                return Err(err);
            }
            Ok(unsafe { CFType::wrap_under_create_rule(out) })
        }

        fn string(&self, attribute: &str) -> Option<String> {
            self.copy(attribute)
                .ok()?
                .downcast::<CFString>()
                .map(|s| s.to_string())
        }

        fn boolean(&self, attribute: &str) -> Option<bool> {
            self.copy(attribute)
                .ok()?
                .downcast::<CFBoolean>()
                .map(bool::from)
        }

        fn children(&self) -> Vec<AxElement> {
            let Ok(value) = self.copy("AXChildren") else {
                return Vec::new();
            };
            let Some(array) = value.downcast::<CFArray>() else {
                return Vec::new();
            };
            array
                .iter()
                .filter(|item| !item.is_null())
                // SAFETY: Get rule -- retain each child we keep.
                .map(|item| AxElement(unsafe { CFType::wrap_under_get_rule(*item as CFTypeRef) }))
                .collect()
        }

        fn set_true(&self, attribute: &str) -> i32 {
            let name = CFString::new(attribute);
            unsafe {
                AXUIElementSetAttributeValue(
                    self.0.as_CFTypeRef(),
                    name.as_CFTypeRef(),
                    CFBoolean::true_value().as_CFTypeRef(),
                )
            }
        }
    }

    /// Find Chrome's omnibox: a native `AXTextField` outside every
    /// `AXWebArea` (web fields are AXTextFields too once web accessibility is
    /// on, so the web subtree is skipped, not just deprioritised). Bounded so
    /// a pathological tree cannot stall the run.
    fn find_omnibox(pid: i32) -> Result<AxElement, String> {
        let app = AxElement::application(pid).ok_or("AXUIElementCreateApplication failed")?;
        let mut queue: std::collections::VecDeque<(AxElement, usize)> =
            std::collections::VecDeque::new();
        queue.push_back((app, 0));
        let mut visited = 0usize;
        let mut fallback: Option<AxElement> = None;
        while let Some((element, depth)) = queue.pop_front() {
            visited += 1;
            if visited > 4000 {
                break;
            }
            let role = element.string("AXRole").unwrap_or_default();
            if role == "AXWebArea" {
                continue;
            }
            if role == "AXTextField" {
                let description = element.string("AXDescription").unwrap_or_default();
                if description == super::OMNIBOX_AX_DESCRIPTION {
                    return Ok(element);
                }
                if fallback.is_none() {
                    fallback = Some(element);
                }
                continue;
            }
            if depth < 30 {
                for child in element.children() {
                    queue.push_back((child, depth + 1));
                }
            }
        }
        fallback.ok_or_else(|| {
            format!("no native AXTextField (address bar) under Chrome pid {pid} ({visited} AX nodes)")
        })
    }

    pub(crate) fn omnibox_value(pid: i32) -> Result<String, String> {
        let omnibox = find_omnibox(pid)?;
        // An empty omnibox has no AXValue at all on some builds; that is a
        // real reading of "empty", not an unreadable one -- the element was
        // found.
        Ok(omnibox.string("AXValue").unwrap_or_default())
    }

    pub(crate) fn omnibox_is_focused(pid: i32) -> Result<bool, String> {
        let omnibox = find_omnibox(pid)?;
        omnibox
            .boolean("AXFocused")
            .ok_or_else(|| "the address bar has no readable AXFocused".to_string())
    }

    /// Put Chrome's keyboard focus in the address bar WITHOUT activating
    /// Chrome (AX `AXFocused`, the measured case-8 setup). Verified by reading
    /// the omnibox's own AXFocused back.
    pub(crate) fn focus_omnibox(pid: i32) -> Result<(), String> {
        let omnibox = find_omnibox(pid)?;
        let err = omnibox.set_true("AXFocused");
        std::thread::sleep(Duration::from_millis(300));
        match omnibox.boolean("AXFocused") {
            Some(true) => Ok(()),
            other => Err(format!(
                "setting AXFocused on the address bar returned {err} and it reads {other:?}"
            )),
        }
    }

    /// pid of the frontmost application, for evidence only.
    pub(crate) fn frontmost_pid() -> Option<i32> {
        use objc2_app_kit::NSWorkspace;
        let workspace = NSWorkspace::sharedWorkspace();
        let app = workspace.frontmostApplication()?;
        Some(app.processIdentifier())
    }

    /// The test-launched Chrome. Dropping it kills every process carrying its
    /// profile and deletes the run directory, so every exit path -- early
    /// returns and panics included -- cleans up.
    pub(crate) struct ChromeTarget {
        pub marker: String,
        pub run_dir: PathBuf,
        pub user_data_dir: PathBuf,
        pub page_path: PathBuf,
        pub browser_pid: i32,
        pub devtools_port: u16,
        child: Option<Child>,
    }

    fn ps_snapshot() -> String {
        Command::new("/bin/ps")
            .args(["-axww", "-o", "pid=,command="])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default()
    }

    impl ChromeTarget {
        pub(crate) async fn launch(
            chrome: &Path,
            marker: String,
            log_path: &Path,
        ) -> Result<Self, String> {
            if !chrome.exists() {
                return Err(format!("Google Chrome not found at {}", chrome.display()));
            }
            let run_dir = std::env::temp_dir().join(&marker);
            let user_data_dir = run_dir.join("profile");
            let page_path = run_dir.join("target.html");
            std::fs::create_dir_all(&user_data_dir)
                .map_err(|error| format!("create Chrome profile dir: {error}"))?;
            std::fs::write(&page_path, super::target_page_html(&marker))
                .map_err(|error| format!("write target page: {error}"))?;
            let log = std::fs::File::create(log_path)
                .map_err(|error| format!("create Chrome log: {error}"))?;
            let log_err = log
                .try_clone()
                .map_err(|error| format!("clone Chrome log: {error}"))?;
            let profile = user_data_dir.display().to_string();
            let mut command = super::super::chrome_command(chrome);
            command
                .args(super::chrome_args(&profile))
                .arg(format!("file://{}", page_path.display()))
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err));
            let child = command
                .spawn()
                .map_err(|error| format!("launch Google Chrome: {error}"))?;
            let mut target = ChromeTarget {
                marker,
                run_dir,
                user_data_dir,
                page_path,
                browser_pid: 0,
                devtools_port: 0,
                child: Some(child),
            };
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if target.devtools_port == 0 {
                    if let Ok(text) =
                        std::fs::read_to_string(target.user_data_dir.join("DevToolsActivePort"))
                    {
                        target.devtools_port = super::parse_devtools_active_port(&text).unwrap_or(0);
                    }
                }
                if target.browser_pid == 0 {
                    target.browser_pid =
                        super::browser_pid_from_ps(&ps_snapshot(), &profile).unwrap_or(0);
                }
                if target.devtools_port != 0 && target.browser_pid != 0 {
                    if let Ok(state) = target.page_state().await {
                        if state.has_input && state.ready == "complete" {
                            return Ok(target);
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "Chrome never served the target page over DevTools (port={}, pid={})",
                        target.devtools_port, target.browser_pid
                    ));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }

        async fn page_ws_url(&self) -> Result<String, String> {
            let url = format!("http://127.0.0.1:{}/json/list", self.devtools_port);
            let list: serde_json::Value = reqwest::Client::new()
                .get(&url)
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .map_err(|error| format!("GET {url}: {error}"))?
                .json()
                .await
                .map_err(|error| format!("DevTools target list: {error}"))?;
            super::page_websocket_url(&list, &self.marker)
                .ok_or_else(|| format!("no DevTools page titled '*{}*'", self.marker))
        }

        /// Read the page's field over DevTools (`Runtime.evaluate`).
        pub(crate) async fn page_state(&self) -> Result<super::PageState, String> {
            use futures_util::{SinkExt, StreamExt};
            use tokio_tungstenite::tungstenite::Message;
            let ws_url = self.page_ws_url().await?;
            let (mut socket, _) = tokio::time::timeout(
                Duration::from_secs(3),
                tokio_tungstenite::connect_async(&ws_url),
            )
            .await
            .map_err(|_| "DevTools websocket connect timed out".to_string())?
            .map_err(|error| format!("DevTools websocket: {error}"))?;
            let request = serde_json::json!({
                "id": 1,
                "method": "Runtime.evaluate",
                "params": { "expression": super::page_state_expression(), "returnByValue": true },
            });
            socket
                .send(Message::Text(request.to_string().into()))
                .await
                .map_err(|error| format!("DevTools send: {error}"))?;
            let deadline = Instant::now() + Duration::from_secs(3);
            let result = loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let next = tokio::time::timeout(remaining, socket.next())
                    .await
                    .map_err(|_| "DevTools evaluate timed out".to_string())?;
                let Some(message) = next else {
                    break Err("DevTools socket closed before answering".to_string());
                };
                let message = message.map_err(|error| format!("DevTools read: {error}"))?;
                if let Message::Text(text) = message {
                    let value: serde_json::Value = serde_json::from_str(text.as_str())
                        .map_err(|error| format!("DevTools reply was not JSON: {error}"))?;
                    if value.get("id").and_then(|id| id.as_u64()) == Some(1) {
                        break super::parse_page_state_response(&value);
                    }
                }
            };
            let _ = socket.close(None).await;
            result
        }

        fn shutdown(&mut self) {
            let profile = self.user_data_dir.display().to_string();
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            // Helpers normally die with the browser; sweep any straggler that
            // still carries this run's profile flag (and nothing else).
            for _ in 0..20 {
                let pids = super::profile_pids_from_ps(&ps_snapshot(), &profile);
                if pids.is_empty() {
                    break;
                }
                for pid in pids {
                    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            let _ = std::fs::remove_dir_all(&self.run_dir);
        }
    }

    impl Drop for ChromeTarget {
        fn drop(&mut self) {
            self.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_cockpit::rc_n2n::{DriveStep, KEYSTONE_TEXT};

    fn input(kind: &str, action: Option<&str>, key: Option<&str>) -> DrivenInput {
        DrivenInput {
            kind: kind.to_string(),
            action: action.map(str::to_string),
            key: key.map(str::to_string),
            meta: false,
            t_ms: 0,
        }
    }

    fn effect(kind: &str, action: Option<&str>, outcome: &str) -> HostEffect {
        HostEffect {
            kind: kind.to_string(),
            action: action.map(str::to_string),
            key: None,
            outcome: outcome.to_string(),
            t_ms: 0,
        }
    }

    fn driven() -> Vec<DrivenInput> {
        let mut out = vec![
            input("request", None, None),
            input("pointer", Some("down"), None),
            input("pointer", Some("up"), None),
        ];
        for ch in KEYSTONE_TEXT.chars() {
            out.push(input("key", Some("down"), Some(&ch.to_string())));
            out.push(input("key", Some("up"), Some(&ch.to_string())));
        }
        out.push(input("release", None, None));
        out
    }

    fn effects() -> Vec<HostEffect> {
        vec![
            effect("pointer", Some("down"), "applied"),
            effect("pointer", Some("up"), "applied"),
            effect("key", Some("down"), "applied"),
            effect("key", Some("up"), "applied"),
        ]
    }

    fn granted() -> Vec<GrantStatus> {
        vec![GrantStatus {
            status: "active".to_string(),
            has_grant_token: true,
            t_ms: 0,
        }]
    }

    fn obs<'a>(
        driven: &'a [DrivenInput],
        statuses: &'a [GrantStatus],
        effects: &'a [HostEffect],
        input_value: Result<&'a str, &'a str>,
        omnibox_after: Result<&'a str, &'a str>,
    ) -> ChromeObservations<'a> {
        ChromeObservations {
            driven,
            statuses,
            host_effects: effects,
            pressed_inputs_after: 0,
            sessions_after: 0,
            input_value,
            omnibox_before: "file:///tmp/x/target.html",
            omnibox_after,
            expected_text: KEYSTONE_TEXT,
        }
    }

    #[test]
    fn exact_value_with_a_clean_address_bar_passes() {
        let (d, s, e) = (driven(), granted(), effects());
        let verdict = evaluate(obs(&d, &s, &e, Ok(KEYSTONE_TEXT), Ok("file:///tmp/x/target.html")));
        assert!(matches!(verdict, RcVerdict::Pass(_)), "{verdict:?}");
    }

    #[test]
    fn keys_in_the_address_bar_are_the_404_failure_named_as_such() {
        let (d, s, e) = (driven(), granted(), effects());
        let omnibox = format!("file:///tmp/x/target.html{KEYSTONE_TEXT}");
        match evaluate(obs(&d, &s, &e, Ok(""), Ok(&omnibox))) {
            RcVerdict::TestFail(detail) => {
                assert!(detail.contains("ADDRESS BAR"), "{detail}");
                assert!(detail.contains("#404"), "{detail}");
            }
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn the_address_bar_check_wins_even_when_the_host_reported_everything_applied() {
        // The silent-success shape: every input 'applied', field empty.
        let (d, s, e) = (driven(), granted(), effects());
        match evaluate(obs(&d, &s, &e, Ok(""), Ok(KEYSTONE_TEXT))) {
            RcVerdict::TestFail(detail) => assert!(detail.contains("ADDRESS BAR"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_field_with_a_clean_address_bar_is_still_a_failure() {
        let (d, s, e) = (driven(), granted(), effects());
        match evaluate(obs(&d, &s, &e, Ok(""), Ok("file:///tmp/x/target.html"))) {
            RcVerdict::TestFail(detail) => assert!(detail.contains("expected exactly"), "{detail}"),
            other => panic!("expected TestFail, got {other:?}"),
        }
    }

    #[test]
    fn a_partial_or_padded_value_is_not_exact() {
        let (d, s, e) = (driven(), granted(), effects());
        for value in ["petalrcnativ", "xpetalrcnative", "petalrcnativepetalrcnative"] {
            let verdict = evaluate(obs(&d, &s, &e, Ok(value), Ok("file:///t")));
            assert!(matches!(verdict, RcVerdict::TestFail(_)), "{value}: {verdict:?}");
        }
    }

    #[test]
    fn an_unreadable_field_is_unmeasured_never_empty() {
        let (d, s, e) = (driven(), granted(), effects());
        let verdict = evaluate(obs(&d, &s, &e, Err("socket closed"), Ok("file:///t")));
        assert!(matches!(verdict, RcVerdict::InfraFail(_)), "{verdict:?}");
    }

    #[test]
    fn an_unreadable_address_bar_is_unmeasured() {
        let (d, s, e) = (driven(), granted(), effects());
        let verdict = evaluate(obs(&d, &s, &e, Ok(KEYSTONE_TEXT), Err("no AX")));
        assert!(matches!(verdict, RcVerdict::InfraFail(_)), "{verdict:?}");
    }

    #[test]
    fn a_short_typing_drive_is_an_instrument_failure() {
        let mut d = driven();
        let mut seen = 0;
        d.retain(|i| {
            if i.kind == "key" && i.action.as_deref() == Some("down") {
                seen += 1;
                return seen <= 3;
            }
            true
        });
        let (s, e) = (granted(), effects());
        let verdict = evaluate(obs(&d, &s, &e, Ok(""), Ok("file:///t")));
        assert!(matches!(verdict, RcVerdict::InfraFail(_)), "{verdict:?}");
    }

    #[test]
    fn no_grant_is_a_product_failure() {
        let (d, e) = (driven(), effects());
        let verdict = evaluate(obs(&d, &[], &e, Ok(KEYSTONE_TEXT), Ok("file:///t")));
        assert!(matches!(verdict, RcVerdict::TestFail(_)), "{verdict:?}");
    }

    #[test]
    fn a_refused_input_fails_and_held_state_after_release_fails() {
        let (d, s) = (driven(), granted());
        let mut e = effects();
        e.push(effect("key", Some("down"), "replayFailed"));
        assert!(matches!(
            evaluate(obs(&d, &s, &e, Ok(KEYSTONE_TEXT), Ok("file:///t"))),
            RcVerdict::TestFail(_)
        ));
        let e = effects();
        let mut o = obs(&d, &s, &e, Ok(KEYSTONE_TEXT), Ok("file:///t"));
        o.pressed_inputs_after = 1;
        assert!(matches!(evaluate(o), RcVerdict::TestFail(_)));
        let mut o = obs(&d, &s, &e, Ok(KEYSTONE_TEXT), Ok("file:///t"));
        o.sessions_after = 1;
        assert!(matches!(evaluate(o), RcVerdict::TestFail(_)));
    }

    #[test]
    fn the_plan_is_click_type_release_with_no_clipboard_chords() {
        let steps = keystone_steps();
        assert_eq!(
            steps,
            vec![
                DriveStep::Click,
                DriveStep::Type(KEYSTONE_TEXT.to_string()),
                DriveStep::ReleaseViaEscape
            ]
        );
    }

    #[test]
    fn page_has_one_unfocused_input_and_escapes_the_marker() {
        let html = target_page_html("petal-rc-n2n-chrome-1<x>");
        assert!(html.contains(&format!("id=\"{INPUT_ELEMENT_ID}\"")), "{html}");
        assert!(html.contains("<title>petal-rc-n2n-chrome-1&lt;x&gt;</title>"), "{html}");
        assert!(!html.contains("autofocus"), "nothing but the click may focus it: {html}");
        assert!(html.contains("100vh"), "the field must cover the click point: {html}");
        assert!(html.contains("animation"), "the window must keep drawing for SCK: {html}");
        assert!(!html.contains(KEYSTONE_TEXT), "the page must not seed the keystone");
    }

    #[test]
    fn marker_is_per_run_and_does_not_contain_the_keystone() {
        let marker = marker_for_run("1785084128968");
        assert_eq!(marker, "petal-rc-n2n-chrome-1785084128968");
        assert!(!marker.contains(KEYSTONE_TEXT));
    }

    #[test]
    fn chrome_args_are_isolated_headed_and_on_an_ephemeral_devtools_port() {
        let args = chrome_args("/tmp/p");
        assert!(args.contains(&"--user-data-dir=/tmp/p".to_string()));
        assert!(args.contains(&"--remote-debugging-port=0".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--headless")), "{args:?}");
        assert!(!args.iter().any(|a| a.contains("9222")), "{args:?}");
    }

    #[test]
    fn devtools_active_port_parses_the_first_line() {
        assert_eq!(parse_devtools_active_port("58258\n/devtools/browser/abc\n"), Some(58258));
        assert_eq!(parse_devtools_active_port(""), None);
        assert_eq!(parse_devtools_active_port("0\n"), None);
        assert_eq!(parse_devtools_active_port("x\n"), None);
    }

    #[test]
    fn page_target_is_picked_by_marker_and_type() {
        let list = serde_json::json!([
            {"type": "service_worker", "title": "petal-rc-n2n-chrome-1", "webSocketDebuggerUrl": "ws://sw"},
            {"type": "page", "title": "New Tab", "webSocketDebuggerUrl": "ws://other"},
            {"type": "page", "title": "petal-rc-n2n-chrome-1", "webSocketDebuggerUrl": "ws://ours"}
        ]);
        assert_eq!(
            page_websocket_url(&list, "petal-rc-n2n-chrome-1").as_deref(),
            Some("ws://ours")
        );
        assert_eq!(page_websocket_url(&list, "petal-rc-n2n-chrome-2"), None);
    }

    #[test]
    fn page_state_response_parses_and_errors_stay_errors() {
        let ok = serde_json::json!({"id": 1, "result": {"result": {"type": "string",
            "value": "{\"ready\":\"complete\",\"hasInput\":true,\"value\":\"abc\",\"activeId\":\"petal-rc-target\"}"}}});
        let state = parse_page_state_response(&ok).unwrap();
        assert_eq!(state.value.as_deref(), Some("abc"));
        assert!(state.has_input);
        let thrown = serde_json::json!({"id": 1, "result": {"result": {}, "exceptionDetails": {"text": "x"}}});
        assert!(parse_page_state_response(&thrown).is_err());
        let protocol = serde_json::json!({"id": 1, "error": {"message": "nope"}});
        assert!(parse_page_state_response(&protocol).is_err());
        let state_expr = page_state_expression();
        assert!(state_expr.contains(INPUT_ELEMENT_ID));
    }

    #[test]
    fn browser_pid_skips_helpers_and_other_profiles() {
        let ps = "  100 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n\
                  \x20 200 /Applications/Google Chrome.app/Contents/Frameworks/Helper --type=renderer --user-data-dir=/tmp/p\n\
                  \x20 300 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --user-data-dir=/tmp/p --remote-debugging-port=0\n\
                  \x20 400 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --user-data-dir=/tmp/other\n\
                  \x20 500 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --user-data-dir=/tmp/pp\n";
        assert_eq!(browser_pid_from_ps(ps, "/tmp/p"), Some(300));
        assert_eq!(profile_pids_from_ps(ps, "/tmp/p"), vec![200, 300]);
        assert_eq!(browser_pid_from_ps(ps, "/tmp/none"), None);
    }
}
