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

/// Element id of `RC-N2N-CHROME-BUTTON`'s button, the click target there.
pub(crate) const PRESS_ELEMENT_ID: &str = "petal-rc-press";

/// AX description Chrome gives its omnibox. Used to prefer the right text
/// field if Chrome ever exposes a second native one (find bar, side panel).
pub(crate) const OMNIBOX_AX_DESCRIPTION: &str = "Address and search bar";

/// How long the run waits, after the typing step, for the page field to reach
/// the keystone before it reads the verdict inputs. The host replay queue is
/// asynchronous; a single fixed settle would turn rig timing into a verdict.
pub(crate) const TYPED_VALUE_POLL_MS: u64 = 6_000;

/// The two page shapes the case runs, each its own scenario with its own
/// keystone and oracle. In the #404 incident every click took the host's
/// `AxPressable` route, and it is not known whether the pressed element was
/// the `<input>` itself or a pressable container around it, so both shapes
/// must pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageShape {
    /// `RC-N2N-CHROME`: one bare `<input>` filling the viewport.
    BareInput,
    /// `RC-N2N-CHROME-FORM`: a realistic application-form field -- a
    /// `<label>` wrapping a styled, clickable `role="group"` box (padding,
    /// border, icon, click handler that focuses the input) containing an
    /// `<input>` with a placeholder.
    FormField,
    /// `RC-N2N-CHROME-BUTTON` (#404 follow-up): the click lands on a page
    /// `<button>` whose click handler focuses the `<input>`, then the
    /// controller types. In the #404 incident every click was `mode=AxPressable
    /// action=Press` on page buttons/links; the two shapes above both land on
    /// the field's text and take the host's AxText route instead. The page
    /// records whether the button saw a real-shaped pointer press (`pointerdown`
    /// with pressure, `mousedown` with a click count), so a run
    /// that fell back to a real click cannot pass as AxPressable coverage.
    ButtonFocus,
}

impl PageShape {
    pub(crate) fn for_scenario(scenario_id: &str) -> Option<Self> {
        match scenario_id {
            "RC-N2N-CHROME" => Some(Self::BareInput),
            "RC-N2N-CHROME-FORM" => Some(Self::FormField),
            "RC-N2N-CHROME-BUTTON" => Some(Self::ButtonFocus),
            _ => None,
        }
    }

    /// Typed by the controller. Distinct per shape, letters only (see
    /// `rc_n2n::KEYSTONE_TEXT`), so one shape's evidence can never satisfy the
    /// other's oracle.
    pub(crate) fn keystone(self) -> &'static str {
        match self {
            Self::BareInput => super::rc_n2n::KEYSTONE_TEXT,
            Self::FormField => "petalrcwebform",
            Self::ButtonFocus => "petalrcwebbutton",
        }
    }

    /// Prefix of the per-run title marker. The peer finds the window by it,
    /// so it must be unique per run (a Chrome left behind by a killed run can
    /// never be picked up by the next one).
    pub(crate) fn marker_prefix(self) -> &'static str {
        match self {
            Self::BareInput => "petal-rc-n2n-chrome-",
            Self::FormField => "petal-rc-n2n-chromeform-",
            Self::ButtonFocus => "petal-rc-n2n-chromebutton-",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::BareInput => "bare-input",
            Self::FormField => "form-field",
            Self::ButtonFocus => "button-then-type",
        }
    }

    /// The element the controller clicks.
    pub(crate) fn click_target_id(self) -> &'static str {
        match self {
            Self::BareInput | Self::FormField => INPUT_ELEMENT_ID,
            Self::ButtonFocus => PRESS_ELEMENT_ID,
        }
    }

    /// Whether the oracle requires the click to have been an AX press (no
    /// real pointer on the button), i.e. this shape exists to cover the
    /// host's AxPressable route.
    pub(crate) fn requires_ax_press(self) -> bool {
        matches!(self, Self::ButtonFocus)
    }
}

/// The ordered drive plan: click into the field's visible text area, type,
/// release. No Command chords on purpose: a Cmd+V would paste whatever the
/// machine's clipboard holds into the very field the oracle reads, and
/// Cmd+A/C prove nothing here that RC-N2N does not already prove.
pub(crate) fn keystone_steps(shape: PageShape, click: (u32, u32)) -> Vec<super::rc_n2n::DriveStep> {
    use super::rc_n2n::DriveStep;
    vec![
        DriveStep::ClickAt {
            x_permille: click.0,
            y_permille: click.1,
        },
        DriveStep::Type(shape.keystone().to_string()),
        DriveStep::Release,
    ]
}

/// The run's title marker, unique per run.
pub(crate) fn marker_for_run(shape: PageShape, run_id: &str) -> String {
    format!("{}{run_id}", shape.marker_prefix())
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

const PAGE_TICK_CSS: &str = "#petal-rc-tick { position: fixed; right: 6px; bottom: 6px; \
     width: 14px; height: 14px; background: #c33; pointer-events: none; \
     animation: petal-rc-spin 1s linear infinite; }\n\
     @keyframes petal-rc-spin { to { transform: rotate(360deg); } }\n";

/// The local target page for `shape`. No `autofocus` in either shape: the
/// whole point is that nothing but the controller's click puts focus there.
/// A small CSS animation keeps the window drawing, because a static window
/// starves ScreenCaptureKit and the share would never read ready.
pub(crate) fn target_page_html(shape: PageShape, marker: &str) -> String {
    let (style, body) = match shape {
        PageShape::BareInput => (
            format!(
                "html, body {{ margin: 0; height: 100%; background: #fff; }}\n\
                 #{id} {{ position: fixed; inset: 0; width: 100vw; height: 100vh; \
                 box-sizing: border-box; border: 0; padding: 0 24px; \
                 font: 40px/1.2 -apple-system, sans-serif; }}\n",
                id = INPUT_ELEMENT_ID
            ),
            format!(
                "<input id=\"{id}\" type=\"text\" autocomplete=\"off\" spellcheck=\"false\">",
                id = INPUT_ELEMENT_ID
            ),
        ),
        PageShape::FormField => (
            format!(
                "html, body {{ margin: 0; height: 100%; background: #f4f5f7; \
                 font: 15px/1.4 -apple-system, sans-serif; color: #1d1f24; }}\n\
                 main {{ max-width: 640px; margin: 0 auto; padding: 28px 32px; }}\n\
                 h1 {{ font-size: 22px; margin: 0 0 4px; }} p {{ margin: 0 0 22px; color: #5b616e; }}\n\
                 .field {{ display: block; margin: 0 0 18px; font-weight: 600; }}\n\
                 .field-box {{ display: flex; align-items: center; gap: 10px; margin-top: 6px; \
                 padding: 12px 14px; background: #fff; border: 1px solid #c9ced8; \
                 border-radius: 8px; cursor: text; font-weight: 400; }}\n\
                 .field-box:focus-within {{ border-color: #3b6ef5; box-shadow: 0 0 0 3px #3b6ef533; }}\n\
                 .field-icon {{ color: #8a90a0; }}\n\
                 #{id} {{ flex: 1; min-width: 0; border: 0; outline: 0; padding: 4px 0; \
                 font: inherit; font-size: 17px; background: transparent; }}\n",
                id = INPUT_ELEMENT_ID
            ),
            format!(
                "<main><h1>Job application</h1><p>Tell us a little about yourself.</p>\
                 <form onsubmit=\"return false\">\
                 <label class=\"field\">Full name\
                 <div class=\"field-box\" role=\"group\" tabindex=\"-1\" \
                 onclick=\"this.querySelector('input').focus()\">\
                 <span class=\"field-icon\" aria-hidden=\"true\">&#9998;</span>\
                 <input id=\"{id}\" name=\"fullname\" type=\"text\" placeholder=\"Jane Doe\" \
                 autocomplete=\"off\" spellcheck=\"false\"></div></label>\
                 <label class=\"field\">Email\
                 <div class=\"field-box\" role=\"group\"><input name=\"email\" type=\"email\" \
                 placeholder=\"jane@example.com\" autocomplete=\"off\" \
                 style=\"flex:1;border:0;outline:0;font:inherit;background:transparent\"></div></label>\
                 </form></main>",
                id = INPUT_ELEMENT_ID
            ),
        ),
        PageShape::ButtonFocus => (
            format!(
                "html, body {{ margin: 0; height: 100%; background: #f4f5f7; \
                 font: 15px/1.4 -apple-system, sans-serif; color: #1d1f24; }}\n\
                 main {{ max-width: 640px; margin: 0 auto; padding: 28px 32px; }}\n\
                 h1 {{ font-size: 22px; margin: 0 0 18px; }}\n\
                 #{press} {{ display: block; width: 100%; padding: 28px; font: inherit; \
                 font-size: 22px; border: 1px solid #3b6ef5; border-radius: 10px; \
                 background: #3b6ef5; color: #fff; cursor: pointer; }}\n\
                 #{id} {{ display: block; width: 100%; box-sizing: border-box; margin-top: 18px; \
                 padding: 10px 12px; font: inherit; font-size: 17px; border: 1px solid #c9ced8; \
                 border-radius: 8px; }}\n",
                id = INPUT_ELEMENT_ID,
                press = PRESS_ELEMENT_ID
            ),
            format!(
                "<main><h1>Profile</h1>\
                 <button id=\"{press}\" type=\"button\">Edit full name</button>\
                 <input id=\"{id}\" name=\"fullname\" type=\"text\" autocomplete=\"off\" \
                 spellcheck=\"false\" placeholder=\"Full name\"></main>\
                 <script>window.__petalPress = {{ clicks: 0, pointerDowns: 0, mouseDowns: 0 }};\
                 const b = document.getElementById('{press}');\
                 b.addEventListener('pointerdown', (e) => {{ if (e.pressure > 0) window.__petalPress.pointerDowns++; }});\
                 b.addEventListener('mousedown', (e) => {{ if (e.detail > 0) window.__petalPress.mouseDowns++; }});\
                 b.addEventListener('click', () => {{ window.__petalPress.clicks++; \
                 document.getElementById('{id}').focus(); }});</script>",
                id = INPUT_ELEMENT_ID,
                press = PRESS_ELEMENT_ID
            ),
        ),
    };
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{title}</title>\n\
         <style>\n{style}{tick}</style></head>\n<body>{body}<div id=\"petal-rc-tick\"></div>\
         </body></html>\n",
        title = html_escape(marker),
        tick = PAGE_TICK_CSS,
    )
}

/// Where the click must land, as read from the page over DevTools: Chrome's
/// outer window size, its viewport size, and the target input's rect and left
/// padding, all in CSS pixels (= points at 100% zoom).
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClickGeometry {
    pub outer_width: f64,
    pub outer_height: f64,
    pub inner_width: f64,
    pub inner_height: f64,
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
    pub padding_left: f64,
}

pub(crate) fn click_geometry_expression(shape: PageShape) -> String {
    let target = shape.click_target_id();
    format!(
        "JSON.stringify((() => {{ const el = document.getElementById('{target}'); \
         const r = el.getBoundingClientRect(); \
         return {{ outerWidth: window.outerWidth, outerHeight: window.outerHeight, \
         innerWidth: window.innerWidth, innerHeight: window.innerHeight, left: r.left, \
         top: r.top, width: r.width, height: r.height, \
         paddingLeft: parseFloat(getComputedStyle(el).paddingLeft) || 0 }}; }})())"
    )
}

/// The click point in thousandths of the SHARED WINDOW (the whole Chrome
/// window, toolbar included -- that is what the controller's overlay maps
/// to). Aimed at the input's visible text area: a little right of its left
/// padding, at its vertical centre. Chrome on macOS has all its chrome on top,
/// so the viewport's offset inside the window is the size difference.
/// `None` when the geometry cannot place a point inside the window.
pub(crate) fn click_permille(shape: PageShape, geometry: &ClickGeometry) -> Option<(u32, u32)> {
    let g = geometry;
    if !(g.outer_width > 0.0 && g.outer_height > 0.0 && g.width > 0.0 && g.height > 0.0) {
        return None;
    }
    let offset_x = (g.outer_width - g.inner_width).max(0.0);
    let offset_y = (g.outer_height - g.inner_height).max(0.0);
    // A text field is aimed at its text area; a button at its centre.
    let inset_x = if shape.requires_ax_press() {
        g.width / 2.0
    } else {
        (g.padding_left + 24.0).min(g.width / 2.0)
    };
    let x = offset_x + g.left + inset_x;
    let y = offset_y + g.top + g.height / 2.0;
    if !(x > 0.0 && x < g.outer_width && y > offset_y && y < g.outer_height) {
        return None;
    }
    Some((
        (x * 1000.0 / g.outer_width).round() as u32,
        (y * 1000.0 / g.outer_height).round() as u32,
    ))
}

/// Decode a `Runtime.evaluate` response whose expression returned a JSON
/// string. Any protocol error or exception is an `Err`: "could not read the
/// page" must never collapse into a value.
pub(crate) fn parse_json_eval_response<T: serde::de::DeserializeOwned>(
    response: &serde_json::Value,
) -> Result<T, String> {
    if let Some(error) = response.get("error") {
        return Err(format!("DevTools error: {error}"));
    }
    let result = response
        .get("result")
        .ok_or("DevTools response carried no result")?;
    if let Some(exception) = result.get("exceptionDetails") {
        return Err(format!("page threw while being read: {exception}"));
    }
    let encoded = result
        .get("result")
        .and_then(|inner| inner.get("value"))
        .and_then(|value| value.as_str())
        .ok_or("DevTools result carried no string value")?;
    serde_json::from_str(encoded)
        .map_err(|error| format!("page answer was not the expected JSON: {error}"))
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
         activeId: document.activeElement ? document.activeElement.id : null, \
         press: window.__petalPress || null }}; }})())"
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
    /// `RC-N2N-CHROME-BUTTON` only: what the button saw. `None` elsewhere.
    #[serde(default)]
    pub press: Option<PressRecord>,
}

/// The button's own event counts. Chrome's `AXPress` (verified on Chrome 155)
/// does dispatch trusted `pointerdown`/`mousedown`/`click`, but shaped as a
/// simulated click: `pointerdown.pressure == 0` and `mousedown.detail == 0`.
/// A real click (session tap, SkyLight) presses with `pressure > 0` and a
/// `mousedown` click count `detail >= 1`. The page counts ONLY real-shaped
/// downs, so `pointer_downs`/`mouse_downs` > 0 means a real pointer click.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PressRecord {
    pub clicks: u32,
    pub pointer_downs: u32,
    pub mouse_downs: u32,
}

/// Decode a `Runtime.evaluate` response for `page_state_expression`. Any
/// protocol error or exception is an `Err`: "could not read the field" must
/// never collapse into "the field is empty".
pub(crate) fn parse_page_state_response(response: &serde_json::Value) -> Result<PageState, String> {
    parse_json_eval_response(response)
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
    /// `Some` when the shape requires the click to be an AX press: what the
    /// button recorded (`None` inside = the page reported nothing).
    pub required_ax_press: Option<Option<&'a PressRecord>>,
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
            if seen.is_empty() {
                "nothing".to_string()
            } else {
                seen.join(", ")
            }
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
            "the release never reached the wire: the controller published no release message"
                .to_string(),
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
    if !obs.omnibox_before.contains(obs.expected_text) && omnibox_after.contains(obs.expected_text)
    {
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
    if let Some(press) = obs.required_ax_press {
        match press {
            None => {
                return RcVerdict::InfraFail(
                    "the page did not report the button's event counts".to_string(),
                )
            }
            Some(press) if press.clicks == 0 => {
                return RcVerdict::TestFail(format!(
                    "the controller's click never reached the page button (no click event); \
                     field read '{input_value}'"
                ))
            }
            Some(press) if press.pointer_downs > 0 || press.mouse_downs > 0 => {
                return RcVerdict::InfraFail(format!(
                    "the button received a real pointer click (pointerdown={}, mousedown={}), \
                     not an AX press, so this run did not cover the host's AxPressable route",
                    press.pointer_downs, press.mouse_downs
                ))
            }
            Some(_) => {}
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
        fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFTypeRef) -> i32;
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

        fn action_names(&self) -> Vec<String> {
            let mut out: CFTypeRef = std::ptr::null();
            // SAFETY: Copy rule -- on success we own `out`.
            let err = unsafe { AXUIElementCopyActionNames(self.0.as_CFTypeRef(), &mut out) };
            if err != 0 || out.is_null() {
                return Vec::new();
            }
            let value = unsafe { CFType::wrap_under_create_rule(out) };
            let Some(array) = value.downcast::<CFArray>() else {
                return Vec::new();
            };
            array
                .iter()
                .filter(|item| !item.is_null())
                .filter_map(|item| {
                    unsafe { CFType::wrap_under_get_rule(*item as CFTypeRef) }
                        .downcast::<CFString>()
                        .map(|s| s.to_string())
                })
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

    /// `RC-N2N-CHROME-BUTTON` precondition: does Chrome expose the page
    /// button (found by its DOM id inside the web area) with an `AXPress`
    /// action? The host picks the AxPressable route only then. Measured on
    /// the first live run (2026-10-23): right after the grant switched web
    /// accessibility on, the button listed only `AXShowMenu` and the host
    /// fell back to a real session-tap click. `Ok(false)` = found, not
    /// pressable yet; `Err` = not found.
    pub(crate) fn page_element_is_ax_pressable(pid: i32, dom_id: &str) -> Result<bool, String> {
        let app = AxElement::application(pid).ok_or("AXUIElementCreateApplication failed")?;
        let mut queue: std::collections::VecDeque<(AxElement, usize)> =
            std::collections::VecDeque::new();
        queue.push_back((app, 0));
        let mut visited = 0usize;
        while let Some((element, depth)) = queue.pop_front() {
            visited += 1;
            if visited > 6000 {
                break;
            }
            if element.string("AXDOMIdentifier").as_deref() == Some(dom_id) {
                return Ok(element.action_names().iter().any(|name| name == "AXPress"));
            }
            if depth < 40 {
                for child in element.children() {
                    queue.push_back((child, depth + 1));
                }
            }
        }
        Err(format!("no AX element with DOM id '{dom_id}' in Chrome's tree"))
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
            format!(
                "no native AXTextField (address bar) under Chrome pid {pid} ({visited} AX nodes)"
            )
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
            shape: super::PageShape,
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
            std::fs::write(&page_path, super::target_page_html(shape, &marker))
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
                        target.devtools_port =
                            super::parse_devtools_active_port(&text).unwrap_or(0);
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
            self.eval_json(super::page_state_expression()).await
        }

        /// Read where the click must land (window + input geometry).
        pub(crate) async fn click_geometry(
            &self,
            shape: super::PageShape,
        ) -> Result<super::ClickGeometry, String> {
            self.eval_json(super::click_geometry_expression(shape)).await
        }

        /// Reload the page (fresh field, fresh press counts) so Chrome
        /// rebuilds the web tree under the accessibility mode the grant
        /// switched on. Waits until the reloaded page reports ready.
        pub(crate) async fn reload_page(&self) -> Result<(), String> {
            let _: bool = self
                .eval_json(
                    "JSON.stringify((() => { setTimeout(() => location.reload(), 0); return true; })())"
                        .to_string(),
                )
                .await?;
            let deadline = Instant::now() + Duration::from_secs(10);
            tokio::time::sleep(Duration::from_millis(300)).await;
            loop {
                if let Ok(state) = self.page_state().await {
                    if state.ready == "complete" && state.value.as_deref() == Some("") {
                        return Ok(());
                    }
                }
                if Instant::now() >= deadline {
                    return Err("the page did not come back ready after a reload".to_string());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        async fn eval_json<T: serde::de::DeserializeOwned>(
            &self,
            expression: String,
        ) -> Result<T, String> {
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
                "params": { "expression": expression, "returnByValue": true },
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
                        break super::parse_json_eval_response(&value);
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
            required_ax_press: None,
        }
    }

    #[test]
    fn exact_value_with_a_clean_address_bar_passes() {
        let (d, s, e) = (driven(), granted(), effects());
        let verdict = evaluate(obs(
            &d,
            &s,
            &e,
            Ok(KEYSTONE_TEXT),
            Ok("file:///tmp/x/target.html"),
        ));
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
        for value in [
            "petalrcnativ",
            "xpetalrcnative",
            "petalrcnativepetalrcnative",
        ] {
            let verdict = evaluate(obs(&d, &s, &e, Ok(value), Ok("file:///t")));
            assert!(
                matches!(verdict, RcVerdict::TestFail(_)),
                "{value}: {verdict:?}"
            );
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
        for shape in [PageShape::BareInput, PageShape::FormField] {
            let steps = keystone_steps(shape, (120, 640));
            assert_eq!(
                steps,
                vec![
                    DriveStep::ClickAt {
                        x_permille: 120,
                        y_permille: 640
                    },
                    DriveStep::Type(shape.keystone().to_string()),
                    DriveStep::Release
                ]
            );
        }
    }

    #[test]
    fn each_shape_is_its_own_scenario_with_its_own_keystone_and_marker() {
        assert_eq!(
            PageShape::for_scenario("RC-N2N-CHROME"),
            Some(PageShape::BareInput)
        );
        assert_eq!(
            PageShape::for_scenario("RC-N2N-CHROME-FORM"),
            Some(PageShape::FormField)
        );
        assert_eq!(PageShape::for_scenario("RC-N2N"), None);
        let (a, b) = (PageShape::BareInput, PageShape::FormField);
        assert_ne!(a.keystone(), b.keystone());
        assert!(!a.keystone().contains(b.keystone()) && !b.keystone().contains(a.keystone()));
        assert!(b.keystone().chars().all(|c| c.is_ascii_lowercase()));
        let (ma, mb) = (marker_for_run(a, "1"), marker_for_run(b, "1"));
        assert!(!ma.contains(&mb) && !mb.contains(&ma), "{ma} / {mb}");
        for shape in [a, b] {
            let marker = marker_for_run(shape, "1785084128968");
            for other in [a, b] {
                assert!(!marker.contains(other.keystone()), "{marker}");
            }
        }
    }

    #[test]
    fn form_shape_wraps_the_input_in_a_pressable_labelled_container() {
        let html = target_page_html(PageShape::FormField, "petal-rc-n2n-chromeform-1");
        assert!(html.contains("<label class=\"field\">"), "{html}");
        assert!(html.contains("role=\"group\""), "{html}");
        assert!(html.contains("tabindex=\"-1\""), "{html}");
        assert!(html.contains("onclick="), "{html}");
        assert!(html.contains("placeholder=\"Jane Doe\""), "{html}");
        assert!(html.contains("padding: 12px 14px"), "{html}");
        assert_eq!(
            html.matches(&format!("id=\"{INPUT_ELEMENT_ID}\"")).count(),
            1
        );
        assert!(!html.contains("autofocus"), "{html}");
        assert!(html.contains("animation"), "{html}");
        for shape in [PageShape::BareInput, PageShape::FormField] {
            assert!(!html.contains(shape.keystone()));
        }
    }

    fn geometry(top: f64, height: f64) -> ClickGeometry {
        ClickGeometry {
            outer_width: 900.0,
            outer_height: 700.0,
            inner_width: 900.0,
            inner_height: 612.0,
            left: 46.0,
            top,
            width: 560.0,
            height,
            padding_left: 0.0,
        }
    }

    #[test]
    fn click_lands_in_the_inputs_text_area_in_window_coordinates() {
        // Input at viewport y 150..176 -> window y 88+163 = 251 of 700.
        let (x, y) = click_permille(PageShape::BareInput, &geometry(150.0, 26.0)).unwrap();
        assert_eq!(y, 359, "toolbar offset must be added: {y}");
        assert_eq!(x, 78, "a little right of the field's left edge: {x}");
        // The bare shape fills the viewport: the click sits in the content.
        let full = ClickGeometry {
            left: 0.0,
            top: 0.0,
            width: 900.0,
            height: 612.0,
            padding_left: 24.0,
            ..geometry(0.0, 0.0)
        };
        let (fx, fy) = click_permille(PageShape::BareInput, &full).unwrap();
        assert!(fy > (88.0 * 1000.0 / 700.0) as u32 && fy < 1000, "{fy}");
        assert_eq!(fx, 53);
    }

    #[test]
    fn click_geometry_that_cannot_place_a_point_is_refused() {
        assert_eq!(click_permille(PageShape::BareInput, &geometry(150.0, 0.0)), None);
        assert_eq!(
            click_permille(PageShape::BareInput, &geometry(900.0, 26.0)),
            None,
            "below the window"
        );
        assert_eq!(
            click_permille(PageShape::BareInput, &geometry(-200.0, 26.0)),
            None,
            "inside the toolbar"
        );
        let geometry_json = serde_json::json!({"id": 1, "result": {"result": {"type": "string",
            "value": "{\"outerWidth\":900,\"outerHeight\":700,\"innerWidth\":900,\"innerHeight\":612,\"left\":46,\"top\":150,\"width\":560,\"height\":26,\"paddingLeft\":0}"}}});
        let parsed: ClickGeometry = parse_json_eval_response(&geometry_json).unwrap();
        assert_eq!(parsed, geometry(150.0, 26.0));
        assert!(click_geometry_expression(PageShape::BareInput).contains(INPUT_ELEMENT_ID));
        assert!(click_geometry_expression(PageShape::FormField).contains(INPUT_ELEMENT_ID));
        assert!(click_geometry_expression(PageShape::ButtonFocus).contains(PRESS_ELEMENT_ID));
    }

    #[test]
    fn button_shape_clicks_the_buttons_centre_and_its_page_wires_the_focus() {
        let button = ClickGeometry {
            left: 32.0,
            top: 60.0,
            width: 576.0,
            height: 80.0,
            padding_left: 28.0,
            ..geometry(0.0, 0.0)
        };
        let (x, y) = click_permille(PageShape::ButtonFocus, &button).unwrap();
        assert_eq!(x, ((32.0 + 288.0) * 1000.0 / 900.0_f64).round() as u32, "button centre");
        assert_eq!(y, ((88.0 + 60.0 + 40.0) * 1000.0 / 700.0_f64).round() as u32);
        let html = target_page_html(PageShape::ButtonFocus, "petal-rc-n2n-chromebutton-1");
        assert!(html.contains(&format!("id=\"{PRESS_ELEMENT_ID}\"")));
        assert!(html.contains(&format!("id=\"{INPUT_ELEMENT_ID}\"")));
        assert!(html.contains("addEventListener('pointerdown'"));
        // Chrome's AXPress dispatches pointerdown/mousedown too; only a
        // real-shaped press (pressure, click count) may count as a pointer.
        assert!(html.contains("if (e.pressure > 0) window.__petalPress.pointerDowns++"));
        assert!(html.contains("if (e.detail > 0) window.__petalPress.mouseDowns++"));
        assert!(html.contains(&format!("document.getElementById('{INPUT_ELEMENT_ID}').focus()")));
        assert!(!html.contains("autofocus"));
        assert_eq!(PageShape::for_scenario("RC-N2N-CHROME-BUTTON"), Some(PageShape::ButtonFocus));
        assert!(PageShape::ButtonFocus.requires_ax_press());
        assert!(!PageShape::BareInput.requires_ax_press() && !PageShape::FormField.requires_ax_press());
    }

    #[test]
    fn button_shape_passes_only_on_an_ax_press_that_lands_the_text() {
        let driven = driven();
        let (statuses, effects) = (granted(), effects());
        let ax_press = PressRecord {
            clicks: 1,
            pointer_downs: 0,
            mouse_downs: 0,
        };
        let with = |press: Option<&PressRecord>, value: &'static str, omnibox: &'static str| {
            let mut o = obs(&driven, &statuses, &effects, Ok(value), Ok(omnibox));
            o.required_ax_press = Some(press);
            evaluate(o)
        };
        assert!(matches!(
            with(Some(&ax_press), KEYSTONE_TEXT, "file:///tmp/x/target.html"),
            RcVerdict::Pass(_)
        ));
        // A real pointer click is not AxPressable coverage, even if it typed.
        let real = PressRecord {
            pointer_downs: 1,
            mouse_downs: 1,
            ..ax_press.clone()
        };
        assert!(matches!(
            with(Some(&real), KEYSTONE_TEXT, "file:///tmp/x/target.html"),
            RcVerdict::InfraFail(reason) if reason.contains("AxPressable")
        ));
        // The click never reached the button.
        let missed = PressRecord::default();
        assert!(matches!(
            with(Some(&missed), "", "file:///tmp/x/target.html"),
            RcVerdict::TestFail(reason) if reason.contains("never reached the page button")
        ));
        assert!(matches!(
            with(None, KEYSTONE_TEXT, "file:///tmp/x/target.html"),
            RcVerdict::InfraFail(_)
        ));
        // The incident: pressed, keys went to the address bar.
        assert!(matches!(
            with(Some(&ax_press), "", "petalrcnativefile:///tmp/x/target.html"),
            RcVerdict::TestFail(reason) if reason.contains("ADDRESS BAR")
        ));
    }

    #[test]
    fn page_has_one_unfocused_input_and_escapes_the_marker() {
        let html = target_page_html(PageShape::BareInput, "petal-rc-n2n-chrome-1<x>");
        assert!(
            html.contains(&format!("id=\"{INPUT_ELEMENT_ID}\"")),
            "{html}"
        );
        assert!(
            html.contains("<title>petal-rc-n2n-chrome-1&lt;x&gt;</title>"),
            "{html}"
        );
        assert!(
            !html.contains("autofocus"),
            "nothing but the click may focus it: {html}"
        );
        assert!(
            html.contains("100vh"),
            "the field must cover the click point: {html}"
        );
        assert!(
            html.contains("animation"),
            "the window must keep drawing for SCK: {html}"
        );
        assert!(
            !html.contains(KEYSTONE_TEXT),
            "the page must not seed the keystone"
        );
    }

    #[test]
    fn marker_is_per_run_and_does_not_contain_the_keystone() {
        let marker = marker_for_run(PageShape::BareInput, "1785084128968");
        assert_eq!(marker, "petal-rc-n2n-chrome-1785084128968");
        assert!(!marker.contains(KEYSTONE_TEXT));
    }

    #[test]
    fn chrome_args_are_isolated_headed_and_on_an_ephemeral_devtools_port() {
        let args = chrome_args("/tmp/p");
        assert!(args.contains(&"--user-data-dir=/tmp/p".to_string()));
        assert!(args.contains(&"--remote-debugging-port=0".to_string()));
        assert!(
            !args.iter().any(|a| a.starts_with("--headless")),
            "{args:?}"
        );
        assert!(!args.iter().any(|a| a.contains("9222")), "{args:?}");
    }

    #[test]
    fn devtools_active_port_parses_the_first_line() {
        assert_eq!(
            parse_devtools_active_port("58258\n/devtools/browser/abc\n"),
            Some(58258)
        );
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
