//! PETAL-WORKAROUND(tao-drag-window): window drags from `data-tauri-drag-region`.
//!
//! Dragging a window by its drag region could make it jump so its top-left
//! corner landed under the pointer. On macOS 26 this happened when dragging Petal
//! View by its title bar soon after it opened or reopened. Tauri's
//! `start_dragging` arrives over IPC after the mouse-down, and tao's macOS
//! `drag_window` hands `NSApp.currentEvent` to `performWindowDragWithEvent:`.
//! By then the current event can be a stale AppKitDefined event at the
//! window's corner (type 13 at (0, 400) in our test logs), so the drag started
//! from there. `src/lib/windowDrag.ts` sends macOS drag-region presses to
//! [`start_window_drag`] instead, which drags from a real mouse-down.
//!
//! Remove when: tao's macOS `drag_window` stops trusting `NSApp.currentEvent` (tauri-apps/tauri#6568).
//! Removal: delete every `PETAL-WORKAROUND(tao-drag-window)` site, keep `acceptFirstMouse`, and check that dragging Petal View right after it opens no longer jumps.

/// `NSEventType` raw values the decision needs (AppKit `NSEvent.h`).
const LEFT_MOUSE_DOWN: usize = 1;
const LEFT_MOUSE_DRAGGED: usize = 6;

/// The parts of `NSApp.currentEvent` the decision reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurrentEvent {
    kind: usize,
    window_number: isize,
}

/// What to hand `performWindowDragWithEvent:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragStart {
    /// The left button is already up: the press was a click, so don't drag.
    Skip,
    /// The current event is this window's own left mouse-down or drag.
    CurrentEvent,
    /// Anything else (an activation event, another window's event, none):
    /// drag from a left mouse-down synthesized at the pointer.
    SynthesizedMouseDown,
}

/// PETAL-WORKAROUND(tao-drag-window): decide how to start the drag.
///
/// This keeps the real mouse-down when AppKit still has it, rather than always
/// synthesizing one at the pointer, so a pointer that moved between the press
/// and the IPC call keeps its original grab point.
fn drag_start(left_button_down: bool, current: Option<CurrentEvent>, this_window: isize) -> DragStart {
    if !left_button_down {
        return DragStart::Skip;
    }
    match current {
        Some(CurrentEvent { kind: LEFT_MOUSE_DOWN | LEFT_MOUSE_DRAGGED, window_number })
            if window_number == this_window =>
        {
            DragStart::CurrentEvent
        }
        _ => DragStart::SynthesizedMouseDown,
    }
}

/// What `performWindowDragWithEvent:` is handed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DragEvent {
    /// `NSApp.currentEvent` as it is.
    Current,
    /// A left mouse-down built at `location` (window coordinates) in window
    /// `window_number`.
    MouseDownAt { location: (f64, f64), window_number: isize },
}

/// PETAL-WORKAROUND(tao-drag-window): the AppKit calls [`start_drag`] makes,
/// so its sequence runs against a fake in tests.
trait DragHost {
    /// Whether the left mouse button is down right now.
    fn left_button_down(&self) -> bool;
    /// This window's `windowNumber`.
    fn window_number(&self) -> isize;
    /// `NSApp.currentEvent`, if there is one.
    fn current_event(&self) -> Option<CurrentEvent>;
    /// The pointer in this window's coordinates.
    fn pointer_in_window(&self) -> (f64, f64);
    /// Call `performWindowDragWithEvent:` on this window.
    fn perform_drag(&self, event: DragEvent) -> Result<(), String>;
}

/// PETAL-WORKAROUND(tao-drag-window): read the button and the current event,
/// then drag from the current event or from a mouse-down at the pointer.
/// Returns what it decided.
fn start_drag(host: &impl DragHost) -> Result<DragStart, String> {
    let this_window = host.window_number();
    let start = drag_start(host.left_button_down(), host.current_event(), this_window);
    match start {
        DragStart::Skip => {}
        DragStart::CurrentEvent => host.perform_drag(DragEvent::Current)?,
        DragStart::SynthesizedMouseDown => host.perform_drag(DragEvent::MouseDownAt {
            location: host.pointer_in_window(),
            window_number: this_window,
        })?,
    }
    Ok(start)
}

/// PETAL-WORKAROUND(tao-drag-window): start a window drag for the calling
/// webview's window. macOS drags from a real left mouse-down (see the module
/// doc); every other OS keeps Tauri's `start_dragging`.
#[tauri::command]
pub fn start_window_drag(window: tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let target = window.clone();
        window
            .run_on_main_thread(move || {
                if let Err(e) = drag_window_on_main(&target) {
                    log::warn!("window_drag: drag for {} failed: {e}", target.label());
                }
            })
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        drag_window_on_main(&window)
    }
}

/// PETAL-WORKAROUND(tao-drag-window): the drag start for callers already on
/// the main thread, such as the remote-window header (`compositor_start_drag`,
/// #585). macOS must be on the main thread; elsewhere this is `start_dragging`.
pub(crate) fn drag_window_on_main(window: &tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::drag_from_mouse_down(window)
    }
    #[cfg(not(target_os = "macos"))]
    {
        window.start_dragging().map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    // PETAL-WORKAROUND(tao-drag-window): the AppKit side of the drag.
    use super::{start_drag, CurrentEvent, DragEvent, DragHost, LEFT_MOUSE_DOWN, LEFT_MOUSE_DRAGGED};
    use objc2::rc::Retained;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSEvent, NSEventType, NSWindow};
    use objc2_foundation::{NSPoint, NSProcessInfo};

    const _: () = assert!(NSEventType::LeftMouseDown.0 == LEFT_MOUSE_DOWN);
    const _: () = assert!(NSEventType::LeftMouseDragged.0 == LEFT_MOUSE_DRAGGED);

    struct AppKit<'a> {
        window: &'a NSWindow,
        current: Option<Retained<NSEvent>>,
    }

    impl DragHost for AppKit<'_> {
        fn left_button_down(&self) -> bool {
            NSEvent::pressedMouseButtons() & 1 != 0
        }

        fn window_number(&self) -> isize {
            self.window.windowNumber()
        }

        fn current_event(&self) -> Option<CurrentEvent> {
            self.current.as_ref().map(|event| CurrentEvent {
                kind: event.r#type().0,
                window_number: event.windowNumber(),
            })
        }

        fn pointer_in_window(&self) -> (f64, f64) {
            let p = self.window.mouseLocationOutsideOfEventStream();
            (p.x, p.y)
        }

        fn perform_drag(&self, event: DragEvent) -> Result<(), String> {
            let event = match event {
                DragEvent::Current => self.current.clone().ok_or("current event vanished")?,
                DragEvent::MouseDownAt { location: (x, y), window_number } => {
                    NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                        NSEventType::LeftMouseDown,
                        NSPoint::new(x, y),
                        NSEvent::modifierFlags_class(),
                        NSProcessInfo::processInfo().systemUptime(),
                        window_number,
                        None,
                        0,
                        1,
                        1.0,
                    )
                    .ok_or("could not create a mouse-down event")?
                }
            };
            self.window.performWindowDragWithEvent(&event);
            Ok(())
        }
    }

    /// Must run on the main thread.
    pub(super) fn drag_from_mouse_down(window: &tauri::WebviewWindow) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
        let ns_window_ptr = window
            .ns_window()
            .map_err(|e| format!("ns_window unavailable: {e}"))?;
        if ns_window_ptr.is_null() {
            return Err("ns_window is null".into());
        }
        // SAFETY: `ns_window_ptr` is the live AppKit `NSWindow*` Tauri exposes
        // for this `WebviewWindow`, checked non-null, and only borrowed for
        // this synchronous main-thread call.
        let ns_window: &NSWindow = unsafe { &*(ns_window_ptr as *const NSWindow) };
        let host = AppKit {
            window: ns_window,
            current: NSApplication::sharedApplication(mtm).currentEvent(),
        };
        start_drag(&host).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PETAL-WORKAROUND(tao-drag-window): the click-or-drag decision.

    const THIS: isize = 4242;
    /// `NSEventTypeAppKitDefined`: what tauri-apps/tauri#6568 measured as the
    /// current event when `start_dragging` arrived.
    const APPKIT_DEFINED: usize = 13;

    fn event(kind: usize, window_number: isize) -> Option<CurrentEvent> {
        Some(CurrentEvent { kind, window_number })
    }

    #[test]
    fn a_released_button_is_a_click_and_never_drags() {
        for current in [None, event(LEFT_MOUSE_DOWN, THIS), event(APPKIT_DEFINED, 0)] {
            assert_eq!(drag_start(false, current, THIS), DragStart::Skip);
        }
    }

    #[test]
    fn this_windows_own_mouse_down_or_drag_is_used_as_is() {
        assert_eq!(drag_start(true, event(LEFT_MOUSE_DOWN, THIS), THIS), DragStart::CurrentEvent);
        assert_eq!(drag_start(true, event(LEFT_MOUSE_DRAGGED, THIS), THIS), DragStart::CurrentEvent);
    }

    #[test]
    fn an_activation_event_is_replaced_by_a_mouse_down_at_the_pointer() {
        assert_eq!(
            drag_start(true, event(APPKIT_DEFINED, THIS), THIS),
            DragStart::SynthesizedMouseDown
        );
    }

    #[test]
    fn another_windows_mouse_down_or_no_event_is_replaced_too() {
        assert_eq!(
            drag_start(true, event(LEFT_MOUSE_DOWN, THIS + 1), THIS),
            DragStart::SynthesizedMouseDown
        );
        assert_eq!(drag_start(true, None, THIS), DragStart::SynthesizedMouseDown);
        // 0x15, the one type tao itself rebuilds: also from the pointer here.
        assert_eq!(drag_start(true, event(0x15, THIS), THIS), DragStart::SynthesizedMouseDown);
    }

    // PETAL-WORKAROUND(tao-drag-window): the command's AppKit sequence, against
    // a fake of the calls it makes.

    struct FakeHost {
        button_down: bool,
        current: Option<CurrentEvent>,
        pointer: (f64, f64),
        drag_result: Result<(), String>,
        performed: std::cell::RefCell<Vec<DragEvent>>,
    }

    impl FakeHost {
        fn new(button_down: bool, current: Option<CurrentEvent>) -> Self {
            Self {
                button_down,
                current,
                pointer: (270.0, 366.0),
                drag_result: Ok(()),
                performed: Default::default(),
            }
        }
    }

    impl DragHost for FakeHost {
        fn left_button_down(&self) -> bool {
            self.button_down
        }
        fn window_number(&self) -> isize {
            THIS
        }
        fn current_event(&self) -> Option<CurrentEvent> {
            self.current
        }
        fn pointer_in_window(&self) -> (f64, f64) {
            self.pointer
        }
        fn perform_drag(&self, event: DragEvent) -> Result<(), String> {
            self.performed.borrow_mut().push(event);
            self.drag_result.clone()
        }
    }

    #[test]
    fn a_stale_activation_event_drags_from_a_mouse_down_at_the_pointer_in_this_window() {
        let host = FakeHost::new(true, event(APPKIT_DEFINED, THIS));
        assert_eq!(start_drag(&host), Ok(DragStart::SynthesizedMouseDown));
        assert_eq!(
            *host.performed.borrow(),
            [DragEvent::MouseDownAt { location: (270.0, 366.0), window_number: THIS }]
        );
    }

    #[test]
    fn this_windows_mouse_down_is_handed_to_appkit_unchanged() {
        let host = FakeHost::new(true, event(LEFT_MOUSE_DOWN, THIS));
        assert_eq!(start_drag(&host), Ok(DragStart::CurrentEvent));
        assert_eq!(*host.performed.borrow(), [DragEvent::Current]);
    }

    #[test]
    fn a_released_button_performs_no_drag() {
        let host = FakeHost::new(false, event(APPKIT_DEFINED, THIS));
        assert_eq!(start_drag(&host), Ok(DragStart::Skip));
        assert!(host.performed.borrow().is_empty());
    }

    #[test]
    fn a_failed_drag_is_returned_to_the_caller() {
        let mut host = FakeHost::new(true, None);
        host.drag_result = Err("could not create a mouse-down event".into());
        assert_eq!(start_drag(&host), Err("could not create a mouse-down event".into()));
    }

    // PETAL-WORKAROUND(tao-drag-window): drift test against the `drag.js` of the
    // Tauri crate this build resolves. It fails only when that file changes what
    // `src/lib/windowDrag.ts` depends on, not on every Tauri bump.

    const QUOTES: [char; 3] = ['\'', '"', '`'];

    /// Reduce JavaScript to its tokens so formatting never fails the drift
    /// test: comments, semicolons, trailing commas and whitespace go (one space
    /// stays between two words), and `"..."` strings become `'...'`.
    fn normalize_js(src: &str) -> String {
        let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        let chars: Vec<char> = src.chars().collect();
        let mut out = String::new();
        let mut pending_space = false;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if QUOTES.contains(&c) {
                let mut body = String::new();
                i += 1;
                while i < chars.len() && chars[i] != c {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        body.push(chars[i]);
                        i += 1;
                    }
                    body.push(chars[i]);
                    i += 1;
                }
                i += 1;
                let quote = if c == '`' { '`' } else { '\'' };
                out.push(quote);
                out.push_str(&body);
                out.push(quote);
                pending_space = false;
            } else if c == '/' && chars.get(i + 1) == Some(&'/') {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                pending_space = true;
            } else if c == '/' && chars.get(i + 1) == Some(&'*') {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
                pending_space = true;
            } else if c.is_whitespace() || c == ';' {
                pending_space = true;
                i += 1;
            } else {
                if pending_space && out.chars().last().is_some_and(is_word) && is_word(c) {
                    out.push(' ');
                }
                if matches!(c, ']' | ')' | '}') && out.ends_with(',') {
                    out.pop();
                }
                out.push(c);
                pending_space = false;
                i += 1;
            }
        }
        out
    }

    /// Byte range of the balanced `open`..`close` group that starts at the
    /// first `open` at or after `from`, skipping string literals.
    fn balanced(src: &str, from: usize, open: char, close: char) -> Option<std::ops::Range<usize>> {
        let start = from + src[from..].find(open)?;
        let mut depth = 0usize;
        let mut quote: Option<char> = None;
        let mut escaped = false;
        for (offset, c) in src[start..].char_indices() {
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
                continue;
            }
            if QUOTES.contains(&c) {
                quote = Some(c);
            } else if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(start..start + offset + c.len_utf8());
                }
            }
        }
        None
    }

    /// Split a call's argument text at top-level commas.
    fn top_level_args(inner: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut depth = 0i32;
        let mut current = String::new();
        let mut quote: Option<char> = None;
        for c in inner.chars() {
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                current.push(c);
                continue;
            }
            match c {
                c if QUOTES.contains(&c) => quote = Some(c),
                '(' | '{' | '[' => depth += 1,
                ')' | '}' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    args.push(current.trim().to_string());
                    current.clear();
                    continue;
                }
                _ => {}
            }
            current.push(c);
        }
        if !current.trim().is_empty() {
            args.push(current.trim().to_string());
        }
        args
    }

    /// The normalized `drag.js` of the `tauri` package `cargo metadata`
    /// resolves for this host. Offline and filtered to the host platform, so it
    /// needs only the crates this build already downloaded.
    fn resolved_tauri_drag_js() -> (String, std::path::PathBuf) {
        // Three tests read it; run `cargo metadata` once per test binary.
        static RESOLVED: std::sync::OnceLock<(String, std::path::PathBuf)> = std::sync::OnceLock::new();
        RESOLVED.get_or_init(resolve_tauri_drag_js).clone()
    }

    fn resolve_tauri_drag_js() -> (String, std::path::PathBuf) {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let version = std::process::Command::new(rustc).arg("-vV").output().expect("run rustc -vV");
        let version = String::from_utf8_lossy(&version.stdout).into_owned();
        let host = version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .expect("rustc -vV prints the host triple")
            .trim()
            .to_string();
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = std::process::Command::new(cargo)
            .args(["metadata", "--format-version", "1", "--offline", "--filter-platform", &host])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("run cargo metadata");
        assert!(
            output.status.success(),
            "cargo metadata --offline --filter-platform {host} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("cargo metadata JSON");
        let manifests: Vec<&str> = metadata["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .filter(|p| p["name"] == "tauri")
            .filter_map(|p| p["manifest_path"].as_str())
            .collect();
        assert_eq!(manifests.len(), 1, "expected one resolved `tauri` package, got {manifests:?}");
        let path = std::path::Path::new(manifests[0])
            .parent()
            .expect("tauri crate dir")
            .join("src/window/scripts/drag.js");
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        (normalize_js(&src), path)
    }

    const REVERIFY: &str = "Re-verify PETAL-WORKAROUND(tao-drag-window): src/lib/windowDrag.ts must \
        still run before Tauri's drag handler and mirror its isDragRegion. First check whether \
        tao's macOS drag_window is fixed (tauri-apps/tauri#6568); if it is, delete the workaround. \
        Otherwise update windowDrag.ts and the pinned text here together, then check that dragging Petal View right after it opens no longer jumps.";

    /// Why a normalized `drag.js` would run a mousedown listener before ours
    /// on `document.documentElement` (bubble phase), or `None` if none would.
    fn mousedown_listener_problem(js: &str) -> Option<String> {
        let calls: Vec<usize> = js.match_indices(".addEventListener('mousedown'").map(|(at, _)| at).collect();
        if calls.is_empty() {
            return Some("no mousedown listener found".into());
        }
        for at in calls {
            let receiver = js[..at]
                .rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '.'))
                .next()
                .unwrap_or("");
            // A `document` or `window` bubble listener runs after ours.
            if receiver != "document" && receiver != "window" {
                return Some(format!("a mousedown listener is on `{receiver}`"));
            }
            let Some(call) = balanced(js, at, '(', ')') else {
                return Some("a mousedown listener call could not be parsed".into());
            };
            let args = top_level_args(&js[call.start + 1..call.end - 1]);
            match args.get(2).map(String::as_str) {
                None | Some("false") | Some("{capture:false}") => {}
                Some(options) => return Some(format!("a mousedown listener takes options `{options}`")),
            }
        }
        None
    }

    #[test]
    fn tauri_drag_handler_still_listens_on_document_in_the_bubble_phase() {
        let (js, path) = resolved_tauri_drag_js();
        if let Some(problem) = mousedown_listener_problem(&js) {
            panic!("{}: {problem}. {REVERIFY}", path.display());
        }
    }

    #[test]
    fn the_listener_check_flags_capture_and_other_targets_only() {
        let check = |src: &str| mousedown_listener_problem(&normalize_js(src));
        assert_eq!(check("document.addEventListener('mousedown', (e) => { f(e) })"), None);
        assert_eq!(check("document.addEventListener(\"mousedown\", (e) => { f(e); }, false);"), None);
        assert_eq!(check("window.addEventListener('mousedown', f, { capture: false })"), None);
        assert!(check("document.addEventListener('mousedown', f, true)").is_some());
        assert!(check("document.addEventListener('mousedown', f, { capture: true })").is_some());
        assert!(check("document.documentElement.addEventListener('mousedown', f)").is_some());
        assert!(check("document.addEventListener('pointerdown', f)").is_some());
    }

    /// Tauri 2.12.2's drag-region semantics, as `normalize_js` prints them.
    const PINNED_DRAG_REGION_SEMANTICS: [(&str, &str); 5] = [
        ("const TAURI_DRAG_REGION_ATTR", "const TAURI_DRAG_REGION_ATTR='data-tauri-drag-region'"),
        (
            "const CLICKABLE_TAGS",
            "const CLICKABLE_TAGS=new Set(['A','BUTTON','INPUT','SELECT','TEXTAREA','LABEL','SUMMARY'])",
        ),
        (
            "const INTERACTIVE_ROLES",
            "const INTERACTIVE_ROLES=new Set(['button','link','menuitem','tab','checkbox','radio','switch','option'])",
        ),
        (
            "function isClickableElement",
            "function isClickableElement(el){return(CLICKABLE_TAGS.has(el.tagName)||(el.hasAttribute('contenteditable')&&el.getAttribute('contenteditable')!=='false')||(el.hasAttribute('tabindex')&&el.getAttribute('tabindex')!=='-1')||INTERACTIVE_ROLES.has(el.getAttribute('role')))}",
        ),
        (
            "function isDragRegion",
            "function isDragRegion(composedPath){for(const el of composedPath){if(!(el instanceof HTMLElement))continue const attr=el.getAttribute(TAURI_DRAG_REGION_ATTR)if(isClickableElement(el)&&attr===null)return false if(attr===null)continue if(attr==='false')return false if(attr==='deep')return true if(attr===''||attr==='true')return el===composedPath[0]}return false}",
        ),
    ];

    /// The declaration starting at `head`: a function through its body, a
    /// `const` through its balanced initializer.
    fn declaration<'a>(js: &'a str, head: &str) -> Option<&'a str> {
        let at = js.find(head)?;
        if head.starts_with("function") {
            return Some(&js[at..balanced(js, at, '{', '}')?.end]);
        }
        let rest = &js[at..];
        let eq = rest.find('=')?;
        let value = &rest[eq + 1..];
        if value.starts_with('\'') {
            let close = value[1..].find('\'')? + 2;
            return Some(&rest[..eq + 1 + close]);
        }
        Some(&js[at..balanced(js, at + eq, '(', ')')?.end])
    }

    #[test]
    fn tauri_drag_region_semantics_match_the_copy_in_window_drag_ts() {
        let (js, path) = resolved_tauri_drag_js();
        for (head, pinned) in PINNED_DRAG_REGION_SEMANTICS {
            assert_eq!(declaration(&js, head), Some(pinned), "{}: `{head}` changed. {REVERIFY}", path.display());
        }
    }

    /// `tests/windowDragRendered.test.ts` drives real presses through this copy.
    #[test]
    fn rendered_test_fixture_is_the_resolved_tauri_drag_js() {
        let (js, path) = resolved_tauri_drag_js();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/tauri-drag.js");
        let src = std::fs::read_to_string(&fixture).unwrap_or_else(|e| panic!("read {}: {e}", fixture.display()));
        assert!(
            normalize_js(&src) == js,
            "{} differs from {}: copy it over, rerun tests/windowDragRendered.test.ts. {REVERIFY}",
            fixture.display(),
            path.display()
        );
    }

    #[test]
    fn normalize_js_ignores_formatting_but_keeps_meaning() {
        assert_eq!(normalize_js("a  // x\n /* y */ b(\"//not\", '/*no*/');"), "a b('//not','/*no*/')");
        assert_eq!(
            normalize_js("new Set([\n  'A',\n  'B',\n])"),
            normalize_js("new Set(['A', 'B'])"),
        );
        assert_ne!(normalize_js("new Set(['A', 'B'])"), normalize_js("new Set(['A'])"));
        assert_eq!(normalize_js("return false\nif (x) continue"), "return false if(x)continue");
    }
}
