//! #535 (PETAL-DESKTOP-4Y): tiny frames pushed through the REAL vendored
//! libwebrtc capture path must be dropped, not abort the process.
//!
//! The field crash was `NativeVideoSource::capture_frame` ->
//! `InternalSource::on_captured_frame` -> `CropAndScale` ->
//! `I420Buffer::Create` RTC_CHECK -> SIGABRT. `InternalSource` is an
//! `AdaptedVideoTrackSource(4)`, so once a sink is attached the adapter
//! rounds any edge below 4 px down to a 0 crop/output size. Webrtc-sys
//! Patch 8 drops those frames; these tests push them for real.
//!
//! An abort kills the whole test binary, so the frames go through a child
//! copy of this binary and the parent asserts on its exit status.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use futures::{FutureExt, StreamExt};
use livekit::webrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use livekit::webrtc::peer_connection_factory::PeerConnectionFactory;
use livekit::webrtc::prelude::*;
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_stream::native::NativeVideoStream;

use super::publisher::{
    capture_frame_counting_source_drops, degenerate_frame_diagnostic, frame_below_publishable_edge,
    ADAPTER_MIN_EDGE,
};
use crate::logging::{build_sentry_diagnostic_event, DegenerateAxisTag, SentryDiagnosticEvent};

const CHILD_ENV: &str = "PETAL_TINY_FRAME_RUNTIME_CHILD";
const CHILD_TEST: &str = "tiny_frames_child_pushes_through_real_libwebrtc";
const CHILD_BUDGET: Duration = Duration::from_secs(120);
/// Below the adapter's alignment of 4 on at least one edge, as in the issue.
const TINY: [(u32, u32); 5] = [(1, 1), (2, 2), (3, 3), (2, 720), (1280, 2)];
const NORMAL: (u32, u32) = (1280, 720);

fn i420(width: u32, height: u32) -> I420Buffer {
    let mut buf = I420Buffer::new(width, height);
    let (y, u, v) = buf.data_mut();
    y.fill(128);
    u.fill(128);
    v.fill(128);
    buf
}

fn capture(source: &NativeVideoSource, (width, height): (u32, u32)) -> bool {
    source.capture_frame(&VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        timestamp_us: 0,
        frame_metadata: None,
        buffer: i420(width, height),
    })
}

/// The publisher's real push helper: `Err(axis)` when the source dropped it.
fn push_counted(
    source: &NativeVideoSource,
    (width, height): (u32, u32),
) -> Result<bool, DegenerateAxisTag> {
    capture_frame_counting_source_drops(
        source,
        &VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: 0,
            frame_metadata: None,
            buffer: i420(width, height),
        },
    )
}

/// Sizes of the frames the sink has received so far (delivery is synchronous).
fn drain(stream: &mut NativeVideoStream) -> Vec<(u32, u32)> {
    let mut sizes = Vec::new();
    while let Some(Some(frame)) = stream.next().now_or_never() {
        sizes.push((frame.buffer.width(), frame.buffer.height()));
    }
    sizes
}


/// Runs only inside the child the parent spawns; a no-op when run directly.
#[test]
#[ignore = "child of tiny_frames_through_real_libwebrtc_do_not_abort"]
fn tiny_frames_child_pushes_through_real_libwebrtc() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let factory = PeerConnectionFactory::default();
    let source = NativeVideoSource::new(
        VideoResolution {
            width: NORMAL.0,
            height: NORMAL.1,
        },
        true,
    );
    let track = factory.create_video_track("petal-535", source.clone());
    // #534: an ordinary adapter rejection (no sink yet) is `Ok(false)`, never
    // a degenerate drop -- the counter is what tells the two apart.
    assert_eq!(push_counted(&source, (2, 2)), Ok(false), "sinkless push misread as a drop");
    assert_eq!(source.degenerate_frames_dropped(), 0);
    // The sink is what engages the adapter: with no sink `AdaptFrame`
    // returns false before any crop math, and the test would be vacuous.
    let mut stream = NativeVideoStream::new(track);

    assert!(capture(&source, NORMAL), "control frame rejected before tiny frames");
    assert_eq!(drain(&mut stream), vec![NORMAL], "sink did not engage");

    for size in TINY {
        eprintln!("issue535: pushing {}x{}", size.0, size.1);
        assert!(!capture(&source, size), "{size:?} frame was accepted");
        assert_eq!(drain(&mut stream), Vec::new(), "{size:?} frame reached the sink");
    }
    assert_eq!(
        source.degenerate_frames_dropped(),
        TINY.len() as u64,
        "#534: every C++ guard drop must be visible to Rust"
    );

    // #534: the Rust floor normally skips these first. Push them past it, via
    // the publisher's own capture helper, so only the C++ guard stands.
    for size in TINY {
        assert!(frame_below_publishable_edge(size.0, size.1, ADAPTER_MIN_EDGE), "{size:?}");
        let before = source.degenerate_frames_dropped();
        let axis = push_counted(&source, size).expect_err("C++ drop read as a delivered frame");
        assert_eq!(axis, DegenerateAxisTag::Adapter, "{size:?}");
        assert_eq!(source.degenerate_frames_dropped(), before + 1, "{size:?}");
        assert_eq!(drain(&mut stream), Vec::new(), "{size:?} frame reached the sink");
        let event = build_sentry_diagnostic_event(
            SentryDiagnosticEvent::DegenerateFrameSkipped(degenerate_frame_diagnostic(
                size, NORMAL, axis,
            )),
            "1",
        );
        assert_eq!(event.tags.get("degenerate_axis").map(String::as_str), Some("adapter"));
        assert!(
            event.message.as_deref().is_some_and(|m| m.starts_with("diagnostic: degenerate-frame-skipped ")),
            "{:?}",
            event.message
        );
    }

    let dropped = source.degenerate_frames_dropped();
    assert_eq!(push_counted(&source, NORMAL), Ok(true), "normal frame rejected after tiny frames");
    assert_eq!(source.degenerate_frames_dropped(), dropped, "a delivered frame bumped the counter");
    assert_eq!(drain(&mut stream), vec![NORMAL], "pipeline wedged after tiny frames");
    println!("issue535: child survived every tiny frame");
}

#[test]
fn tiny_frames_through_real_libwebrtc_do_not_abort() {
    // libtest names omit the crate segment of module_path!().
    let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    let name = format!("{module}::{CHILD_TEST}");
    let exe = std::env::current_exe().expect("test binary path");
    let mut child = Command::new(exe)
        .args([name.as_str(), "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child test");
    let deadline = Instant::now() + CHILD_BUDGET;
    while child.try_wait().expect("poll child").is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().expect("child output");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "pushing tiny frames through libwebrtc failed or aborted ({:?}); the \
         webrtc-sys Patch 8 guard in video_track.cpp is the likely regression.\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status
    );
    // A filter that matches nothing also exits 0: prove the child ran.
    assert!(
        stdout.contains("1 passed") && stdout.contains("child survived every tiny frame"),
        "child test did not run: {stdout}"
    );
}
