//! Central share-target policy shared by picker enumeration and hover hit-tests.
//!
//! Platform modules only collect native facts. They do not decide whether a
//! window is a user-selectable share target. Keeping the decision here makes a
//! fast point hit-test and a slower picker enumeration agree by construction.

const DENYLISTED_BUNDLE_IDS: &[&str] = &["com.apple.controlcenter", "com.apple.WindowManager"];
pub(crate) const MIN_WINDOW_SIDE: i32 = 40;

/// The source kind accepted by the central policy. Registered Petal View
/// regions are intentionally distinct from ordinary application windows: the
/// picker may expose the region's capture source, while the hollow selector
/// itself must block hover-through targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ShareTargetKind {
    Window,
    RegisteredRegion,
}

/// Stable, testable reason a native surface was not accepted. Reasons are
/// deliberately structural; callers should not log them on every 16ms tick.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ShareTargetRejection {
    InvalidHandle,
    UnknownOwner,
    Hidden,
    Minimized,
    ToolWindow,
    OwnedOrTransient,
    Cloaked,
    TooSmall,
    OwnPetalWindow,
    PetalChrome,
    NonNormalLayer,
    DenylistedBundle,
    SystemSurface,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) enum ShareTargetDecision {
    Eligible(ShareTargetKind),
    Rejected(ShareTargetRejection),
}

impl ShareTargetDecision {
    pub(crate) const fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible(_))
    }

    pub(crate) const fn kind(&self) -> Option<ShareTargetKind> {
        match self {
            Self::Eligible(kind) => Some(*kind),
            Self::Rejected(_) => None,
        }
    }

    pub(crate) const fn rejection(&self) -> Option<&ShareTargetRejection> {
        match self {
            Self::Eligible(_) => None,
            Self::Rejected(reason) => Some(reason),
        }
    }
}

/// Native facts projected into the platform-neutral policy. `frame` is kept
/// outside this structure because geometry is needed by both accepted and
/// rejected surfaces for diagnostics, while policy only needs its dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShareTargetFacts {
    pub(crate) owner_pid: u32,
    pub(crate) self_pid: u32,
    pub(crate) visible: bool,
    pub(crate) minimized: bool,
    pub(crate) tool_window: bool,
    /// `WS_EX_APPWINDOW` is retained as an observed fact, not an admission
    /// gate: legitimate custom-chrome applications may use either style.
    pub(crate) app_window: bool,
    pub(crate) cloaked: bool,
    pub(crate) owner_present: bool,
    pub(crate) root_owner_differs: bool,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) layer: i32,
    pub(crate) region_selector: bool,
    pub(crate) petal_chrome: bool,
    pub(crate) system_surface: bool,
    pub(crate) bundle_id: Option<String>,
    pub(crate) class_name: Option<String>,
    pub(crate) process_name: Option<String>,
}

impl ShareTargetFacts {
    #[cfg(test)]
    fn ordinary(owner_pid: u32, self_pid: u32) -> Self {
        Self {
            owner_pid,
            self_pid,
            visible: true,
            minimized: false,
            tool_window: false,
            app_window: true,
            cloaked: false,
            owner_present: false,
            root_owner_differs: false,
            width: 800,
            height: 600,
            layer: 0,
            region_selector: false,
            petal_chrome: false,
            system_surface: false,
            bundle_id: None,
            class_name: Some("ApplicationFrameWindow".to_string()),
            process_name: Some("example.exe".to_string()),
        }
    }
}

/// Apply the one policy used by both native source enumeration and point
/// targeting. The order is intentional: fail closed on incomplete identity or
/// visibility facts before considering cosmetic metadata.
pub(crate) fn classify(facts: &ShareTargetFacts) -> ShareTargetDecision {
    if facts.region_selector && facts.owner_pid == facts.self_pid && facts.owner_pid != 0 {
        return ShareTargetDecision::Eligible(ShareTargetKind::RegisteredRegion);
    }
    if facts.owner_pid == 0 {
        return ShareTargetDecision::Rejected(ShareTargetRejection::UnknownOwner);
    }
    if !facts.visible {
        return ShareTargetDecision::Rejected(ShareTargetRejection::Hidden);
    }
    if facts.minimized {
        return ShareTargetDecision::Rejected(ShareTargetRejection::Minimized);
    }
    if facts.cloaked {
        return ShareTargetDecision::Rejected(ShareTargetRejection::Cloaked);
    }
    if facts.width < MIN_WINDOW_SIDE || facts.height < MIN_WINDOW_SIDE {
        return ShareTargetDecision::Rejected(ShareTargetRejection::TooSmall);
    }
    if facts.owner_pid == facts.self_pid {
        return ShareTargetDecision::Rejected(if facts.petal_chrome {
            ShareTargetRejection::PetalChrome
        } else {
            ShareTargetRejection::OwnPetalWindow
        });
    }
    if facts.layer != 0 {
        return ShareTargetDecision::Rejected(ShareTargetRejection::NonNormalLayer);
    }
    if facts.system_surface || is_known_system_surface(facts) {
        return ShareTargetDecision::Rejected(ShareTargetRejection::SystemSurface);
    }
    if facts.tool_window {
        return ShareTargetDecision::Rejected(ShareTargetRejection::ToolWindow);
    }
    if facts.owner_present || facts.root_owner_differs {
        return ShareTargetDecision::Rejected(ShareTargetRejection::OwnedOrTransient);
    }
    if facts
        .bundle_id
        .as_deref()
        .is_some_and(is_denylisted_bundle_id)
    {
        return ShareTargetDecision::Rejected(ShareTargetRejection::DenylistedBundle);
    }
    ShareTargetDecision::Eligible(ShareTargetKind::Window)
}

/// Narrow, evidence-backed Windows shell identities. Process names are part of
/// the predicate where a generic class is also used by legitimate applications
/// (for example, `Windows.UI.Core.CoreWindow`). File Explorer's
/// `CabinetWClass`/`ExploreWClass` is intentionally absent. Windows 11 Quick
/// Settings uses the `ShellHost`/`ControlCenterWindow` pair rather than the
/// older `ShellExperienceHost` CoreWindow pair.
fn is_known_system_surface(facts: &ShareTargetFacts) -> bool {
    let class = facts
        .class_name
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let process = facts
        .process_name
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    matches!(
        class.as_str(),
        "#32768"
            | "tooltips_class32"
            | "shell_traywnd"
            | "shell_secondarytraywnd"
            | "notifyiconoverflowwindow"
            | "toplevelwindowforoverflowxamlisland"
            | "xamlexplorerhostislandwindow"
    ) || (class == "windows.ui.core.corewindow"
        && matches!(
            process.as_str(),
            "shellexperiencehost.exe"
                | "startmenuexperiencehost.exe"
                | "searchhost.exe"
                | "textinputhost.exe"
        ))
        || (class == "controlcenterwindow" && process == "shellhost.exe")
}

pub(crate) fn is_denylisted_bundle_id(bundle_id: &str) -> bool {
    DENYLISTED_BUNDLE_IDS
        .iter()
        .any(|denylisted| bundle_id.eq_ignore_ascii_case(denylisted))
}

/// Collect and classify a live Windows HWND through the platform adapter.
#[cfg(target_os = "windows")]
pub(crate) fn classify_windows_window(
    hwnd: windows::Win32::Foundation::HWND,
    self_pid: u32,
) -> ShareTargetDecision {
    let Some(inspection) = crate::platform::windows::inspect_window(hwnd, self_pid) else {
        return ShareTargetDecision::Rejected(ShareTargetRejection::InvalidHandle);
    };
    classify(&inspection.facts)
}

/// Build the policy facts for a macOS hover-registry record. Nonzero-layer
/// records are still rejected by policy, but the hover caller may treat that
/// particular reason as a transparent system layer to preserve Dock behavior.
pub(crate) fn mac_window_facts(
    layer: i64,
    width: f64,
    height: f64,
    owner_pid: i64,
    self_pid: i64,
    bundle_id: Option<&str>,
    region_selector: bool,
    petal_chrome: bool,
) -> ShareTargetFacts {
    // Registry-confirmed Petal View regions stay authoritative even when a
    // point-in-time window-stack owner PID is stale or unavailable.
    let owner_pid = if region_selector {
        u32::try_from(self_pid).unwrap_or_default()
    } else {
        u32::try_from(owner_pid).unwrap_or_default()
    };
    ShareTargetFacts {
        owner_pid,
        self_pid: u32::try_from(self_pid).unwrap_or_default(),
        visible: true,
        minimized: false,
        tool_window: false,
        app_window: false,
        cloaked: false,
        owner_present: false,
        root_owner_differs: false,
        width: width.round().max(0.0) as i32,
        height: height.round().max(0.0) as i32,
        layer: layer.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        region_selector,
        petal_chrome,
        system_surface: false,
        bundle_id: bundle_id.map(str::to_owned),
        class_name: None,
        process_name: None,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn bundle_id_for_pid(pid: i32) -> Option<String> {
    use objc2_app_kit::NSRunningApplication;

    if pid <= 0 {
        return None;
    }

    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    app.bundleIdentifier()
        .map(|bundle_id| bundle_id.to_string())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn bundle_id_for_pid(_pid: i32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(facts: ShareTargetFacts) -> ShareTargetDecision {
        classify(&facts)
    }

    #[test]
    fn normal_and_frameless_windows_are_eligible_without_title_heuristics() {
        let mut normal = ShareTargetFacts::ordinary(99, 42);
        assert_eq!(
            decision(normal.clone()),
            ShareTargetDecision::Eligible(ShareTargetKind::Window)
        );
        normal.class_name = Some("CustomChromeWindow".to_string());
        normal.process_name = Some("frameless-app.exe".to_string());
        assert_eq!(
            decision(normal),
            ShareTargetDecision::Eligible(ShareTargetKind::Window)
        );
    }

    #[test]
    fn file_explorer_remains_eligible() {
        let mut facts = ShareTargetFacts::ordinary(99, 42);
        facts.class_name = Some("CabinetWClass".to_string());
        facts.process_name = Some("explorer.exe".to_string());
        assert_eq!(
            decision(facts),
            ShareTargetDecision::Eligible(ShareTargetKind::Window)
        );
    }

    #[test]
    fn eligible_elevated_windows_are_not_rejected_by_process_identity() {
        let mut facts = ShareTargetFacts::ordinary(99, 42);
        facts.process_name = Some("elevated-tool.exe".to_string());
        assert_eq!(
            decision(facts),
            ShareTargetDecision::Eligible(ShareTargetKind::Window)
        );
    }

    #[test]
    fn structural_rejections_have_stable_typed_reasons() {
        let cases: &[(&str, fn(&mut ShareTargetFacts), ShareTargetRejection)] = &[
            (
                "unknown owner",
                |f: &mut ShareTargetFacts| f.owner_pid = 0,
                ShareTargetRejection::UnknownOwner,
            ),
            (
                "hidden",
                |f: &mut ShareTargetFacts| f.visible = false,
                ShareTargetRejection::Hidden,
            ),
            (
                "minimized",
                |f: &mut ShareTargetFacts| f.minimized = true,
                ShareTargetRejection::Minimized,
            ),
            (
                "tool",
                |f: &mut ShareTargetFacts| f.tool_window = true,
                ShareTargetRejection::ToolWindow,
            ),
            (
                "owned",
                |f: &mut ShareTargetFacts| f.owner_present = true,
                ShareTargetRejection::OwnedOrTransient,
            ),
            (
                "root owner",
                |f: &mut ShareTargetFacts| f.root_owner_differs = true,
                ShareTargetRejection::OwnedOrTransient,
            ),
            (
                "cloaked",
                |f: &mut ShareTargetFacts| f.cloaked = true,
                ShareTargetRejection::Cloaked,
            ),
            (
                "undersized",
                |f: &mut ShareTargetFacts| f.width = MIN_WINDOW_SIDE - 1,
                ShareTargetRejection::TooSmall,
            ),
            (
                "shell",
                |f: &mut ShareTargetFacts| f.system_surface = true,
                ShareTargetRejection::SystemSurface,
            ),
        ];
        for (name, mutate, expected) in cases {
            let mut facts = ShareTargetFacts::ordinary(99, 42);
            mutate(&mut facts);
            assert_eq!(
                decision(facts),
                ShareTargetDecision::Rejected(expected.clone()),
                "{name}"
            );
        }
    }

    #[test]
    fn shell_class_rules_are_narrow_and_do_not_deny_explorer() {
        let mut facts = ShareTargetFacts::ordinary(99, 42);
        facts.class_name = Some("Shell_TrayWnd".to_string());
        facts.process_name = Some("explorer.exe".to_string());
        assert_eq!(
            decision(facts),
            ShareTargetDecision::Rejected(ShareTargetRejection::SystemSurface)
        );

        let mut start = ShareTargetFacts::ordinary(99, 42);
        start.class_name = Some("Windows.UI.Core.CoreWindow".to_string());
        start.process_name = Some("StartMenuExperienceHost.exe".to_string());
        assert_eq!(
            decision(start),
            ShareTargetDecision::Rejected(ShareTargetRejection::SystemSurface)
        );

        let mut tooltip = ShareTargetFacts::ordinary(99, 42);
        tooltip.class_name = Some("tooltips_class32".to_string());
        assert_eq!(
            decision(tooltip),
            ShareTargetDecision::Rejected(ShareTargetRejection::SystemSurface)
        );

        let mut quick_settings = ShareTargetFacts::ordinary(99, 42);
        quick_settings.class_name = Some("ControlCenterWindow".to_string());
        quick_settings.process_name = Some("ShellHost.exe".to_string());
        assert_eq!(
            decision(quick_settings),
            ShareTargetDecision::Rejected(ShareTargetRejection::SystemSurface)
        );

        let mut unrelated_shell_host = ShareTargetFacts::ordinary(99, 42);
        unrelated_shell_host.class_name = Some("ControlCenterWindow".to_string());
        unrelated_shell_host.process_name = Some("example.exe".to_string());
        assert_eq!(
            decision(unrelated_shell_host),
            ShareTargetDecision::Eligible(ShareTargetKind::Window)
        );
    }

    #[test]
    fn petal_regions_are_registered_targets_but_petal_chrome_is_rejected() {
        let mut region = ShareTargetFacts::ordinary(42, 42);
        region.region_selector = true;
        assert_eq!(
            decision(region),
            ShareTargetDecision::Eligible(ShareTargetKind::RegisteredRegion)
        );

        let mut chrome = ShareTargetFacts::ordinary(42, 42);
        chrome.petal_chrome = true;
        assert_eq!(
            decision(chrome),
            ShareTargetDecision::Rejected(ShareTargetRejection::PetalChrome)
        );

        let stale_region = mac_window_facts(3, 800.0, 600.0, 999, 42, None, true, false);
        assert_eq!(
            decision(stale_region),
            ShareTargetDecision::Eligible(ShareTargetKind::RegisteredRegion)
        );
    }

    #[test]
    fn mac_projection_keeps_bundle_and_layer_policy_in_the_same_classifier() {
        let facts = mac_window_facts(
            0,
            800.0,
            600.0,
            99,
            42,
            Some("com.apple.controlcenter"),
            false,
            false,
        );
        assert_eq!(
            decision(facts),
            ShareTargetDecision::Rejected(ShareTargetRejection::DenylistedBundle)
        );
        let facts = mac_window_facts(4, 800.0, 600.0, 99, 42, None, false, false);
        assert_eq!(
            decision(facts),
            ShareTargetDecision::Rejected(ShareTargetRejection::NonNormalLayer)
        );
    }

    #[test]
    fn decision_exposes_a_rejection_without_string_parsing() {
        let mut facts = ShareTargetFacts::ordinary(99, 42);
        facts.system_surface = true;
        let result = decision(facts);
        assert_eq!(
            result.rejection(),
            Some(&ShareTargetRejection::SystemSurface)
        );
        assert!(!result.is_eligible());
        assert_eq!(result.kind(), None);
    }
}
