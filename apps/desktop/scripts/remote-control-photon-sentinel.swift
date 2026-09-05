import AppKit
import Foundation

private let sentinelWidth: CGFloat = 960
private let sentinelHeight: CGFloat = 600
private let grayCodeBits = 16
private let eventLogPath = ProcessInfo.processInfo.environment["PETAL_RC_SENTINEL_EVENT_LOG"] ?? "/tmp/petal-rc-sentinel-events.jsonl"

// #618 positive control ONLY. Sleeps the sentinel's main thread AFTER the press
// has been timestamped, so the host's synchronous AX call stays blocked and the
// serialised replay shard is genuinely occupied for longer. Never set this in a
// normal run -- it manufactures the exact backlog the test looks for.
private let sentinelClickDelayMs = Double(
    ProcessInfo.processInfo.environment["PETAL_RC_SENTINEL_CLICK_DELAY_MS"] ?? ""
) ?? 0

// The AX-hostile actuation region (#446 acceptance). Custom-drawn, exposes no
// AX role/children/actions, so Petal's AX tier must miss it and fall through to
// the coordinate route. Kept clear of the AppKit button/text field so a hit
// here can never be serviced by the AX path.
private let hostileRect = NSRect(x: 60, y: 440, width: 840, height: 140)

// `--warp <x> <y>` reuses this same compiled binary as a one-shot cursor mover
// for #446's host-reclaims-the-mouse acceptance case. CGWarpMouseCursorPosition
// moves the real cursor WITHOUT synthesizing an event, which is what the
// host-presence policy actually reads (`CGEventCreate(NULL)` location), so it
// is a faithful stand-in for a human nudging the mouse mid-gesture.
private func runWarpModeIfRequested() {
    let args = CommandLine.arguments
    guard let flagIndex = args.firstIndex(of: "--warp"), args.count > flagIndex + 2,
          let x = Double(args[flagIndex + 1]), let y = Double(args[flagIndex + 2]) else { return }
    CGWarpMouseCursorPosition(CGPoint(x: x, y: y))
    CGAssociateMouseAndMouseCursorPosition(1)
    let current = CGEvent(source: nil)?.location ?? .zero
    FileHandle.standardOutput.write(Data("PETAL_RC_WARPED x=\(current.x) y=\(current.y)\n".utf8))
    exit(0)
}

/// `--click <x> <y>` posts a left click through EXACTLY the route Petal's
/// coordinate tier uses (`CGEventPost(.cgSessionEventTap)` at a global,
/// top-left-origin point). This is the harness's positive control: if this
/// lands in the sentinel's ledger, the target is raised, hit-testable and
/// observable, so a zero from Petal is a real zero. If it does NOT land, the
/// harness is broken and every measurement in the run is uninterpretable.
private func runClickModeIfRequested() {
    let args = CommandLine.arguments
    guard let flagIndex = args.firstIndex(of: "--click"), args.count > flagIndex + 2,
          let x = Double(args[flagIndex + 1]), let y = Double(args[flagIndex + 2]) else { return }
    let point = CGPoint(x: x, y: y)
    for (type, label) in [(CGEventType.mouseMoved, "move"), (.leftMouseDown, "down"), (.leftMouseUp, "up")] {
        guard let event = CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: point, mouseButton: .left) else {
            FileHandle.standardError.write(Data("PETAL_RC_CLICK_FAILED \(label)\n".utf8))
            exit(1)
        }
        // Load-bearing: AppKit drops a synthesized mouse-down whose click state
        // is 0, so a control that omits this reads as "nothing was delivered"
        // and would invalidate the whole run. Petal's own tier sets it too.
        event.setIntegerValueField(.mouseEventClickState, value: 1)
        event.setIntegerValueField(.mouseEventButtonNumber, value: 0)
        event.post(tap: .cgSessionEventTap)
        Thread.sleep(forTimeInterval: 0.03)
    }
    FileHandle.standardOutput.write(Data("PETAL_RC_CLICKED x=\(x) y=\(y)\n".utf8))
    exit(0)
}

/// `--occluder <x> <y> <w> <h>` (top-left origin) opens an opaque floating
/// window over the given screen rect and parks. It exists to answer one
/// question on #446: does the coordinate tier report `outcome=Handled` when its
/// event cannot actually reach the target? A `.floating`-level window is above
/// anything `AXRaise` can lift a normal window to, so the tier's raise
/// "succeeds" and its post "succeeds" while delivery provably does not happen.
private final class OccluderDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow!
    private let rect: NSRect

    init(rect: NSRect) {
        self.rect = rect
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = NSWindow(contentRect: rect, styleMask: [.borderless], backing: .buffered, defer: false)
        window.backgroundColor = NSColor(calibratedRed: 0.9, green: 0.1, blue: 0.1, alpha: 1)
        window.isOpaque = true
        window.hasShadow = false
        window.level = .floating
        window.ignoresMouseEvents = false
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        window.orderFrontRegardless()
        FileHandle.standardOutput.write(Data("PETAL_RC_OCCLUDER_READY pid=\(ProcessInfo.processInfo.processIdentifier)\n".utf8))
    }
}

private func runOccluderModeIfRequested() {
    let args = CommandLine.arguments
    guard let flagIndex = args.firstIndex(of: "--occluder"), args.count > flagIndex + 4,
          let x = Double(args[flagIndex + 1]), let y = Double(args[flagIndex + 2]),
          let w = Double(args[flagIndex + 3]), let h = Double(args[flagIndex + 4]) else { return }
    let screenHeight = (NSScreen.main?.frame.height) ?? 0
    // Incoming rect is top-left origin; NSWindow wants bottom-left.
    let rect = NSRect(x: x, y: screenHeight - y - h, width: w, height: h)
    let app = NSApplication.shared
    let delegate = OccluderDelegate(rect: rect)
    app.setActivationPolicy(.accessory)
    app.delegate = delegate
    app.run()
    exit(0)
}

runWarpModeIfRequested()
runClickModeIfRequested()
runOccluderModeIfRequested()

private func appendEvent(_ value: [String: Any]) {
    guard JSONSerialization.isValidJSONObject(value), let data = try? JSONSerialization.data(withJSONObject: value), let line = String(data: data, encoding: .utf8) else { return }
    if !FileManager.default.fileExists(atPath: eventLogPath) { FileManager.default.createFile(atPath: eventLogPath, contents: nil) }
    guard let handle = try? FileHandle(forWritingTo: URL(fileURLWithPath: eventLogPath)) else { return }
    defer { try? handle.close() }
    handle.seekToEndOfFile()
    handle.write(Data((line + "\n").utf8))
}

private func eventTypeName(_ type: NSEvent.EventType) -> String {
    switch type {
    case .leftMouseDown: return "leftMouseDown"
    case .leftMouseUp: return "leftMouseUp"
    case .rightMouseDown: return "rightMouseDown"
    case .rightMouseUp: return "rightMouseUp"
    case .otherMouseDown: return "otherMouseDown"
    case .otherMouseUp: return "otherMouseUp"
    case .mouseMoved: return "mouseMoved"
    case .leftMouseDragged: return "leftMouseDragged"
    case .rightMouseDragged: return "rightMouseDragged"
    case .otherMouseDragged: return "otherMouseDragged"
    case .scrollWheel: return "scrollWheel"
    case .keyDown: return "keyDown"
    case .keyUp: return "keyUp"
    default: return "type_\(type.rawValue)"
    }
}

private struct CalibrationSquare {
    let rect: NSRect
    let color: NSColor
}

private let calibrationSquares = [
    CalibrationSquare(rect: NSRect(x: 16, y: 16, width: 24, height: 24), color: NSColor(calibratedRed: 1, green: 45 / 255, blue: 85 / 255, alpha: 1)),
    CalibrationSquare(rect: NSRect(x: 920, y: 16, width: 24, height: 24), color: NSColor(calibratedRed: 0, green: 1, blue: 136 / 255, alpha: 1)),
    CalibrationSquare(rect: NSRect(x: 16, y: 560, width: 24, height: 24), color: NSColor(calibratedRed: 45 / 255, green: 125 / 255, blue: 1, alpha: 1)),
    CalibrationSquare(rect: NSRect(x: 920, y: 560, width: 24, height: 24), color: NSColor(calibratedRed: 1, green: 212 / 255, blue: 0, alpha: 1)),
]

private final class SentinelPatternView: NSView {
    private(set) var generation: UInt16 = 0
    private(set) var lastInput = "ready"

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }

    func recordInput(_ input: String) {
        generation &+= 1
        lastInput = input
        needsDisplay = true
        displayIfNeeded()
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor(calibratedRed: 27 / 255, green: 16 / 255, blue: 51 / 255, alpha: 1).setFill()
        bounds.fill()

        for square in calibrationSquares {
            square.color.setFill()
            square.rect.fill()
        }

        let value = Int(generation)
        let gray = value ^ (value >> 1)
        for index in 0..<grayCodeBits {
            let shift = grayCodeBits - 1 - index
            let lit = ((gray >> shift) & 1) == 1
            (lit ? NSColor.white : NSColor.black).setFill()
            NSRect(x: 160 + CGFloat(index * 40), y: 88, width: 40, height: 30).fill()
        }

        let centered = NSMutableParagraphStyle()
        centered.alignment = .center
        let titleAttributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: 30, weight: .bold),
            .foregroundColor: NSColor.white,
            .paragraphStyle: centered,
        ]
        let statusAttributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedSystemFont(ofSize: 24, weight: .bold),
            .foregroundColor: NSColor(calibratedRed: 0, green: 1, blue: 136 / 255, alpha: 1),
            .paragraphStyle: centered,
        ]
        NSString(string: "PETAL REMOTE-CONTROL PHOTON SENTINEL").draw(
            in: NSRect(x: 80, y: 45, width: 800, height: 36),
            withAttributes: titleAttributes
        )
        NSString(string: "generation \(generation) · \(lastInput)").draw(
            in: NSRect(x: 160, y: 135, width: 640, height: 32),
            withAttributes: statusAttributes
        )

        NSColor(calibratedWhite: 1, alpha: 0.12).setFill()
        NSBezierPath(roundedRect: NSRect(x: 60, y: 215, width: 420, height: 270), xRadius: 22, yRadius: 22).fill()
        NSBezierPath(roundedRect: NSRect(x: 520, y: 215, width: 380, height: 270), xRadius: 22, yRadius: 22).fill()
    }
}

/// #446 acceptance surface: custom-drawn content with NO accessibility
/// affordance of any kind, so Petal's AX tier cannot service a hit here and
/// must fall through to the coordinate route under test. Every mouse event it
/// receives is appended to the same JSONL ledger as `kind: "hostile"`, which is
/// the app's own observable behaviour -- not a "packet sent, no error" claim.
private final class HostileCanvasView: NSView {
    private(set) var downs = 0
    private(set) var drags = 0
    private(set) var ups = 0
    private(set) var scrolls = 0
    var onActuation: (() -> Void)?

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    // Deliberately invisible to accessibility: this is the whole point of the
    // surface. If any of these ever start reporting an actionable element, the
    // #446 acceptance run silently degrades into an AX-path test that proves
    // nothing about the coordinate route.
    override func isAccessibilityElement() -> Bool { false }
    override func accessibilityRole() -> NSAccessibility.Role? { .unknown }
    override func accessibilityChildren() -> [Any]? { nil }
    override func accessibilityActionNames() -> [NSAccessibility.Action] { [] }
    override func accessibilityLabel() -> String? { nil }

    private func record(_ action: String, _ event: NSEvent) {
        let local = convert(event.locationInWindow, from: nil)
        let global = NSEvent.mouseLocation
        appendEvent([
            "kind": "hostile",
            "tMs": Int(Date().timeIntervalSince1970 * 1000),
            "action": action,
            "button": event.buttonNumber,
            "localX": Double(local.x),
            "localY": Double(local.y),
            "globalX": Double(global.x),
            "globalY": Double(global.y),
            "scrollingDeltaX": event.type == .scrollWheel ? Double(event.scrollingDeltaX) : 0,
            "scrollingDeltaY": event.type == .scrollWheel ? Double(event.scrollingDeltaY) : 0,
            "counts": ["down": downs, "drag": drags, "up": ups, "scroll": scrolls],
        ])
        onActuation?()
    }

    override func mouseDown(with event: NSEvent) { downs += 1; record("down", event) }
    override func mouseDragged(with event: NSEvent) { drags += 1; record("drag", event) }
    override func mouseUp(with event: NSEvent) { ups += 1; record("up", event) }
    override func rightMouseDown(with event: NSEvent) { downs += 1; record("rightDown", event) }
    override func rightMouseDragged(with event: NSEvent) { drags += 1; record("rightDrag", event) }
    override func rightMouseUp(with event: NSEvent) { ups += 1; record("rightUp", event) }
    override func otherMouseDown(with event: NSEvent) { downs += 1; record("otherDown", event) }
    override func otherMouseDragged(with event: NSEvent) { drags += 1; record("otherDrag", event) }
    override func otherMouseUp(with event: NSEvent) { ups += 1; record("otherUp", event) }
    override func scrollWheel(with event: NSEvent) { scrolls += 1; record("scroll", event) }

    override func draw(_ dirtyRect: NSRect) {
        NSColor(calibratedRed: 0.10, green: 0.06, blue: 0.20, alpha: 1).setFill()
        bounds.fill()
        // Hand-drawn content only: no NSButton, no NSTextField, nothing the AX
        // tier could latch onto.
        NSColor(calibratedRed: 1, green: 212 / 255, blue: 0, alpha: 1).setStroke()
        let path = NSBezierPath(roundedRect: bounds.insetBy(dx: 6, dy: 6), xRadius: 14, yRadius: 14)
        path.lineWidth = 4
        path.stroke()
        let centered = NSMutableParagraphStyle()
        centered.alignment = .center
        NSString(string: "AX-HOSTILE CANVAS  down \(downs) · drag \(drags) · up \(ups) · scroll \(scrolls)").draw(
            in: NSRect(x: 20, y: bounds.midY - 16, width: bounds.width - 40, height: 32),
            withAttributes: [
                .font: NSFont.monospacedSystemFont(ofSize: 22, weight: .bold),
                .foregroundColor: NSColor.white,
                .paragraphStyle: centered,
            ]
        )
    }
}

/// Samples the real OS cursor so the harness can assert both "the AX path moved
/// no cursor" and "the host cursor was put back after a coordinate gesture".
/// Uses the same reading Petal's own host-presence policy uses
/// (`CGEventCreate(NULL)` location, top-left origin), so the two agree by
/// construction.
private func startCursorSampler() {
    let thread = Thread {
        var last = CGPoint(x: CGFloat.nan, y: CGFloat.nan)
        var lastEmit = Date.distantPast
        while true {
            let point = CGEvent(source: nil)?.location ?? .zero
            let moved = !(abs(point.x - last.x) < 0.5 && abs(point.y - last.y) < 0.5)
            let stale = Date().timeIntervalSince(lastEmit) > 0.25
            if moved || stale {
                appendEvent([
                    "kind": "cursor",
                    "tMs": Int(Date().timeIntervalSince1970 * 1000),
                    "x": Double(point.x),
                    "y": Double(point.y),
                ])
                last = point
                lastEmit = Date()
            }
            Thread.sleep(forTimeInterval: 0.008)
        }
    }
    thread.name = "petal-rc-sentinel-cursor"
    thread.start()
}

private final class SentinelWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }

    override func sendEvent(_ event: NSEvent) {
        // CONFIRMED LIVE 2026-07-27, and the reason this ledger recorded ZERO
        // mouse events for as long as it has existed: several NSEvent
        // properties are only defined for particular event types and raise an
        // ObjC exception otherwise -- `scrollingDeltaX/Y` and `phase` on a
        // plain mouse event, `buttonNumber`/`clickCount` on a key event. The
        // old unconditional dictionary raised while it was still being built,
        // AppKit's event loop swallowed the exception, and `super.sendEvent`
        // was never reached -- so the event was not merely unlogged, it was
        // never delivered to any view at all. Read each field only for the
        // types that define it. #446.
        // Buttonless hover is a high-rate stream and the cursor sampler already
        // covers pointer position; logging it here would bury the ledger.
        if event.type == .mouseMoved {
            super.sendEvent(event)
            return
        }
        var payload: [String: Any] = [
            "kind": "event",
            "tMs": Int(Date().timeIntervalSince1970 * 1000),
            "type": eventTypeName(event.type),
            "typeRaw": event.type.rawValue,
            "modifierFlags": event.modifierFlags.rawValue,
        ]
        switch event.type {
        case .leftMouseDown, .leftMouseUp, .leftMouseDragged,
             .rightMouseDown, .rightMouseUp, .rightMouseDragged,
             .otherMouseDown, .otherMouseUp, .otherMouseDragged:
            payload["button"] = event.buttonNumber
            payload["clickCount"] = event.clickCount
        case .scrollWheel:
            payload["button"] = event.buttonNumber
            payload["scrollingDeltaX"] = Double(event.scrollingDeltaX)
            payload["scrollingDeltaY"] = Double(event.scrollingDeltaY)
            payload["phase"] = event.phase.rawValue
        default:
            break
        }
        appendEvent(payload)
        super.sendEvent(event)
    }
}

/// #811: the horizontal-scroll sentinel's document view. Wide enough that the
/// strip always has somewhere to scroll to, with tick marks so a human can see
/// the position move in a recording.
private final class HScrollTickView: NSView {
    override var isFlipped: Bool { true }
    override func draw(_ dirtyRect: NSRect) {
        NSColor(calibratedWhite: 0.12, alpha: 1).setFill()
        dirtyRect.fill()
        NSColor.systemTeal.setFill()
        var x: CGFloat = 0
        while x < bounds.width {
            NSRect(x: x, y: 8, width: 4, height: bounds.height - 16).fill()
            x += 80
        }
    }
}

private final class SentinelAppDelegate: NSObject, NSApplicationDelegate, NSTextFieldDelegate {
    private var window: NSWindow!
    private var patternView: SentinelPatternView!
    private var textField: NSTextField!
    fileprivate var hostileCanvas: HostileCanvasView!
    private var scrollStrip: NSScrollView!
    private var lastHScrollOriginX: CGFloat = 0

    func applicationDidFinishLaunching(_ notification: Notification) {
        patternView = SentinelPatternView(frame: NSRect(x: 0, y: 0, width: sentinelWidth, height: sentinelHeight))

        textField = NSTextField(frame: NSRect(x: 90, y: 285, width: 360, height: 72))
        textField.font = NSFont.monospacedSystemFont(ofSize: 28, weight: .medium)
        textField.placeholderString = "Remote text lands here"
        textField.stringValue = ""
        textField.delegate = self
        textField.setAccessibilityLabel("Remote text input sentinel")

        let textLabel = NSTextField(labelWithString: "TEXT INPUT")
        textLabel.frame = NSRect(x: 90, y: 245, width: 360, height: 28)
        textLabel.font = NSFont.systemFont(ofSize: 18, weight: .semibold)
        textLabel.textColor = .white
        textLabel.alignment = .center

        let clickButton = NSButton(frame: NSRect(x: 560, y: 270, width: 300, height: 150))
        clickButton.title = "REMOTE CLICK"
        clickButton.font = NSFont.systemFont(ofSize: 26, weight: .bold)
        clickButton.bezelStyle = .rounded
        clickButton.target = self
        clickButton.action = #selector(remoteClick)
        clickButton.setAccessibilityLabel("Remote click sentinel")

        hostileCanvas = HostileCanvasView(frame: hostileRect)
        hostileCanvas.onActuation = { [weak self] in
            self?.patternView.recordInput("hostile")
            self?.hostileCanvas.needsDisplay = true
        }

        // #811: horizontal scroll lands as an AX scrollbar-value write
        // (AXValue on AXHorizontalScrollBar), never as a scrollWheel NSEvent --
        // SkyLight/CGEvent wheel posting delivers 0 NSEvents on this platform
        // (docs/TESTING.md, "#446 ... measured ineffective"). The observable
        // host effect is therefore the scroll POSITION: this strip ledgers
        // every horizontal origin change as a `type: "hscroll"` event.
        scrollStrip = NSScrollView(frame: NSRect(x: 60, y: 60, width: 840, height: 120))
        scrollStrip.hasHorizontalScroller = true
        scrollStrip.hasVerticalScroller = false
        // Legacy (always-visible) scrollers, so AXHorizontalScrollBar is
        // reliably exposed to the replay path's scroll-target resolution.
        scrollStrip.scrollerStyle = .legacy
        scrollStrip.autohidesScrollers = false
        scrollStrip.documentView = HScrollTickView(frame: NSRect(x: 0, y: 0, width: 4000, height: 96))
        scrollStrip.contentView.postsBoundsChangedNotifications = true
        scrollStrip.setAccessibilityLabel("Horizontal scroll sentinel")
        NotificationCenter.default.addObserver(
            forName: NSView.boundsDidChangeNotification,
            object: scrollStrip.contentView,
            queue: .main
        ) { [weak self] _ in
            guard let self else { return }
            let originX = self.scrollStrip.contentView.bounds.origin.x
            let delta = originX - self.lastHScrollOriginX
            if abs(delta) < 0.5 { return }
            self.lastHScrollOriginX = originX
            appendEvent([
                "type": "hscroll",
                "originX": Double(originX),
                "deltaX": Double(delta),
                "timestamp": Date().timeIntervalSince1970,
            ])
            self.patternView.recordInput("hscroll")
        }

        patternView.addSubview(textLabel)
        patternView.addSubview(textField)
        patternView.addSubview(clickButton)
        patternView.addSubview(hostileCanvas)
        patternView.addSubview(scrollStrip)

        window = SentinelWindow(
            contentRect: NSRect(x: 120, y: 120, width: sentinelWidth, height: sentinelHeight),
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.title = "Petal RC Photon Sentinel"
        window.contentView = patternView
        window.backgroundColor = NSColor(calibratedRed: 27 / 255, green: 16 / 255, blue: 51 / 255, alpha: 1)
        window.isOpaque = true
        window.hasShadow = false
        // Petal intentionally filters ScreenCaptureKit targets to the normal
        // window layer, matching the windows users can normally select.
        window.level = .normal
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        window.makeFirstResponder(textField)

        // Screen geometry, so a harness can convert the AX-hostile canvas rect
        // into the global (top-left origin) coordinates Petal injects at, and
        // pick a "host reclaims the mouse" warp point far outside the window.
        let screenFrame = (window.screen ?? NSScreen.main)?.frame ?? .zero
        // `convert(_:to: nil)` (not `patternView.convert(from:)`) is load-bearing:
        // the pattern view is FLIPPED, so its own coordinates are not the
        // window's base coordinates and converting through it silently mirrors
        // the rect vertically.
        let canvasInWindow = hostileCanvas.convert(hostileCanvas.bounds, to: nil)
        let canvasOnScreen = window.convertToScreen(canvasInWindow)
        // Global, TOP-LEFT origin -- the coordinate space CGEvent/Petal inject in.
        let canvasCenterTopLeft = CGPoint(
            x: canvasOnScreen.midX,
            y: screenFrame.height - canvasOnScreen.midY
        )
        appendEvent([
            "kind": "geometry",
            "tMs": Int(Date().timeIntervalSince1970 * 1000),
            "windowFrame": ["x": window.frame.origin.x, "y": window.frame.origin.y, "w": window.frame.width, "h": window.frame.height],
            "hostileOnScreenBottomLeft": ["x": canvasOnScreen.origin.x, "y": canvasOnScreen.origin.y, "w": canvasOnScreen.width, "h": canvasOnScreen.height],
            "screenFrame": ["x": screenFrame.origin.x, "y": screenFrame.origin.y, "w": screenFrame.width, "h": screenFrame.height],
            "contentSize": ["w": sentinelWidth, "h": sentinelHeight],
            "hostileInContent": ["x": hostileRect.origin.x, "y": hostileRect.origin.y, "w": hostileRect.width, "h": hostileRect.height],
            "hostileCenterTopLeft": ["x": canvasCenterTopLeft.x, "y": canvasCenterTopLeft.y],
        ])
        startCursorSampler()

        // Two independent observers of the same stream, so a zero in the
        // ledger can be attributed. `local` sees what AppKit dispatched to
        // THIS app; `global` sees mouse events the session delivered to some
        // OTHER app. "global fired, local silent" means the event existed but
        // was routed elsewhere (occlusion / wrong window); "both silent" means
        // it was never delivered at all.
        let mouseMask: NSEvent.EventTypeMask = [
            .leftMouseDown, .leftMouseUp, .leftMouseDragged,
            .rightMouseDown, .rightMouseUp, .rightMouseDragged,
            .otherMouseDown, .otherMouseUp, .otherMouseDragged,
            .scrollWheel,
        ]
        NSEvent.addGlobalMonitorForEvents(matching: mouseMask) { event in
            appendEvent([
                "kind": "monitor", "scope": "global", "tMs": Int(Date().timeIntervalSince1970 * 1000),
                "type": eventTypeName(event.type), "x": Double(NSEvent.mouseLocation.x), "y": Double(NSEvent.mouseLocation.y),
            ])
        }
        NSEvent.addLocalMonitorForEvents(matching: mouseMask) { event in
            appendEvent([
                "kind": "monitor", "scope": "local", "tMs": Int(Date().timeIntervalSince1970 * 1000),
                "type": eventTypeName(event.type), "x": Double(NSEvent.mouseLocation.x), "y": Double(NSEvent.mouseLocation.y),
                "eventWindowNumber": event.windowNumber, "sentinelWindowNumber": self.window?.windowNumber ?? -1,
                "hasWindow": event.window != nil,
            ])
            return event
        }

        let ready = "PETAL_RC_PHOTON_SENTINEL_READY pid=\(ProcessInfo.processInfo.processIdentifier) log=\(eventLogPath)\n"
        FileHandle.standardOutput.write(Data(ready.utf8))
    }

    @objc private func remoteClick() {
        // tMs is load-bearing for #618's queueing test: it is the per-event
        // host-side landing timestamp, the only signal that resolves every
        // event at cadences above the capture frame rate.
        appendEvent([
            "kind": "axAction", "action": "press", "element": "Remote click sentinel",
            "tMs": Date().timeIntervalSince1970 * 1000,
        ])
        patternView.recordInput("click")
        window.makeFirstResponder(textField)
        if sentinelClickDelayMs > 0 {
            Thread.sleep(forTimeInterval: sentinelClickDelayMs / 1000)
        }
    }

    func controlTextDidChange(_ notification: Notification) {
        appendEvent(["kind": "axAction", "action": "setValue", "element": "Remote text input sentinel"])
        patternView.recordInput("text")
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

let app = NSApplication.shared
private let delegate = SentinelAppDelegate()
app.setActivationPolicy(.regular)
app.delegate = delegate
app.run()
