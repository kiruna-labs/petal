import AppKit
import CoreGraphics
import Foundation

struct Options {
    var countdown = 15
    var referencePath: String?
    var preflightOnly = false
}

private func options() -> Options {
    var result = Options()
    var index = 1
    while index < CommandLine.arguments.count {
        switch CommandLine.arguments[index] {
        case "--preflight-only":
            result.preflightOnly = true
        case "--countdown":
            index += 1
            guard index < CommandLine.arguments.count,
                  let value = Int(CommandLine.arguments[index]),
                  (1...120).contains(value) else {
                fputs("--countdown requires an integer from 1 through 120\n", stderr)
                exit(2)
            }
            result.countdown = value
        case "--reference":
            index += 1
            guard index < CommandLine.arguments.count else {
                fputs("--reference requires a PNG path\n", stderr)
                exit(2)
            }
            result.referencePath = CommandLine.arguments[index]
        default:
            fputs("unknown argument: \(CommandLine.arguments[index])\n", stderr)
            exit(2)
        }
        index += 1
    }
    return result
}

private func displayEvidence() -> [String: Any] {
    let displayID = CGMainDisplayID()
    guard let mode = CGDisplayCopyDisplayMode(displayID) else {
        return ["ok": false, "error": "CGDisplayCopyDisplayMode returned nil"]
    }
    let logicalWidth = mode.width
    let logicalHeight = mode.height
    let pixelWidth = mode.pixelWidth
    let pixelHeight = mode.pixelHeight
    let scaleX = logicalWidth > 0 ? Double(pixelWidth) / Double(logicalWidth) : 0
    let scaleY = logicalHeight > 0 ? Double(pixelHeight) / Double(logicalHeight) : 0
    return [
        "ok": logicalWidth > 0 && logicalHeight > 0 && pixelWidth > 0 && pixelHeight > 0,
        "displayId": displayID,
        "logicalWidth": logicalWidth,
        "logicalHeight": logicalHeight,
        "pixelWidth": pixelWidth,
        "pixelHeight": pixelHeight,
        "scaleX": scaleX,
        "scaleY": scaleY,
        "backingScaleFactor": NSScreen.main?.backingScaleFactor ?? 0,
        "refreshRate": mode.refreshRate,
    ]
}

private func printJSON(_ value: [String: Any], prefix: String) {
    let data = try! JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    print("\(prefix)\(String(decoding: data, as: UTF8.self))")
    fflush(stdout)
}

let configured = options()
if configured.preflightOnly {
    printJSON(displayEvidence(), prefix: "PETAL_FIDELITY_DISPLAY=")
    exit(0)
}

final class FidelityView: NSView {
    var remaining: Int
    var locked = false

    init(frame: NSRect, countdown: Int) {
        remaining = countdown
        super.init(frame: frame)
    }

    required init?(coder: NSCoder) { nil }
    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.white.setFill()
        bounds.fill()

        let cell: CGFloat = 4
        for row in 0..<24 {
            for column in 0..<80 where (row + column).isMultiple(of: 2) {
                NSColor.black.setFill()
                NSRect(x: CGFloat(column) * cell, y: CGFloat(row) * cell,
                       width: cell, height: cell).fill()
            }
        }

        for x in stride(from: 24, through: 936, by: 24) {
            NSColor.black.setFill()
            NSRect(x: x, y: 112, width: 1, height: 116).fill()
        }
        [NSColor.systemRed, .systemGreen, .systemBlue].enumerated().forEach { index, color in
            color.setFill()
            NSRect(x: 48 + CGFloat(index) * 288, y: 246, width: 240, height: 72).fill()
        }

        let centered = NSMutableParagraphStyle()
        centered.alignment = .center
        let heading: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedSystemFont(ofSize: 13, weight: .regular),
            .foregroundColor: NSColor.black,
            .paragraphStyle: centered,
        ]
        NSString(string: "ABCDEFGHIJKLMNOPQRSTUVWXYZ  abcdefghijklmnopqrstuvwxyz\n0123456789  !@#$%^&*()_+-=[]{};':\",.<>/?")
            .draw(in: NSRect(x: 32, y: 346, width: 896, height: 48), withAttributes: heading)

        let status: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: 28, weight: .bold),
            .foregroundColor: locked ? NSColor.systemGreen : NSColor.systemOrange,
            .paragraphStyle: centered,
        ]
        let text = locked
            ? "CAPTURE LOCKED — SAFE TO SWITCH AWAY"
            : "KEEP THIS WINDOW VISIBLE · \(remaining)s"
        NSString(string: text).draw(in: NSRect(x: 24, y: 452, width: 912, height: 46),
                                    withAttributes: status)
    }

    func saveReference(to path: String) throws -> (Int, Int) {
        guard let bitmap = bitmapImageRepForCachingDisplay(in: bounds) else {
            throw NSError(domain: "PetalFidelity", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "could not allocate bitmap"])
        }
        cacheDisplay(in: bounds, to: bitmap)
        guard let png = bitmap.representation(using: .png, properties: [:]) else {
            throw NSError(domain: "PetalFidelity", code: 2,
                          userInfo: [NSLocalizedDescriptionKey: "could not encode PNG"])
        }
        try png.write(to: URL(fileURLWithPath: path), options: .atomic)
        return (bitmap.pixelsWide, bitmap.pixelsHigh)
    }
}

final class FixtureDelegate: NSObject, NSApplicationDelegate {
    private let configured: Options
    private var window: NSWindow!
    private var view: FidelityView!
    private var timer: Timer!

    init(configured: Options) { self.configured = configured }

    func applicationDidFinishLaunching(_ notification: Notification) {
        view = FidelityView(frame: NSRect(x: 0, y: 0, width: 960, height: 600),
                            countdown: configured.countdown)
        window = NSWindow(contentRect: view.bounds, styleMask: [.titled, .closable],
                          backing: .buffered, defer: false)
        window.title = "Petal Window Fidelity Reference"
        window.contentView = view
        window.level = .floating
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        printJSON(displayEvidence().merging([
            "pid": ProcessInfo.processInfo.processIdentifier,
            "countdownSeconds": configured.countdown,
        ]) { _, new in new }, prefix: "PETAL_FIDELITY_READY=")

        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.view.remaining -= 1
            if self.view.remaining <= 0 {
                self.timer.invalidate()
                self.view.remaining = 0
                self.view.locked = true
                self.window.level = .normal
                self.view.needsDisplay = true
                self.view.displayIfNeeded()
                var result: [String: Any] = ["locked": true]
                if let path = self.configured.referencePath {
                    do {
                        let size = try self.view.saveReference(to: path)
                        result["referencePath"] = path
                        result["referencePixelWidth"] = size.0
                        result["referencePixelHeight"] = size.1
                    } catch {
                        result["referenceError"] = error.localizedDescription
                    }
                }
                printJSON(result, prefix: "PETAL_FIDELITY_LOCKED=")
            }
            self.view.needsDisplay = true
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

let app = NSApplication.shared
let delegate = FixtureDelegate(configured: configured)
app.setActivationPolicy(.regular)
app.delegate = delegate
app.run()
