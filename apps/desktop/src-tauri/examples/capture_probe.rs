//! M0 spike, stage (a): confirm real `SCStream` frames flow for a real
//! on-screen window, end to end, before wiring anything else up.
//!
//! Usage: `cargo run --example capture_probe -- [window_id] [--geometry]`
//!
//! With no `window_id`, lists shareable windows and picks the first one.
//!
//! `--geometry` (or `PETAL_PROBE_GEOMETRY=1`) additionally runs the
//! **capture-geometry integrity harness** (#531): it captures the same window
//! through BOTH the direct-window-id path and the system-picker filter path,
//! decodes the delivered NV12 raster, and asserts numerically that
//!
//!   * the raster's aspect/orientation matches the source window's real point
//!     aspect, and
//!   * the *useful content* (non-black pixels) fills that raster,
//!
//! which is exactly what "a malformed portrait raster carrying landscape
//! content with black padding" violates. Set `PETAL_PROBE_DUMP_DIR=<dir>` to
//! also write the Y plane of the first accepted frame as a PGM.
//!
//! This intentionally does NOT touch LiveKit -- it only proves
//! `capture.rs`'s `WindowCapture` receives real frames (real resolution,
//! real frame count over time), so any problem below this line can't be
//! blamed on the transport.

#[cfg(target_os = "macos")]
mod probe {
    use desktop_lib::capture::{CapturedFrame, CapturedFramePayload};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// NV12 video-range black is Y=16. Anything at or below this is padding,
    /// not content; a small margin absorbs real-capture rounding.
    const BLACK_Y_MAX: u8 = 24;

    /// Fraction of a raster edge that may be padding before the raster counts
    /// as malformed. Real windows can carry a genuinely dark edge row, so a
    /// zero-tolerance bound would false-positive.
    const MAX_PADDING_FRACTION: f64 = 0.06;

    /// Aspect agreement tolerance between the delivered raster and the source
    /// window's point geometry. Capture rounds to even pixel dimensions and
    /// caps the long edge, so exact equality is not expected.
    const MAX_ASPECT_ERROR: f64 = 0.05;

    #[derive(Debug, Clone, Copy)]
    pub struct RasterGeometry {
        pub width: u32,
        pub height: u32,
        pub y_stride: u32,
        pub source_scale: f64,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct ContentBounds {
        pub left: u32,
        pub top: u32,
        pub right: u32,
        pub bottom: u32,
    }

    impl ContentBounds {
        pub fn width(&self) -> u32 {
            self.right.saturating_sub(self.left) + 1
        }
        pub fn height(&self) -> u32 {
            self.bottom.saturating_sub(self.top) + 1
        }
    }

    /// Bounding box of non-black pixels in an NV12 Y plane. Stride-aware and
    /// pure, so it is testable without a real capture.
    pub fn content_bounds(
        y_plane: &[u8],
        stride: usize,
        width: u32,
        height: u32,
    ) -> Option<ContentBounds> {
        if width == 0 || height == 0 || stride < width as usize {
            return None;
        }
        let mut left = u32::MAX;
        let mut top = u32::MAX;
        let mut right = 0u32;
        let mut bottom = 0u32;
        let mut any = false;
        for row in 0..height {
            let start = row as usize * stride;
            let Some(line) = y_plane.get(start..start + width as usize) else {
                break;
            };
            for (col, sample) in line.iter().enumerate() {
                if *sample > BLACK_Y_MAX {
                    let col = col as u32;
                    any = true;
                    left = left.min(col);
                    right = right.max(col);
                    top = top.min(row);
                    bottom = bottom.max(row);
                }
            }
        }
        any.then_some(ContentBounds {
            left,
            top,
            right,
            bottom,
        })
    }

    #[derive(Debug)]
    pub struct GeometryVerdict {
        pub aspect_error: f64,
        pub fill_ratio: f64,
        pub padding_fraction: f64,
        pub failures: Vec<String>,
    }

    /// Decide, numerically, whether a raster and its content agree with the
    /// source window's real geometry.
    pub fn evaluate_geometry(
        raster: RasterGeometry,
        bounds: Option<ContentBounds>,
        source_point_width: f64,
        source_point_height: f64,
    ) -> GeometryVerdict {
        let mut failures = Vec::new();

        let source_aspect = source_point_width.max(1.0) / source_point_height.max(1.0);
        let raster_aspect = f64::from(raster.width.max(1)) / f64::from(raster.height.max(1));
        let aspect_error = (raster_aspect - source_aspect).abs() / source_aspect;
        if aspect_error > MAX_ASPECT_ERROR {
            failures.push(format!(
                "raster aspect {raster_aspect:.4} ({}x{}) disagrees with source aspect \
                 {source_aspect:.4} ({source_point_width:.0}x{source_point_height:.0}pt) by \
                 {:.1}% (max {:.1}%)",
                raster.width,
                raster.height,
                aspect_error * 100.0,
                MAX_ASPECT_ERROR * 100.0
            ));
        }
        if (raster_aspect >= 1.0) != (source_aspect >= 1.0) {
            failures.push(format!(
                "raster orientation ({}) does not match source orientation ({})",
                orientation(raster_aspect),
                orientation(source_aspect)
            ));
        }

        let (fill_ratio, padding_fraction) = match bounds {
            Some(bounds) => {
                let fill = f64::from(bounds.width()) * f64::from(bounds.height())
                    / (f64::from(raster.width.max(1)) * f64::from(raster.height.max(1)));
                let pad_w = 1.0 - f64::from(bounds.width()) / f64::from(raster.width.max(1));
                let pad_h = 1.0 - f64::from(bounds.height()) / f64::from(raster.height.max(1));
                (fill, pad_w.max(pad_h))
            }
            None => {
                failures.push("raster carries no non-black content at all".to_string());
                (0.0, 1.0)
            }
        };

        if let Some(bounds) = bounds {
            if padding_fraction > MAX_PADDING_FRACTION {
                failures.push(format!(
                    "content box {}x{} at ({},{}) leaves {:.1}% of an edge as padding in a \
                     {}x{} raster (max {:.1}%)",
                    bounds.width(),
                    bounds.height(),
                    bounds.left,
                    bounds.top,
                    padding_fraction * 100.0,
                    raster.width,
                    raster.height,
                    MAX_PADDING_FRACTION * 100.0
                ));
            }
        }

        GeometryVerdict {
            aspect_error,
            fill_ratio,
            padding_fraction,
            failures,
        }
    }

    pub fn orientation(aspect: f64) -> &'static str {
        if aspect >= 1.0 {
            "landscape"
        } else {
            "portrait"
        }
    }

    pub struct FirstRaster {
        pub geometry: RasterGeometry,
        pub bounds: Option<ContentBounds>,
        pub y_plane: Vec<u8>,
    }

    /// Positive control for the geometry oracle itself (#200/#531).
    ///
    /// A PASS from the live harness is uninterpretable unless the oracle is
    /// known to reject a raster it should reject. This synthesizes, on the
    /// CPU, (a) a well-formed landscape raster that must be accepted, and
    /// (b) the exact raster shape reported against 0.7.12 -- a PORTRAIT
    /// raster carrying a LANDSCAPE band of content at the top with the rest
    /// black -- which must be rejected on both orientation and padding.
    ///
    /// Returns true only if both expectations hold. If it returns false the
    /// measurement apparatus is broken and no live verdict may be reported.
    pub fn oracle_self_check() -> bool {
        // Source window: landscape 1600x900pt.
        let (src_w, src_h) = (1600.0_f64, 900.0_f64);

        fn synth(width: u32, height: u32, fill_h: u32) -> (Vec<u8>, usize) {
            let stride = width as usize + 32; // exercise the stride path
            let mut plane = vec![0u8; stride * height as usize];
            for row in 0..fill_h.min(height) {
                let start = row as usize * stride;
                for col in 0..width as usize {
                    plane[start + col] = 200;
                }
            }
            (plane, stride)
        }

        let mut ok = true;

        // (a) well-formed: landscape raster, content fills it.
        let (plane, stride) = synth(1600, 900, 900);
        let bounds = content_bounds(&plane, stride, 1600, 900);
        let good = evaluate_geometry(
            RasterGeometry {
                width: 1600,
                height: 900,
                y_stride: stride as u32,
                source_scale: 1.0,
            },
            bounds,
            src_w,
            src_h,
        );
        if good.failures.is_empty() {
            println!(
                "[control-a] PASS: well-formed 1600x900 raster accepted \
                 (aspect_error={:.3}% fill={:.3})",
                good.aspect_error * 100.0,
                good.fill_ratio
            );
        } else {
            ok = false;
            for f in &good.failures {
                eprintln!("[control-a] BROKEN ORACLE: rejected a good raster: {f}");
            }
        }

        // (b) the reported #200/0.7.12 symptom: portrait 900x1600 raster,
        // landscape content squeezed into the top 506 rows, rest black.
        let (plane, stride) = synth(900, 1600, 506);
        let bounds = content_bounds(&plane, stride, 900, 1600);
        let bad = evaluate_geometry(
            RasterGeometry {
                width: 900,
                height: 1600,
                y_stride: stride as u32,
                source_scale: 1.0,
            },
            bounds,
            src_w,
            src_h,
        );
        if bad.failures.is_empty() {
            ok = false;
            eprintln!(
                "[control-b] BROKEN ORACLE: accepted the malformed portrait raster \
                 (aspect_error={:.3}% max_edge_padding={:.3}%)",
                bad.aspect_error * 100.0,
                bad.padding_fraction * 100.0
            );
        } else {
            println!(
                "[control-b] PASS: malformed portrait raster rejected with {} finding(s) \
                 (aspect_error={:.1}% max_edge_padding={:.1}%)",
                bad.failures.len(),
                bad.aspect_error * 100.0,
                bad.padding_fraction * 100.0
            );
            for f in &bad.failures {
                println!("           - {f}");
            }
        }

        ok
    }

    pub struct Collected {
        pub frames: u64,
        pub first: Option<FirstRaster>,
        pub errors: Vec<String>,
    }

    pub struct Collector {
        frames: Arc<AtomicU64>,
        first: Arc<Mutex<Option<FirstRaster>>>,
        errors: Arc<Mutex<Vec<String>>>,
    }

    impl Collector {
        pub fn new() -> Self {
            Self {
                frames: Arc::new(AtomicU64::new(0)),
                first: Arc::new(Mutex::new(None)),
                errors: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn on_frame(&self) -> impl Fn(CapturedFrame) + Send + Sync + 'static {
            let frames = self.frames.clone();
            let first = self.first.clone();
            move |frame: CapturedFrame| {
                frames.fetch_add(1, Ordering::Relaxed);
                let mut slot = first.lock().unwrap();
                if slot.is_some() {
                    return;
                }
                let payload = match &frame.payload {
                    CapturedFramePayload::Native { pixel_buffer } => {
                        match pixel_buffer.copy_nv12_payload() {
                            Ok(payload) => payload,
                            Err(e) => {
                                eprintln!("  (could not copy NV12 payload: {e})");
                                return;
                            }
                        }
                    }
                    _ => return,
                };
                let CapturedFramePayload::Nv12 { y, y_stride, .. } = &payload else {
                    return;
                };
                let geometry = RasterGeometry {
                    width: frame.width,
                    height: frame.height,
                    y_stride: *y_stride,
                    source_scale: frame.source_scale,
                };
                let bounds = content_bounds(y, *y_stride as usize, frame.width, frame.height);
                *slot = Some(FirstRaster {
                    geometry,
                    bounds,
                    y_plane: y.to_vec(),
                });
            }
        }

        pub fn on_error(&self) -> impl Fn(String) + Send + Sync + 'static {
            let errors = self.errors.clone();
            move |e: String| {
                errors.lock().unwrap().push(e);
            }
        }

        pub fn errors_snapshot(&self) -> Vec<String> {
            self.errors.lock().unwrap().clone()
        }

        pub fn take(self) -> Collected {
            Collected {
                frames: self.frames.load(Ordering::Relaxed),
                first: self.first.lock().unwrap().take(),
                errors: self.errors.lock().unwrap().clone(),
            }
        }
    }

    pub fn write_pgm(
        path: &std::path::Path,
        y_plane: &[u8],
        stride: usize,
        width: u32,
        height: u32,
    ) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        write!(file, "P5\n{width} {height}\n255\n")?;
        for row in 0..height {
            let start = row as usize * stride;
            let Some(line) = y_plane.get(start..start + width as usize) else {
                break;
            };
            file.write_all(line)?;
        }
        Ok(())
    }
}

/// Local mirror of `capture::parse_layout_reconfigure` (which is crate-private).
/// The wire string is `capture-layout-reconfigure:<w>x<h>`.
#[cfg(target_os = "macos")]
fn parse_reconfigure_event(event: &str) -> Option<(u32, u32)> {
    let dims = event.strip_prefix("capture-layout-reconfigure:")?;
    let (w, h) = dims.split_once('x')?;
    let w: u32 = w.parse().ok()?;
    let h: u32 = h.parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(target_os = "macos")]
fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let geometry_mode = args.iter().any(|a| a == "--geometry")
        || std::env::var("PETAL_PROBE_GEOMETRY").is_ok_and(|v| v != "0");
    let window_id: Option<u32> = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .and_then(|s| s.parse().ok());

    if !desktop_lib::window_source::has_screen_recording_access() {
        eprintln!(
            "BLOCKED: Screen Recording permission is not granted to this binary \
             (target/debug/examples/capture_probe). Grant it in System Settings -> \
             Privacy & Security -> Screen Recording for your terminal app (or this \
             binary directly), then re-run. macOS ties this grant to the exact \
             executable path, so granting it for the Petal.app bundle elsewhere does \
             NOT cover this example binary."
        );
        std::process::exit(1);
    }

    let windows = match desktop_lib::window_source::list() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Failed to enumerate windows: {e}");
            std::process::exit(1);
        }
    };

    if windows.is_empty() {
        eprintln!("No shareable windows found (unexpected with permission granted).");
        std::process::exit(1);
    }

    let target = match window_id {
        Some(id) => windows
            .iter()
            .find(|w| w.window_id == id)
            .unwrap_or_else(|| {
                eprintln!("window_id {id} not found in current shareable windows. Available:");
                for w in &windows {
                    eprintln!(
                        "  {} - {} ({})",
                        w.window_id,
                        w.app_name,
                        w.title.as_deref().unwrap_or("")
                    );
                }
                std::process::exit(1);
            }),
        None => {
            println!("No window_id given; available shareable windows:");
            for w in &windows {
                println!(
                    "  {} - {} ({})",
                    w.window_id,
                    w.app_name,
                    w.title.as_deref().unwrap_or("")
                );
            }
            let first = &windows[0];
            println!(
                "\nDefaulting to first window: {} - {}",
                first.window_id, first.app_name
            );
            first
        }
    };

    println!(
        "Starting capture of window {} ({} - {:?})",
        target.window_id, target.app_name, target.title
    );

    if geometry_mode {
        run_geometry_harness(target.window_id);
        return;
    }

    let frame_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let last_info = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

    let fc = frame_count.clone();
    let li = last_info.clone();
    let capture = desktop_lib::capture::WindowCapture::start(target.window_id, move |frame| {
        fc.store(frame.sequence, std::sync::atomic::Ordering::Relaxed);
        *li.lock().unwrap() = format!(
            "{}x{} scale={:.2} payload={}",
            frame.width,
            frame.height,
            frame.source_scale,
            frame.payload.payload_kind()
        );
    });

    let capture = match capture {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to start capture: {e}");
            std::process::exit(1);
        }
    };

    println!("Capturing for 5 seconds...");
    let start = std::time::Instant::now();
    let mut last_reported = 0u64;
    while start.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let count = frame_count.load(std::sync::atomic::Ordering::Relaxed);
        if count != last_reported {
            println!(
                "  t={:.1}s frames={} last_frame=[{}]",
                start.elapsed().as_secs_f32(),
                count,
                last_info.lock().unwrap()
            );
            last_reported = count;
        }
    }

    let total = frame_count.load(std::sync::atomic::Ordering::Relaxed);
    let _ = capture.stop();

    if total == 0 {
        eprintln!(
            "FAILED: zero frames received in 5 seconds. Capture pipeline is not \
             delivering real frames."
        );
        std::process::exit(1);
    }

    println!(
        "OK: received {} real frames in 5s (~{:.1} fps). window_id={}",
        total,
        total as f32 / 5.0,
        capture.window_id()
    );
}

/// The #531 harness: capture one window through both capture entry points and
/// assert the delivered raster against the window's real point geometry.
#[cfg(target_os = "macos")]
fn run_geometry_harness(window_id: u32) {
    use desktop_lib::capture::WindowCapture;
    use probe::{evaluate_geometry, oracle_self_check, orientation, write_pgm, Collector};
    use screencapturekit::shareable_content::SCShareableContent;
    use screencapturekit::stream::content_filter::SCContentFilter;

    // Positive control FIRST: a live PASS below means nothing unless the
    // oracle demonstrably rejects the malformed raster it exists to catch.
    println!("=== oracle positive control (#200/#531) ===");
    if !oracle_self_check() {
        eprintln!(
            "\nNO RESULT: the geometry oracle failed its own positive control. \
             Do not interpret any live verdict from this run."
        );
        std::process::exit(2);
    }

    let content = match SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to read shareable content: {e}");
            std::process::exit(1);
        }
    };
    let Some(window) = content
        .windows()
        .into_iter()
        .find(|w| w.window_id() == window_id)
    else {
        eprintln!("window {window_id} not present in SCShareableContent");
        std::process::exit(1);
    };

    let frame = window.frame();
    let source_w = frame.size.width;
    let source_h = frame.size.height;
    println!(
        "\n=== #531 capture-geometry harness ===\nsource window {window_id}: \
         {source_w:.1}x{source_h:.1}pt at ({:.1},{:.1}) aspect {:.4} ({})",
        frame.origin.x,
        frame.origin.y,
        source_w / source_h,
        orientation(source_w / source_h)
    );

    let probe_filter = SCContentFilter::create().with_window(&window).build();
    let filter_rect = probe_filter.content_rect();
    let point_pixel_scale = f64::from(probe_filter.point_pixel_scale()).max(1.0);
    println!(
        "picker-equivalent filter: contentRect {:.1}x{:.1}pt at ({:.1},{:.1}), \
         pointPixelScale {point_pixel_scale:.2} => {:.0}x{:.0}px",
        filter_rect.size.width,
        filter_rect.size.height,
        filter_rect.origin.x,
        filter_rect.origin.y,
        filter_rect.size.width * point_pixel_scale,
        filter_rect.size.height * point_pixel_scale
    );

    // A geometry verdict is meaningful only after a short healthy stream has
    // produced several rasters; one callback is not evidence of a working arm.
    const MIN_HEALTHY_RASTERS: u64 = 3;

    let dump_dir = std::env::var("PETAL_PROBE_DUMP_DIR").ok();
    let mut any_failure = false;

    // "swapped" manufactures the exact #531 condition: a stream configured
    // portrait for a landscape source. ScreenCaptureKit aspect-fits into the
    // configured size, so the delivered raster is portrait with landscape
    // content in a band and black padding -- the malformed raster the issue
    // describes. It must NOT reach a consumer as an accepted frame.
    for pass in ["direct", "picker-filter", "swapped"] {
        let collector = Collector::new();
        let on_frame = collector.on_frame();
        let on_error = collector.on_error();
        let capture = if pass == "direct" {
            WindowCapture::start_with_error_handler(window_id, 30, on_frame, on_error)
        } else {
            let filter = SCContentFilter::create().with_window(&window).build();
            let (logical_w, logical_h) = if pass == "swapped" {
                (filter_rect.size.height, filter_rect.size.width)
            } else {
                (filter_rect.size.width, filter_rect.size.height)
            };
            WindowCapture::start_with_picker_filter(
                window_id,
                filter,
                logical_w,
                logical_h,
                point_pixel_scale,
                desktop_lib::video_color::VideoColorProfile::legacy_publish_default(),
                30,
                on_frame,
                on_error,
            )
        };
        let capture = match capture {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[{pass}] failed to start capture: {e}");
                any_failure = true;
                continue;
            }
        };
        std::thread::sleep(std::time::Duration::from_secs(3));

        println!("\n--- pass: {pass} ---");

        // Mirror `session/share.rs`'s real recovery wiring: a layout
        // reconfigure event is not just logged, it drives
        // `update_stream_configuration` and the stream must then converge on a
        // coherent raster. Exercising the real chain (not just the pure
        // decision fn) is the point.
        let pending = collector.errors_snapshot();
        if pass == "swapped" {
            if let Some((w, h)) = pending
                .iter()
                .rev()
                .find_map(|e| parse_reconfigure_event(e))
            {
                println!("recovery: stream asked to reconfigure to {w}x{h}; applying it");
                let config = capture.configuration_handle();
                match config.update_stream_configuration(
                    w,
                    h,
                    30,
                    desktop_lib::transport::publisher::CaptureResolution::default(),
                ) {
                    Ok(()) => std::thread::sleep(std::time::Duration::from_secs(2)),
                    Err(e) => {
                        eprintln!("[{pass}] FAIL: reconfiguration rejected: {e}");
                        any_failure = true;
                    }
                }
            }
        }

        let _ = capture.stop();
        drop(capture);
        let collected = collector.take();

        println!("frames delivered: {}", collected.frames);
        if !collected.errors.is_empty() {
            println!("capture layout events: {:?}", collected.errors);
        }
        if collected.frames == 0 {
            // Zero-evidence control (#622), constructed by blocking delivery:
            // `frames delivered: 0` then `[swapped] INSUFFICIENT DATA: no NV12
            // raster arrived in 3s; cannot demonstrate the capture-layout gate`.
            eprintln!(
                "[{pass}] INSUFFICIENT DATA: no accepted NV12 raster arrived in 3s; \
                 cannot evaluate geometry or demonstrate the capture-layout gate"
            );
            any_failure = true;
            continue;
        }
        if pass != "swapped" && collected.frames < MIN_HEALTHY_RASTERS {
            eprintln!(
                "[{pass}] INSUFFICIENT DATA: only {} accepted NV12 raster(s) in 3s \
                 (need at least {MIN_HEALTHY_RASTERS} healthy rasters)",
                collected.frames
            );
            any_failure = true;
            continue;
        }
        let Some(first) = collected.first else {
            unreachable!("a nonzero collector frame count must retain its first raster");
        };
        let raster = first.geometry;
        println!(
            "raster: {}x{}px stride={} source_scale={:.2} ({})",
            raster.width,
            raster.height,
            raster.y_stride,
            raster.source_scale,
            orientation(f64::from(raster.width) / f64::from(raster.height))
        );
        match first.bounds {
            Some(b) => println!(
                "content bbox: {}x{} at ({},{}) [right={} bottom={}]",
                b.width(),
                b.height(),
                b.left,
                b.top,
                b.right,
                b.bottom
            ),
            None => println!("content bbox: NONE (raster is entirely black)"),
        }

        if let Some(dir) = &dump_dir {
            let _ = std::fs::create_dir_all(dir);
            let path = std::path::Path::new(dir).join(format!("capture-{pass}-y.pgm"));
            match write_pgm(
                &path,
                &first.y_plane,
                raster.y_stride as usize,
                raster.width,
                raster.height,
            ) {
                Ok(()) => println!("wrote {}", path.display()),
                Err(e) => eprintln!("could not write {}: {e}", path.display()),
            }
        }

        let verdict = evaluate_geometry(raster, first.bounds, source_w, source_h);
        println!(
            "verdict: aspect_error={:.3}% fill_ratio={:.3} max_edge_padding={:.3}%",
            verdict.aspect_error * 100.0,
            verdict.fill_ratio,
            verdict.padding_fraction * 100.0
        );
        if verdict.failures.is_empty() {
            println!("[{pass}] PASS: raster geometry agrees with source geometry");
        } else {
            any_failure = true;
            for failure in &verdict.failures {
                eprintln!("[{pass}] FAIL: {failure}");
            }
        }
    }

    if any_failure {
        eprintln!("\n#531 harness: FAILED (malformed capture geometry reproduced)");
        std::process::exit(1);
    }
    println!("\n#531 harness: PASSED (both capture paths produce coherent geometry)");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("capture_probe is macOS-only.");
    std::process::exit(1);
}
