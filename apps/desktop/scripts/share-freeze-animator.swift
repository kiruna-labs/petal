// Animated share source for the share-freeze soak (#391).
//
// A borderless 960x600 window that repaints every 33 ms, so a presented-frame
// gap on a viewer is a real freeze, never a static screen. Each repaint shows
// the tick number as a Gray code in 16 large blocks, bracketed by a white and
// a black marker block (row y=260..340). A viewer decodes the tick from any
// presented frame, which proves whether the source moved during a gap.
//
// The fixture logs its own repaint times ("<tick> <epochMs>" per line) to
// PETAL_FREEZE_ANIMATOR_LOG, so the analysis can tell a stalled source (the VM
// stalled this process) apart from a Petal/LiveKit freeze.
import AppKit
import Foundation

private let width: CGFloat = 960
private let height: CGFloat = 600
private let tickInterval: TimeInterval = 1.0 / 30.0
private let grayBits = 16
// Keep in lockstep with share-freeze-soak.mjs's STRIP_* constants.
private let stripX: CGFloat = 84
private let stripY: CGFloat = 260
private let blockWidth: CGFloat = 44
private let blockHeight: CGFloat = 80
private let logPath = ProcessInfo.processInfo.environment["PETAL_FREEZE_ANIMATOR_LOG"]
    ?? "/tmp/petal-freeze-animator-ticks.log"

final class AnimatorView: NSView {
    var tick: UInt32 = 0
    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        // Hue cycles every tick, so every frame differs over the whole window
        // and no dirty-rect or static-frame path can treat it as unchanged.
        let hue = CGFloat(tick % 120) / 120.0
        NSColor(calibratedHue: hue, saturation: 0.55, brightness: 0.45, alpha: 1).setFill()
        bounds.fill()

        // A moving bar for genuine motion, away from the code strip.
        NSColor(calibratedWhite: 0.9, alpha: 1).setFill()
        let barX = CGFloat((tick &* 12) % UInt32(width))
        NSRect(x: barX, y: 420, width: 60, height: 120).fill()

        NSColor.white.setFill()
        NSRect(x: stripX, y: stripY, width: blockWidth, height: blockHeight).fill()
        let gray = tick ^ (tick >> 1)
        for index in 0..<grayBits {
            let lit = ((gray >> UInt32(grayBits - 1 - index)) & 1) == 1
            (lit ? NSColor.white : NSColor.black).setFill()
            NSRect(
                x: stripX + CGFloat(index + 1) * blockWidth, y: stripY,
                width: blockWidth, height: blockHeight
            ).fill()
        }
        NSColor.black.setFill()
        NSRect(
            x: stripX + CGFloat(grayBits + 1) * blockWidth, y: stripY,
            width: blockWidth, height: blockHeight
        ).fill()

        let attributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedSystemFont(ofSize: 40, weight: .bold),
            .foregroundColor: NSColor.white,
        ]
        NSString(string: "PETAL FREEZE SOAK  tick \(tick)").draw(
            at: NSPoint(x: 84, y: 120), withAttributes: attributes
        )
    }
}

// AppKit constrains a window's frame to the screen; the strip geometry the
// viewers decode assumes exactly 960x600, so opt out.
final class AnimatorWindow: NSWindow {
    override func constrainFrameRect(_ frameRect: NSRect, to screen: NSScreen?) -> NSRect { frameRect }
}

final class AnimatorDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow!
    var activity: NSObjectProtocol?
    let view = AnimatorView(frame: NSRect(x: 0, y: 0, width: width, height: height))
    var log: FileHandle?
    var pending = ""
    var timer: Timer?

    func applicationDidFinishLaunching(_ notification: Notification) {
        FileManager.default.createFile(atPath: logPath, contents: nil)
        log = FileHandle(forWritingAtPath: logPath)

        // App Nap throttles a background app's timers to a few Hz (measured:
        // 13 ticks/s, 167 ms gaps), which would make the source itself freeze.
        activity = ProcessInfo.processInfo.beginActivity(
            options: [.userInitiated, .latencyCritical, .idleDisplaySleepDisabled],
            reason: "share-freeze soak source must repaint every 33 ms"
        )
        window = AnimatorWindow(
            contentRect: NSRect(x: 80, y: 120, width: width, height: height),
            styleMask: [.borderless], backing: .buffered, defer: false
        )
        window.title = "Petal Freeze Soak Animator"
        window.contentView = view
        window.isOpaque = true
        window.hasShadow = false
        // Petal only offers normal-layer windows as share targets.
        window.level = .normal
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        window.orderFrontRegardless()

        let timer = Timer(timeInterval: tickInterval, repeats: true) { [weak self] _ in
            self?.advance()
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
        let flush = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in self?.flush() }
        RunLoop.main.add(flush, forMode: .common)

        let pid = ProcessInfo.processInfo.processIdentifier
        FileHandle.standardOutput.write(Data("PETAL_FREEZE_ANIMATOR_READY pid=\(pid)\n".utf8))
    }

    func advance() {
        view.tick &+= 1
        view.needsDisplay = true
        view.displayIfNeeded()
        let ms = Int64((Date().timeIntervalSince1970 * 1000).rounded())
        pending += "\(view.tick) \(ms)\n"
    }

    func flush() {
        guard !pending.isEmpty else { return }
        log?.write(Data(pending.utf8))
        pending = ""
    }

    func applicationWillTerminate(_ notification: Notification) { flush() }
}

let app = NSApplication.shared
private let delegate = AnimatorDelegate()
// Regular, like the photon sentinel: Petal's picker lists normal app windows.
app.setActivationPolicy(.regular)
app.delegate = delegate
signal(SIGTERM, SIG_IGN)
private let termSource = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
termSource.setEventHandler { delegate.flush(); exit(0) }
termSource.resume()
app.run()
