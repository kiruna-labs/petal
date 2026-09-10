//! Host-independent decision for a FAILED `GraphicsCaptureSession.
//! IsBorderRequired` write (#163).
//!
//! The setter lives on `IGraphicsCaptureSession2`, which exists from Windows
//! 10 build 20348. Below that the write fails with `E_NOINTERFACE` and WGC
//! draws its capture border unconditionally -- the setter was added later
//! precisely so the border could be turned OFF. So what a failed write means
//! depends on the DIRECTION of the request, not on the indicator mode:
//!
//! - asked to SHOW the border and could not: the border was never off, so the
//!   system indicator is already in effect. Benign; continue under `System`.
//! - asked to HIDE the border and could not: WGC's state is uncertain and a
//!   local replacement is about to become the only visible indicator. Never
//!   proceed borderless; the caller restores the system indicator first.
//!
//! This module has no Windows dependency on purpose: `windows_screen_capture`
//! is `cfg(target_os = "windows")` end to end, so the decision is kept here
//! where it is unit-tested on every host and the gated call site delegates.
//! It must not depend on `logging`'s Sentry machinery either (the Sentry tag
//! is mapped at the call site), same split `browser_url` documents.

/// `E_NOINTERFACE` (0x80004002): the object does not implement the requested
/// interface -- here, `IGraphicsCaptureSession2` on a pre-20348 build.
/// Hand-typed so this module stays host-independent; the Windows-gated test
/// `e_nointerface_matches_the_sdk_constant` pins it to the SDK's value.
pub(crate) const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;

/// Bounded classification of the failing `HRESULT`, so a field occurrence is
/// attributable and groupable -- the raw error `Display` carries a localized
/// OS string (`不支持此接口`) that cannot be matched on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BorderFailureCause {
    /// `E_NOINTERFACE`: this Windows build has no `IGraphicsCaptureSession2`.
    NoInterface,
    /// Any other `HRESULT`; the numeric code is logged alongside.
    Other,
}

impl BorderFailureCause {
    pub(crate) fn classify(hresult: i32) -> Self {
        if hresult == E_NOINTERFACE {
            Self::NoInterface
        } else {
            Self::Other
        }
    }

    /// Stable, log-greppable name (never the OS text).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoInterface => "e_nointerface",
            Self::Other => "other",
        }
    }
}

/// What the capture-thread call site must do after a failed write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BorderFailureDecision {
    /// The border could not be turned ON, which means it was never OFF.
    /// `System` mode is satisfied; continue with the system indicator.
    ContinueWithSystemBorder,
    /// The border could not be turned OFF. Never proceed borderless: restore
    /// the system indicator and disable the local replacement first, and
    /// treat a failed restore as terminal (the existing `Petal` guarantee).
    RefuseBorderless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BorderRequestFailure {
    /// The value that was passed to `SetIsBorderRequired`.
    pub(crate) requested_border: bool,
    pub(crate) hresult: i32,
    pub(crate) cause: BorderFailureCause,
    pub(crate) decision: BorderFailureDecision,
}

/// Decide what a failed `SetIsBorderRequired(requested_border)` means.
///
/// `requested_border` is the value that was written (`true` = show WGC's
/// border, `false` = hide it); `hresult` is the failing code.
pub(crate) fn on_border_request_failure(
    requested_border: bool,
    hresult: i32,
) -> BorderRequestFailure {
    let cause = BorderFailureCause::classify(hresult);
    // Direction alone decides. The code only classifies the cause: a SHOW
    // that fails for ANY reason left the border where it was (on -- WGC's
    // default, and unconditional where the setter is missing), and a HIDE
    // that fails for ANY reason must not be trusted to have left it on.
    // Before #163 a failed SHOW aborted the share for failing to request what
    // the system was already doing.
    let decision = if requested_border {
        BorderFailureDecision::ContinueWithSystemBorder
    } else {
        BorderFailureDecision::RefuseBorderless
    };
    BorderRequestFailure {
        requested_border,
        hresult,
        cause,
        decision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RO_E_CLOSED` -- a plausible non-interface failure (session already
    /// closed); any code other than `E_NOINTERFACE` classifies as `Other`.
    const RO_E_CLOSED: i32 = 0x8000_0013_u32 as i32;
    const E_FAIL: i32 = 0x8000_4005_u32 as i32;

    /// The #163 shape: `System` mode asks WGC to SHOW its border on a build
    /// with no `IGraphicsCaptureSession2`. The border is unconditional there,
    /// so the request is already satisfied -- the share must continue.
    #[test]
    fn failing_to_show_the_border_continues_with_the_system_border() {
        let failure = on_border_request_failure(true, E_NOINTERFACE);
        assert_eq!(
            failure.decision,
            BorderFailureDecision::ContinueWithSystemBorder
        );
        assert_eq!(failure.cause, BorderFailureCause::NoInterface);
        assert!(failure.requested_border);
        assert_eq!(failure.hresult, E_NOINTERFACE);
    }

    /// Direction, not the specific code, is the safety property: a SHOW that
    /// fails for any other reason is still benign (the border was never off).
    #[test]
    fn failing_to_show_the_border_is_benign_for_any_hresult() {
        for hresult in [RO_E_CLOSED, E_FAIL, -1, 1] {
            let failure = on_border_request_failure(true, hresult);
            assert_eq!(
                failure.decision,
                BorderFailureDecision::ContinueWithSystemBorder,
                "hresult {hresult:#010x}"
            );
            assert_eq!(failure.cause, BorderFailureCause::Other);
        }
    }

    /// The `Petal` guarantee is unchanged: a failed HIDE must never proceed
    /// borderless, whatever the code -- including `E_NOINTERFACE`, where the
    /// border is in fact still showing but the caller must still restore the
    /// system indicator before touching the local replacement.
    #[test]
    fn failing_to_hide_the_border_refuses_to_proceed_borderless() {
        for (hresult, cause) in [
            (E_NOINTERFACE, BorderFailureCause::NoInterface),
            (RO_E_CLOSED, BorderFailureCause::Other),
            (E_FAIL, BorderFailureCause::Other),
        ] {
            let failure = on_border_request_failure(false, hresult);
            assert_eq!(
                failure.decision,
                BorderFailureDecision::RefuseBorderless,
                "hresult {hresult:#010x}"
            );
            assert_eq!(failure.cause, cause, "hresult {hresult:#010x}");
            assert!(!failure.requested_border);
        }
    }

    /// The decision is a function of direction only; the code never flips it.
    /// (A hide failure becomes terminal only if the RESTORE then fails, which
    /// is the call site's business, not this module's.)
    #[test]
    fn direction_alone_decides_and_the_code_only_classifies() {
        for hresult in [E_NOINTERFACE, RO_E_CLOSED, E_FAIL, 0, -1] {
            let show = on_border_request_failure(true, hresult);
            let hide = on_border_request_failure(false, hresult);
            assert_eq!(
                show.decision,
                BorderFailureDecision::ContinueWithSystemBorder
            );
            assert_eq!(hide.decision, BorderFailureDecision::RefuseBorderless);
            assert_eq!(show.cause, hide.cause, "hresult {hresult:#010x}");
            assert_eq!(show.cause, BorderFailureCause::classify(hresult));
        }
    }

    #[test]
    fn cause_classification_is_exact_and_names_are_stable() {
        assert_eq!(E_NOINTERFACE, 0x8000_4002_u32 as i32);
        assert_eq!(
            BorderFailureCause::classify(E_NOINTERFACE),
            BorderFailureCause::NoInterface
        );
        assert_eq!(
            BorderFailureCause::classify(E_NOINTERFACE + 1),
            BorderFailureCause::Other
        );
        assert_eq!(BorderFailureCause::classify(0), BorderFailureCause::Other);
        assert_eq!(BorderFailureCause::NoInterface.as_str(), "e_nointerface");
        assert_eq!(BorderFailureCause::Other.as_str(), "other");
    }
}
