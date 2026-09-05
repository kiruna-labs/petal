// Deterministic moving text target for #613 real-capture measurements.
//
// Usage: swift latency-target-window.swift [--width 1600] [--height 900]
//        [--fps 30] [--seconds 120] [--origin-x 60] [--origin-y 60]
//        [--presentation-pattern] [--target-style borderless|decorated]
//        [--display-id <CGDirectDisplayID>] [--list-displays] [--self-test]
//
// Prints `WINDOW_ID <n>`. The accessory app deliberately never activates or
// becomes key, so an unattended diagnostic cannot steal keyboard focus. Width
// and height are the required ScreenCaptureKit physical raster, not AppKit
// points or content-view dimensions.

import AppKit
import CoreGraphics

private var width = 1600
private var height = 900
private var fps = 30.0
private var seconds = 120.0
private var selfTest = false
private var presentationPattern = false
private var targetStyleArgument: String?
private var originX = 60.0
private var originY = 60.0
private var inspectWindowOwnerPID: Int32?
private var listDisplays = false
private var targetDisplayID: CGDirectDisplayID?

private var i = 1
while i < CommandLine.arguments.count {
    let argument = CommandLine.arguments[i]
    func next() -> String {
        i += 1
        guard i < CommandLine.arguments.count else {
            fputs("\(argument) requires a value\n", stderr)
            exit(2)
        }
        return CommandLine.arguments[i]
    }
    switch argument {
    case "--width": width = Int(next()) ?? width
    case "--height": height = Int(next()) ?? height
    case "--fps": fps = Double(next()) ?? fps
    case "--seconds": seconds = Double(next()) ?? seconds
    case "--origin-x": originX = Double(next()) ?? originX
    case "--origin-y": originY = Double(next()) ?? originY
    case "--presentation-pattern": presentationPattern = true
    case "--target-style": targetStyleArgument = next()
    case "--inspect-window-owner-pid": inspectWindowOwnerPID = Int32(next())
    case "--list-displays": listDisplays = true
    case "--display-id": targetDisplayID = CGDirectDisplayID(UInt32(next()) ?? 0)
    case "--self-test": selfTest = true
    default:
        fputs("unknown argument: \(argument)\n", stderr)
        exit(2)
    }
    i += 1
}

final class ContentView: NSView {
    var frameIndex: UInt64 = 0
    let presentationPattern: Bool
    private let font = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
    private let glyphs = Array("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ()[]{}<>=+-*/_.,:;#&|")

    init(frame: NSRect, presentationPattern: Bool) {
        self.presentationPattern = presentationPattern
        super.init(frame: frame)
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override var isFlipped: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        if presentationPattern {
            drawPresentationPattern()
            return
        }
        NSColor.white.setFill()
        bounds.fill()

        let lineHeight: CGFloat = 16
        let rows = Int(bounds.height / lineHeight) + 1
        let columns = Int(bounds.width / 7.6)
        let scroll = frameIndex / 6
        let attributes: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor.black,
        ]
        for row in 0..<rows {
            var state = UInt64(truncatingIfNeeded: (UInt64(row) &+ scroll) &* 6_364_136_223_846_793_005 &+ 1_442_695_040_888_963_407)
            var line = ""
            line.reserveCapacity(columns)
            let indent = Int(state % 8)
            for _ in 0..<indent { line.append(" ") }
            for _ in indent..<columns {
                state = state &* 6_364_136_223_846_793_005 &+ 1_442_695_040_888_963_407
                line.append(glyphs[Int((state >> 33) % UInt64(glyphs.count))])
            }
            (line as NSString).draw(
                at: NSPoint(x: 8, y: CGFloat(row) * lineHeight),
                withAttributes: attributes
            )
        }

        let x = CGFloat((frameIndex &* 11) % UInt64(max(1, Int(bounds.width) - 40)))
        let y = CGFloat((frameIndex &* 7) % UInt64(max(1, Int(bounds.height) - 40)))
        NSColor.systemBlue.setFill()
        NSRect(x: x, y: y, width: 28, height: 18).fill()
    }

    private func scaledRect(x: CGFloat, y: CGFloat, width: CGFloat, height: CGFloat) -> NSRect {
        NSRect(
            x: x * bounds.width / 960,
            y: y * bounds.height / 600,
            width: width * bounds.width / 960,
            height: height * bounds.height / 600
        )
    }

    private func drawPresentationPattern() {
        NSColor(calibratedRed: 27 / 255, green: 16 / 255, blue: 51 / 255, alpha: 1).setFill()
        bounds.fill()

        let corners: [(CGFloat, CGFloat, NSColor)] = [
            (16, 16, NSColor(calibratedRed: 1, green: 45 / 255, blue: 85 / 255, alpha: 1)),
            (920, 16, NSColor(calibratedRed: 0, green: 1, blue: 136 / 255, alpha: 1)),
            (16, 560, NSColor(calibratedRed: 45 / 255, green: 125 / 255, blue: 1, alpha: 1)),
            (920, 560, NSColor(calibratedRed: 1, green: 212 / 255, blue: 0, alpha: 1)),
        ]
        for (x, y, color) in corners {
            color.setFill()
            scaledRect(x: x, y: y, width: 24, height: 24).fill()
        }

        let value = Int(frameIndex & 0xffff)
        let gray = value ^ (value >> 1)
        for index in 0..<16 {
            let shift = 15 - index
            (((gray >> shift) & 1) == 1 ? NSColor.white : NSColor.black).setFill()
            scaledRect(x: 160 + CGFloat(index * 40), y: 88, width: 40, height: 30).fill()
        }

        NSColor(calibratedWhite: 1, alpha: 0.9).setFill()
        scaledRect(x: 352, y: 220, width: 256, height: 160).fill()
        NSColor(calibratedWhite: 0, alpha: 1).setFill()
        for row in 0..<20 {
            for column in 0..<32 where (row + column).isMultiple(of: 2) {
                scaledRect(
                    x: 352 + CGFloat(column * 8),
                    y: 220 + CGFloat(row * 8),
                    width: 8,
                    height: 8
                ).fill()
            }
        }
    }
}

private struct WindowGeometry {
    let frameSizePoints: NSSize
    let contentSizePoints: NSSize
}

private let decoratedTargetStyleMask: NSWindow.StyleMask = [.titled, .nonactivatingPanel]
private let presentationTargetStyleMask: NSWindow.StyleMask = [.borderless, .nonactivatingPanel]

private enum TargetStyle: String {
    case borderless
    case decorated

    var styleMask: NSWindow.StyleMask {
        switch self {
        case .borderless: presentationTargetStyleMask
        case .decorated: decoratedTargetStyleMask
        }
    }
}

private let targetStyle: TargetStyle
if let targetStyleArgument {
    guard let parsed = TargetStyle(rawValue: targetStyleArgument) else {
        fputs("--target-style must be borderless or decorated\n", stderr)
        exit(2)
    }
    targetStyle = parsed
} else {
    targetStyle = presentationPattern ? .borderless : .decorated
}

private func deriveWindowGeometry(
    physicalWidth: Int,
    physicalHeight: Int,
    backingScale: CGFloat,
    styleMask: NSWindow.StyleMask
) -> WindowGeometry? {
    guard physicalWidth > 0, physicalHeight > 0, backingScale > 0 else { return nil }
    let frameSize = NSSize(
        width: CGFloat(physicalWidth) / backingScale,
        height: CGFloat(physicalHeight) / backingScale
    )
    let frameRect = NSRect(origin: .zero, size: frameSize)
    let contentRect = NSWindow.contentRect(forFrameRect: frameRect, styleMask: styleMask)
    guard contentRect.width > 0, contentRect.height > 0 else { return nil }
    return WindowGeometry(frameSizePoints: frameSize, contentSizePoints: contentRect.size)
}

private func physicalRaster(for frameSize: NSSize, backingScale: CGFloat) -> (Int, Int) {
    (
        Int((frameSize.width * backingScale).rounded()),
        Int((frameSize.height * backingScale).rounded())
    )
}

/// An explicitly requested display is an apparatus contract: never silently
/// relocate its target to another screen.  Defaults may use main/first only
/// when the CLI did not name a display.
private func selectTargetDisplayID(
    available: [CGDirectDisplayID],
    requested: CGDirectDisplayID?,
    defaultID: CGDirectDisplayID?
) -> CGDirectDisplayID? {
    if let requested {
        return available.contains(requested) ? requested : nil
    }
    if let defaultID, available.contains(defaultID) { return defaultID }
    return available.first
}

private func runGeometrySelfTest() {
    precondition(
        TargetStyle.borderless.styleMask == presentationTargetStyleMask
            && TargetStyle.decorated.styleMask == decoratedTargetStyleMask,
        "target style selection regression"
    )
    for scale in [CGFloat(1), CGFloat(2)] {
        guard let geometry = deriveWindowGeometry(
            physicalWidth: 1600,
            physicalHeight: 900,
            backingScale: scale,
            styleMask: decoratedTargetStyleMask
        ) else {
            fatalError("geometry derivation unexpectedly failed at scale \(scale)")
        }
        let raster = physicalRaster(for: geometry.frameSizePoints, backingScale: scale)
        precondition(raster == (1600, 900), "physical raster regression at scale \(scale)")
        let roundTripFrame = NSWindow.frameRect(
            forContentRect: NSRect(origin: .zero, size: geometry.contentSizePoints),
            styleMask: decoratedTargetStyleMask
        )
        precondition(
            abs(roundTripFrame.width - geometry.frameSizePoints.width) < 0.01
                && abs(roundTripFrame.height - geometry.frameSizePoints.height) < 0.01,
            "window decoration round-trip regression at scale \(scale)"
        )
        precondition(
            geometry.contentSizePoints.height < geometry.frameSizePoints.height,
            "titled-window decoration must consume vertical space"
        )
        let presentationGeometry = deriveWindowGeometry(
            physicalWidth: 960,
            physicalHeight: 600,
            backingScale: scale,
            styleMask: presentationTargetStyleMask
        )!
        precondition(
            presentationGeometry.frameSizePoints == presentationGeometry.contentSizePoints,
            "borderless presentation target must have frame/content parity"
        )
    }
    precondition(
        deriveWindowGeometry(
            physicalWidth: 1600,
            physicalHeight: 900,
            backingScale: 0,
            styleMask: decoratedTargetStyleMask
        ) == nil,
        "zero backing scale must be rejected"
    )
    let screens: [CGDirectDisplayID] = [11, 22]
    precondition(
        selectTargetDisplayID(available: screens, requested: 22, defaultID: 11) == 22,
        "explicit display must be selected exactly"
    )
    precondition(
        selectTargetDisplayID(available: screens, requested: 99, defaultID: 11) == nil,
        "missing explicit display must fail closed rather than use main/first"
    )
    precondition(
        selectTargetDisplayID(available: screens, requested: nil, defaultID: 11) == 11,
        "default behavior may use main display when no explicit id was supplied"
    )
    precondition(
        selectTargetDisplayID(available: screens, requested: nil, defaultID: nil) == 11,
        "default behavior may use first display only when no explicit id was supplied"
    )
    print("SELF_TEST_PASS physical-raster geometry and decoration round-trip")
}

if selfTest {
    runGeometrySelfTest()
    exit(0)
}

if listDisplays {
    let displays: [[String: Any]] = NSScreen.screens.compactMap { screen in
        guard let id = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber else { return nil }
        let appKit = screen.frame
        let cg = CGDisplayBounds(CGDirectDisplayID(id.uint32Value))
        return [
            "id": id.uint32Value,
            "appkitX": appKit.minX, "appkitY": appKit.minY,
            "width": appKit.width, "height": appKit.height,
            "scale": screen.backingScaleFactor,
            "cgX": cg.minX, "cgY": cg.minY,
        ]
    }
    guard let data = try? JSONSerialization.data(withJSONObject: displays),
          let json = String(data: data, encoding: .utf8) else { exit(1) }
    print("DISPLAY_LAYOUT_JSON \(json)")
    exit(0)
}

if let ownerPID = inspectWindowOwnerPID {
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    let candidates = list.filter { info in
        (info[kCGWindowOwnerPID as String] as? Int32) == ownerPID
            && (info[kCGWindowLayer as String] as? Int ?? -1) == 0
            && ((info[kCGWindowBounds as String] as? [String: Any])?["Width"] as? CGFloat ?? 0) > 0
    }
    guard candidates.count == 1,
          let id = candidates[0][kCGWindowNumber as String] as? NSNumber else {
        fputs("WINDOW_INSPECTION_AMBIGUOUS owner_pid=\(ownerPID) candidates=\(candidates.count)\n", stderr)
        exit(3)
    }
    print("WINDOW_ID \(id.uint32Value)")
    print("WINDOW_INSPECTION_READY owner_pid=\(ownerPID)")
    exit(0)
}

guard width > 0, height > 0, fps > 0, seconds > 0 else {
    fputs("width, height, fps, and seconds must be positive\n", stderr)
    exit(2)
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)

let availableScreens = NSScreen.screens.compactMap { screen -> (id: CGDirectDisplayID, screen: NSScreen)? in
    guard let id = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber else { return nil }
    return (CGDirectDisplayID(id.uint32Value), screen)
}
let mainDisplayID = NSScreen.main.flatMap {
    ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber).map { CGDirectDisplayID($0.uint32Value) }
}
guard let selectedDisplayID = selectTargetDisplayID(
    available: availableScreens.map(\.id), requested: targetDisplayID, defaultID: mainDisplayID
), let targetScreen = availableScreens.first(where: { $0.id == selectedDisplayID })?.screen else {
    if let targetDisplayID {
        fputs("requested display-id \(targetDisplayID) is unavailable; refusing fallback\n", stderr)
    } else {
        fputs("no AppKit screen available\n", stderr)
    }
    exit(1)
}
let plannedScale = targetScreen.backingScaleFactor
let targetStyleMask = targetStyle.styleMask
guard let plannedGeometry = deriveWindowGeometry(
    physicalWidth: width,
    physicalHeight: height,
    backingScale: plannedScale,
    styleMask: targetStyleMask
) else {
    fputs("could not derive target window geometry\n", stderr)
    exit(1)
}
let frameOrigin = NSPoint(x: targetScreen.frame.minX + originX, y: targetScreen.frame.minY + originY)
var desiredFrame = NSRect(origin: frameOrigin, size: plannedGeometry.frameSizePoints)

let window = NSWindow(
    contentRect: NSRect(origin: frameOrigin, size: plannedGeometry.contentSizePoints),
    styleMask: targetStyleMask,
    backing: .buffered,
    defer: false
)
window.title = presentationPattern ? "Petal presentation latency source" : "Petal latency target"
window.setFrame(desiredFrame, display: false)
window.orderFrontRegardless()

// Window placement determines its actual screen/backing scale. Re-derive once
// from that observed scale so mixed-density display setups cannot silently
// produce a different physical raster than the preregistered source.
let actualScale = window.backingScaleFactor
if abs(actualScale - plannedScale) > 0.001 {
    guard let actualGeometry = deriveWindowGeometry(
        physicalWidth: width,
        physicalHeight: height,
        backingScale: actualScale,
        styleMask: targetStyleMask
    ) else {
        fputs("could not re-derive target geometry for actual backing scale\n", stderr)
        exit(1)
    }
    desiredFrame = NSRect(origin: frameOrigin, size: actualGeometry.frameSizePoints)
    window.setFrame(desiredFrame, display: false)
}
let raster = physicalRaster(for: window.frame.size, backingScale: actualScale)
guard raster == (width, height) else {
    fputs("physical raster mismatch before capture: expected \(width)x\(height), derived \(raster.0)x\(raster.1)\n", stderr)
    exit(1)
}
let contentSize = window.contentRect(forFrameRect: window.frame).size
window.contentView = ContentView(
    frame: NSRect(origin: .zero, size: contentSize),
    presentationPattern: presentationPattern
)

print(
    "TARGET_RASTER_VERIFIED \(raster.0)x\(raster.1) "
        + "scale=\(actualScale) frame_points=\(window.frame.width)x\(window.frame.height) "
        + "content_points=\(contentSize.width)x\(contentSize.height)"
)
print("TARGET_STYLE \(targetStyle.rawValue) nonactivating=true")

print("WINDOW_ID \(window.windowNumber)")
if presentationPattern {
    let screenFrame = targetScreen.frame
    let cropX = Int(((window.frame.minX - screenFrame.minX) * actualScale).rounded())
    let cropY = Int(((screenFrame.maxY - window.frame.maxY) * actualScale).rounded())
    print("SOURCE_CROP_PX \(cropX) \(cropY) \(raster.0) \(raster.1)")
    print("PRESENTATION_SOURCE_READY nonactivating=true key=\(window.isKeyWindow) main=\(window.isMainWindow)")
}
fflush(stdout)

let view = window.contentView as! ContentView
Timer.scheduledTimer(withTimeInterval: 1.0 / fps, repeats: true) { _ in
    view.frameIndex &+= 1
    view.needsDisplay = true
}
Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { _ in exit(0) }

app.run()
