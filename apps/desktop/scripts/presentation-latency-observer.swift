// Same-display, presentation-inclusive latency observer for #613.
//
// Captures one physical display with ScreenCaptureKit, decodes calibrated
// 16-bit Gray counters from source and destination crops in memory, and emits
// timing/counter evidence only. No captured pixels are ever written to disk.

import CoreMedia
import CoreVideo
import CoreGraphics
import Foundation
import ScreenCaptureKit

private let patternWidth = 960
private let patternHeight = 600
private let grayBits = 16
private let generationMask = (1 << grayBits) - 1
private let invalidObserverDisplayUnavailable = "INVALID_OBSERVER_DISPLAY_UNAVAILABLE"

private struct PixelRect: Equatable {
    let x: Int
    let y: Int
    let width: Int
    let height: Int

    var maxX: Int { x + width }
    var maxY: Int { y + height }

    func contains(width bufferWidth: Int, height bufferHeight: Int) -> Bool {
        x >= 0 && y >= 0 && self.width > 0 && self.height > 0
            && maxX <= bufferWidth && maxY <= bufferHeight
    }

    func intersects(_ other: PixelRect) -> Bool {
        x < other.maxX && other.x < maxX && y < other.maxY && other.y < maxY
    }
}

private struct RGB {
    let red: Double
    let green: Double
    let blue: Double
}

private struct DecodedPattern {
    let generation: Int
    let confidence: Double
}

private struct PairedSample {
    let generation: UInt64
    let sourceMs: Double
    let destinationMs: Double
    var latencyMs: Double { destinationMs - sourceMs }
}

private struct Summary: Encodable {
    let samples: Int
    let averageMs: Double
    let p50Ms: Double
    let p95Ms: Double
    let sourceFps: Double
    let destinationFps: Double
    let unpairedDestinationGenerations: Int
    let frameStatusErrors: Int
    let decodeFailuresAfterReady: Int
    let counterRegressions: Int
}

private struct PointRect: Equatable {
    let x: Double
    let y: Double
    let width: Double
    let height: Double

    var cgRect: CGRect { CGRect(x: x, y: y, width: width, height: height) }
}

private struct DisplayDescriptor: Equatable {
    let id: CGDirectDisplayID
    let frame: PointRect
    let scale: Double
    let pixelWidth: Int
    let pixelHeight: Int
}

private struct DisplayCandidate: Equatable {
    let index: Int
    let id: CGDirectDisplayID
    let frame: PointRect
    let width: Double
    let height: Double
}

private enum DisplaySelection: String {
    case exactID = "exact-id"
    case structuralFallback = "structural-fallback"
}

private struct Options {
    var sourceRect: PixelRect?
    var destinationRect: PixelRect?
    var samples = 120
    var warmup = 30
    var timeoutSeconds = 30.0
    var outputPath: String?
    var sourceWindowID: CGWindowID?
    var destinationWindowID: CGWindowID?
    var displayID: CGDirectDisplayID?
    var displayCGX: Double?
    var displayCGY: Double?
    var displayWidthPoints: Double?
    var displayHeightPoints: Double?
    var displayScale: Double?
    var displayPixelWidth: Int?
    var displayPixelHeight: Int?
    var selfTest = false
}

private func parseRect(_ values: [String], flag: String) throws -> PixelRect {
    guard values.count == 4, let x = Int(values[0]), let y = Int(values[1]),
          let width = Int(values[2]), let height = Int(values[3]) else {
        throw NSError(domain: "PetalPresentationObserver", code: 2,
                      userInfo: [NSLocalizedDescriptionKey: "\(flag) requires four integers"])
    }
    return PixelRect(x: x, y: y, width: width, height: height)
}

private func parseOptions(_ arguments: [String]) throws -> Options {
    var options = Options()
    var index = 1
    while index < arguments.count {
        let argument = arguments[index]
        switch argument {
        case "--source-rect", "--destination-rect":
            guard index + 4 < arguments.count else { throw NSError(domain: "PetalPresentationObserver", code: 2) }
            let rect = try parseRect(Array(arguments[(index + 1)...(index + 4)]), flag: argument)
            if argument == "--source-rect" { options.sourceRect = rect } else { options.destinationRect = rect }
            index += 5
        case "--samples", "--warmup", "--timeout-seconds", "--output", "--source-window-id", "--destination-window-id", "--display-id", "--display-cg-x", "--display-cg-y", "--display-width-points", "--display-height-points", "--display-scale", "--display-pixel-width", "--display-pixel-height":
            guard index + 1 < arguments.count else { throw NSError(domain: "PetalPresentationObserver", code: 2) }
            let value = arguments[index + 1]
            switch argument {
            case "--samples": options.samples = Int(value) ?? 0
            case "--warmup": options.warmup = Int(value) ?? -1
            case "--timeout-seconds": options.timeoutSeconds = Double(value) ?? 0
            case "--source-window-id": options.sourceWindowID = CGWindowID(UInt32(value) ?? 0)
            case "--destination-window-id": options.destinationWindowID = CGWindowID(UInt32(value) ?? 0)
            case "--display-id": options.displayID = CGDirectDisplayID(UInt32(value) ?? 0)
            case "--display-cg-x": options.displayCGX = Double(value)
            case "--display-cg-y": options.displayCGY = Double(value)
            case "--display-width-points": options.displayWidthPoints = Double(value)
            case "--display-height-points": options.displayHeightPoints = Double(value)
            case "--display-scale": options.displayScale = Double(value)
            case "--display-pixel-width": options.displayPixelWidth = Int(value)
            case "--display-pixel-height": options.displayPixelHeight = Int(value)
            default: options.outputPath = value
            }
            index += 2
        case "--self-test":
            options.selfTest = true
            index += 1
        default:
            throw NSError(domain: "PetalPresentationObserver", code: 2,
                          userInfo: [NSLocalizedDescriptionKey: "unknown argument: \(argument)"])
        }
    }
    guard options.selfTest || (options.sourceRect != nil && options.destinationRect != nil && options.sourceWindowID != nil && options.destinationWindowID != nil && options.displayID != nil && options.displayCGX != nil && options.displayCGY != nil && options.displayWidthPoints != nil && options.displayHeightPoints != nil && options.displayScale != nil && options.displayPixelWidth != nil && options.displayPixelHeight != nil) else {
        throw NSError(domain: "PetalPresentationObserver", code: 2,
                      userInfo: [NSLocalizedDescriptionKey: "source and destination rects are required"])
    }
    guard options.samples > 0, options.warmup >= 0, options.timeoutSeconds > 0 else {
        throw NSError(domain: "PetalPresentationObserver", code: 2,
                      userInfo: [NSLocalizedDescriptionKey: "samples/timeout must be positive and warmup non-negative"])
    }
    return options
}

/// Makes the 16-bit Gray counter monotonic across one modulo wrap while
/// rejecting ordinary backwards movement. A measurement is bounded to fewer
/// than 32K transitions, so a larger backward jump is the only legal wrap.
private final class GenerationUnwrapper {
    private var rawPrevious: Int?
    private(set) var epoch: UInt64 = 0

    func observe(_ raw: Int) -> UInt64? {
        guard let previous = rawPrevious else { rawPrevious = raw; return UInt64(raw) }
        if raw == previous { return epoch * UInt64(generationMask + 1) + UInt64(raw) }
        if raw < previous {
            guard previous - raw > generationMask / 2 else { return nil }
            epoch += 1
        } else if raw - previous > generationMask / 2 {
            return nil
        }
        rawPrevious = raw
        return epoch * UInt64(generationMask + 1) + UInt64(raw)
    }
}

private let calibration: [(PixelRect, RGB)] = [
    (PixelRect(x: 16, y: 16, width: 24, height: 24), RGB(red: 255, green: 45, blue: 85)),
    (PixelRect(x: 920, y: 16, width: 24, height: 24), RGB(red: 0, green: 255, blue: 136)),
    (PixelRect(x: 16, y: 560, width: 24, height: 24), RGB(red: 45, green: 125, blue: 255)),
    (PixelRect(x: 920, y: 560, width: 24, height: 24), RGB(red: 255, green: 212, blue: 0)),
]

private let grayRects = (0..<grayBits).map {
    PixelRect(x: 160 + $0 * 40, y: 88, width: 40, height: 30)
}

private func scaled(_ logical: PixelRect, into crop: PixelRect) -> PixelRect {
    let left = crop.x + Int((Double(logical.x) / Double(patternWidth) * Double(crop.width)).rounded(.down))
    let top = crop.y + Int((Double(logical.y) / Double(patternHeight) * Double(crop.height)).rounded(.down))
    let right = crop.x + Int((Double(logical.maxX) / Double(patternWidth) * Double(crop.width)).rounded(.up))
    let bottom = crop.y + Int((Double(logical.maxY) / Double(patternHeight) * Double(crop.height)).rounded(.up))
    return PixelRect(x: left, y: top, width: right - left, height: bottom - top)
}

private func decodePattern(crop: PixelRect, sample: (Int, Int) -> RGB?) -> DecodedPattern? {
    func mean(_ logical: PixelRect) -> RGB? {
        let rect = scaled(logical, into: crop)
        let insetX = max(1, rect.width / 4)
        let insetY = max(1, rect.height / 4)
        var red = 0.0, green = 0.0, blue = 0.0, count = 0.0
        for y in (rect.y + insetY)..<max(rect.y + insetY + 1, rect.maxY - insetY) {
            for x in (rect.x + insetX)..<max(rect.x + insetX + 1, rect.maxX - insetX) {
                guard let pixel = sample(x, y) else { return nil }
                red += pixel.red; green += pixel.green; blue += pixel.blue; count += 1
            }
        }
        return count > 0 ? RGB(red: red / count, green: green / count, blue: blue / count) : nil
    }

    for (rect, expected) in calibration {
        guard let actual = mean(rect) else { return nil }
        let distance = max(abs(actual.red - expected.red), abs(actual.green - expected.green), abs(actual.blue - expected.blue))
        guard distance <= 100 else { return nil }
    }

    var gray = 0
    var confidence = 1.0
    for rect in grayRects {
        guard let rgb = mean(rect) else { return nil }
        let luma = rgb.red * 0.2126 + rgb.green * 0.7152 + rgb.blue * 0.0722
        let bit: Int
        if luma <= 96 { bit = 0 }
        else if luma >= 159 { bit = 1 }
        else { return nil }
        gray = (gray << 1) | bit
        confidence = min(confidence, abs(luma - 127.5) / 127.5)
    }
    var value = gray
    var shift = 1
    while shift < grayBits { value ^= value >> shift; shift <<= 1 }
    return DecodedPattern(generation: value & generationMask, confidence: confidence)
}

private final class Reducer {
    private let targetSamples: Int
    private let warmup: Int
    // Keep the first observed source presentation for each generation.  The
    // ordinal is deliberately retained: a generation first seen during warmup
    // must never become an eligible post-warmup sample later.
    private(set) var sourceFirst = [UInt64: (timestampMs: Double, ordinal: Int)]()
    private(set) var destinationSeen = Set<UInt64>()
    private(set) var samples = [PairedSample]()
    private(set) var unpairedDestinationGenerations = 0
    private(set) var frameStatusErrors = 0
    private(set) var decodeFailuresAfterReady = 0
    private(set) var counterRegressions = 0
    private let sourceUnwrapper = GenerationUnwrapper()
    private let destinationUnwrapper = GenerationUnwrapper()
    private(set) var sourceTransitions = 0
    private(set) var destinationTransitions = 0
    private var firstSourceMs: Double?
    private var lastSourceMs: Double?
    private var firstDestinationMs: Double?
    private var lastDestinationMs: Double?

    init(targetSamples: Int, warmup: Int) { self.targetSamples = targetSamples; self.warmup = warmup }

    func recordStatusError() { frameStatusErrors += 1 }

    func recordDecodeFailure() {
        // Startup can legitimately contain an unpainted destination.  Once
        // both calibrated patterns have been seen, however, a missing decode
        // means the "visible and unobscured" gate no longer holds.
        if firstSourceMs != nil && firstDestinationMs != nil { decodeFailuresAfterReady += 1 }
    }

    func observe(source: Int?, destination: Int?, at timestampMs: Double) {
        let sourceGeneration = source.flatMap { sourceUnwrapper.observe($0) }
        let destinationGeneration = destination.flatMap { destinationUnwrapper.observe($0) }
        if source != nil && sourceGeneration == nil { counterRegressions += 1; return }
        if destination != nil && destinationGeneration == nil { counterRegressions += 1; return }
        if let sourceGeneration, sourceFirst[sourceGeneration] == nil {
            sourceTransitions += 1
            sourceFirst[sourceGeneration] = (timestampMs, sourceTransitions)
            firstSourceMs = firstSourceMs ?? timestampMs
            lastSourceMs = timestampMs
        }
        guard let destinationGeneration, destinationSeen.insert(destinationGeneration).inserted else { return }
        destinationTransitions += 1
        firstDestinationMs = firstDestinationMs ?? timestampMs
        lastDestinationMs = timestampMs
        guard let source = sourceFirst[destinationGeneration] else {
            if sourceTransitions > warmup { unpairedDestinationGenerations += 1 }
            return
        }
        guard source.ordinal > warmup, samples.count < targetSamples else { return }
        samples.append(PairedSample(generation: destinationGeneration, sourceMs: source.timestampMs, destinationMs: timestampMs))
    }

    var complete: Bool { samples.count >= targetSamples }

    func summary() -> Summary? {
        guard complete, firstSourceMs != nil, lastSourceMs != nil,
              firstDestinationMs != nil, lastDestinationMs != nil else { return nil }
        let values = samples.map(\.latencyMs).sorted()
        func percentile(_ p: Double) -> Double {
            values[max(0, min(values.count - 1, Int(ceil(Double(values.count) * p)) - 1))]
        }
        // A web source canvas may repaint at 60Hz while its published media
        // track is 30Hz.  Cadence therefore comes from the paired, delivered
        // generations rather than every source repaint.
        let sourceSeconds = max(0.001, (samples.last!.sourceMs - samples.first!.sourceMs) / 1000)
        let destinationSeconds = max(0.001, (samples.last!.destinationMs - samples.first!.destinationMs) / 1000)
        return Summary(
            samples: values.count,
            averageMs: values.reduce(0, +) / Double(values.count),
            p50Ms: percentile(0.5),
            p95Ms: percentile(0.95),
            sourceFps: Double(max(0, samples.count - 1)) / sourceSeconds,
            destinationFps: Double(max(0, samples.count - 1)) / destinationSeconds,
            unpairedDestinationGenerations: unpairedDestinationGenerations,
            frameStatusErrors: frameStatusErrors,
            decodeFailuresAfterReady: decodeFailuresAfterReady,
            counterRegressions: counterRegressions
        )
    }
}

private final class StreamObserver: NSObject, SCStreamOutput, SCStreamDelegate {
    let sourceRect: PixelRect
    let destinationRect: PixelRect
    let reducer: Reducer

    init(sourceRect: PixelRect, destinationRect: PixelRect, reducer: Reducer) {
        self.sourceRect = sourceRect
        self.destinationRect = destinationRect
        self.reducer = reducer
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        fputs("OBSERVER_STREAM_ERROR \(error.localizedDescription)\n", stderr)
        reducer.recordStatusError()
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, CMSampleBufferIsValid(sampleBuffer),
              let pixelBuffer = sampleBuffer.imageBuffer else {
            reducer.recordStatusError(); return
        }
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
            as? [[SCStreamFrameInfo: Any]],
           let rawStatus = attachments.first?[.status] as? Int,
           rawStatus != SCFrameStatus.complete.rawValue {
            reducer.recordStatusError(); return
        }
        let width = CVPixelBufferGetWidth(pixelBuffer)
        let height = CVPixelBufferGetHeight(pixelBuffer)
        guard sourceRect.contains(width: width, height: height),
              destinationRect.contains(width: width, height: height),
              !sourceRect.intersects(destinationRect) else {
            reducer.recordStatusError(); return
        }
        CVPixelBufferLockBaseAddress(pixelBuffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, .readOnly) }
        guard CVPixelBufferGetPixelFormatType(pixelBuffer) == kCVPixelFormatType_32BGRA,
              let base = CVPixelBufferGetBaseAddress(pixelBuffer) else {
            reducer.recordStatusError(); return
        }
        let stride = CVPixelBufferGetBytesPerRow(pixelBuffer)
        let bytes = base.assumingMemoryBound(to: UInt8.self)
        let sampler: (Int, Int) -> RGB? = { x, y in
            guard x >= 0, y >= 0, x < width, y < height else { return nil }
            let offset = y * stride + x * 4
            return RGB(red: Double(bytes[offset + 2]), green: Double(bytes[offset + 1]), blue: Double(bytes[offset]))
        }
        let source = decodePattern(crop: sourceRect, sample: sampler)?.generation
        let destination = decodePattern(crop: destinationRect, sample: sampler)?.generation
        if source == nil || destination == nil { reducer.recordDecodeFailure() }
        reducer.observe(source: source, destination: destination, at: ProcessInfo.processInfo.systemUptime * 1000)
    }
}

private func runSelfTest() {
    let unwrap = GenerationUnwrapper()
    precondition(unwrap.observe(65_534) == 65_534)
    precondition(unwrap.observe(65_535) == 65_535)
    precondition(unwrap.observe(0) == 65_536)
    precondition(unwrap.observe(1) == 65_537)
    precondition(unwrap.observe(0) == nil, "ordinary counter regression must fail")
    let width = 960, height = 600, generation = 0x5a3c
    var pixels = Array(repeating: RGB(red: 27, green: 16, blue: 51), count: width * height)
    func paint(_ rect: PixelRect, _ color: RGB) {
        for y in rect.y..<rect.maxY { for x in rect.x..<rect.maxX { pixels[y * width + x] = color } }
    }
    for (rect, color) in calibration { paint(rect, color) }
    let gray = generation ^ (generation >> 1)
    for (index, rect) in grayRects.enumerated() {
        let lit = ((gray >> (grayBits - 1 - index)) & 1) == 1
        paint(rect, lit ? RGB(red: 255, green: 255, blue: 255) : RGB(red: 0, green: 0, blue: 0))
    }
    let full = PixelRect(x: 0, y: 0, width: width, height: height)
    let decoded = decodePattern(crop: full) { x, y in pixels[y * width + x] }
    precondition(decoded?.generation == generation && (decoded?.confidence ?? 0) > 0.9)
    precondition(full.contains(width: width, height: height))
    precondition(!full.intersects(PixelRect(x: 960, y: 0, width: 20, height: 20)))

    func rejects(_ body: () throws -> Void) -> Bool { (try? body()) == nil }
    let retina = DisplayDescriptor(id: 1, frame: PointRect(x: 0, y: 0, width: 1512, height: 982), scale: 2, pixelWidth: 3024, pixelHeight: 1964)
    let exact = DisplayCandidate(index: 0, id: 1, frame: retina.frame, width: 1512, height: 982)
    let sameSizeElsewhere = DisplayCandidate(index: 1, id: 99, frame: PointRect(x: 1512, y: 0, width: 1512, height: 982), width: 1512, height: 982)
    precondition((try! resolveDisplayCandidate([exact, sameSizeElsewhere], descriptor: retina, cgDisplayOnline: true)).1 == .exactID)
    let fallback = DisplayCandidate(index: 0, id: 77, frame: retina.frame, width: 1512, height: 982)
    precondition((try! resolveDisplayCandidate([fallback, sameSizeElsewhere], descriptor: retina, cgDisplayOnline: true)).1 == .structuralFallback)
    precondition(rejects { _ = try resolveDisplayCandidate([], descriptor: retina, cgDisplayOnline: true) })
    precondition(rejects { _ = try resolveDisplayCandidate([fallback, DisplayCandidate(index: 1, id: 78, frame: retina.frame, width: 1512, height: 982)], descriptor: retina, cgDisplayOnline: true) })
    precondition(rejects { _ = try resolveDisplayCandidate([DisplayCandidate(index: 0, id: 1, frame: PointRect(x: 1, y: 0, width: 1512, height: 982), width: 1512, height: 982)], descriptor: retina, cgDisplayOnline: true) })
    precondition(physicalRect(CGRect(x: 20, y: 33, width: 500, height: 375), relativeTo: retina) == PixelRect(x: 40, y: 66, width: 1000, height: 750))
    let secondary = DisplayDescriptor(id: 2, frame: PointRect(x: -1280, y: -720, width: 1280, height: 720), scale: 1, pixelWidth: 1280, pixelHeight: 720)
    precondition(physicalRect(CGRect(x: -1200, y: -600, width: 640, height: 360), relativeTo: secondary) == PixelRect(x: 80, y: 120, width: 640, height: 360))
    let plannedDestination = PixelRect(x: 720, y: 264, width: 640, height: 360)
    precondition(destinationFrameMatchesPlan(plannedDestination, plannedDestination))
    precondition(!destinationFrameMatchesPlan(PixelRect(x: 726, y: 268, width: 628, height: 352), plannedDestination), "shadow/inset destination drift must fail")
    let noDisplay = unavailableDisplayDiagnostic(retina, [])
    precondition(noDisplay?.hasPrefix("INVALID_OBSERVER_DISPLAY_UNAVAILABLE zero_cells=1") == true,
                 "empty display set must emit the exact machine-readable invalid marker")
    precondition(unavailableDisplayDiagnostic(retina, [exact]) == nil, "nonempty display candidate path must remain unchanged")
    // Empty displays take precedence even if the same snapshot also has no
    // matching windows; otherwise an apparatus failure is misclassified.
    var emptyDisplayEvaluatedWindowCheck = false
    let emptyDisplayAndMissingWindows = selectionPreflightFailure(
        descriptor: retina, candidates: [], windowFailure: {
            emptyDisplayEvaluatedWindowCheck = true
            return windowAvailabilityFailure(sourceCount: 0, destinationCount: 0)
        }()
    )
    precondition(emptyDisplayAndMissingWindows == noDisplay && !emptyDisplayEvaluatedWindowCheck,
                 "empty display classification must happen before missing-window evaluation")
    var nonemptyDisplayEvaluatedWindowCheck = false
    let nonemptyDisplayAndMissingWindows = selectionPreflightFailure(
        descriptor: retina, candidates: [exact], windowFailure: {
            nonemptyDisplayEvaluatedWindowCheck = true
            return windowAvailabilityFailure(sourceCount: 0, destinationCount: 0)
        }()
    )
    precondition(nonemptyDisplayEvaluatedWindowCheck,
                 "nonempty display sets must retain immediate window availability checks")
    precondition(
        nonemptyDisplayAndMissingWindows
            == "source/destination SCWindow id is ambiguous or unavailable",
        "missing windows remain an immediate failure only after a nonempty display set"
    )
    precondition(windowAvailabilityFailure(sourceCount: 1, destinationCount: 1) == nil)

    let reducer = Reducer(targetSamples: 3, warmup: 1)
    reducer.observe(source: 1, destination: nil, at: 0)
    reducer.observe(source: 2, destination: 1, at: 33)
    reducer.observe(source: 3, destination: 2, at: 66)
    reducer.observe(source: 4, destination: 3, at: 99)
    reducer.observe(source: 5, destination: 4, at: 132)
    precondition(reducer.complete)
    let summary = reducer.summary()!
    precondition(summary.samples == 3 && summary.p50Ms == 33 && summary.p95Ms == 33)

    let wrapReducer = Reducer(targetSamples: 2, warmup: 0)
    wrapReducer.observe(source: 65_535, destination: nil, at: 0)
    wrapReducer.observe(source: 0, destination: 65_535, at: 33)
    wrapReducer.observe(source: 1, destination: 0, at: 66)
    precondition(wrapReducer.complete && wrapReducer.samples.map(\.generation) == [65_535, 65_536])
    wrapReducer.observe(source: 0, destination: nil, at: 99)
    precondition(wrapReducer.counterRegressions == 1)
    print("SELF_TEST_PASS gray decoder display identity physical geometry reducer")
}

private func samePoint(_ first: Double, _ second: Double) -> Bool { abs(first - second) < 0.001 }

private func sameFrame(_ first: PointRect, _ second: PointRect) -> Bool {
    samePoint(first.x, second.x) && samePoint(first.y, second.y)
        && samePoint(first.width, second.width) && samePoint(first.height, second.height)
}

private func descriptor(from options: Options) throws -> DisplayDescriptor {
    guard let id = options.displayID, let x = options.displayCGX, let y = options.displayCGY,
          let width = options.displayWidthPoints, let height = options.displayHeightPoints,
          let scale = options.displayScale, let pixelWidth = options.displayPixelWidth,
          let pixelHeight = options.displayPixelHeight,
          width > 0, height > 0, scale > 0, pixelWidth > 0, pixelHeight > 0,
          Int((width * scale).rounded()) == pixelWidth,
          Int((height * scale).rounded()) == pixelHeight else {
        throw NSError(domain: "PetalPresentationObserver", code: 2,
                      userInfo: [NSLocalizedDescriptionKey: "selected display descriptor is incomplete or inconsistent"])
    }
    return DisplayDescriptor(id: id, frame: PointRect(x: x, y: y, width: width, height: height), scale: scale, pixelWidth: pixelWidth, pixelHeight: pixelHeight)
}

private func physicalRect(_ rect: CGRect, relativeTo display: DisplayDescriptor) -> PixelRect? {
    let frame = display.frame.cgRect
    guard frame.contains(rect) else { return nil }
    return PixelRect(
        x: Int(((rect.minX - frame.minX) * display.scale).rounded()),
        y: Int(((rect.minY - frame.minY) * display.scale).rounded()),
        width: Int((rect.width * display.scale).rounded()),
        height: Int((rect.height * display.scale).rounded())
    )
}

private func contains(_ outer: PixelRect, _ inner: PixelRect) -> Bool {
    inner.x >= outer.x && inner.y >= outer.y && inner.maxX <= outer.maxX && inner.maxY <= outer.maxY
}

private func destinationFrameMatchesPlan(_ observed: PixelRect, _ planned: PixelRect) -> Bool { observed == planned }

private func candidateDescription(_ candidate: DisplayCandidate) -> String {
    "id=\(candidate.id) frame=(\(candidate.frame.x),\(candidate.frame.y),\(candidate.frame.width),\(candidate.frame.height)) size=\(candidate.width)x\(candidate.height)"
}

private func unavailableDisplayDiagnostic(_ descriptor: DisplayDescriptor, _ candidates: [DisplayCandidate]) -> String? {
    guard candidates.isEmpty else { return nil }
    return "\(invalidObserverDisplayUnavailable) zero_cells=1 requested_id=\(descriptor.id) requested_frame=(\(descriptor.frame.x),\(descriptor.frame.y),\(descriptor.frame.width),\(descriptor.frame.height)) candidate_count=0 available=[] resume=retry-only-after-SCShareableContent-reports-one-matching-display"
}

private func windowAvailabilityFailure(sourceCount: Int, destinationCount: Int) -> String? {
    guard sourceCount == 1, destinationCount == 1 else {
        return "source/destination SCWindow id is ambiguous or unavailable"
    }
    return nil
}

/// Do not evaluate window availability until display enumeration has succeeded:
/// an empty display set is an invalid measurement apparatus, irrespective of
/// whether the same snapshot also lacks either target window.
private func selectionPreflightFailure(
    descriptor: DisplayDescriptor,
    candidates: [DisplayCandidate],
    windowFailure: @autoclosure () -> String?
) -> String? {
    if let invalid = unavailableDisplayDiagnostic(descriptor, candidates) { return invalid }
    return windowFailure()
}

private func structurallyMatches(_ candidate: DisplayCandidate, _ descriptor: DisplayDescriptor) -> Bool {
    sameFrame(candidate.frame, descriptor.frame)
        && samePoint(candidate.width, descriptor.frame.width)
        && samePoint(candidate.height, descriptor.frame.height)
}

private func resolveDisplayCandidate(_ candidates: [DisplayCandidate], descriptor: DisplayDescriptor, cgDisplayOnline: Bool) throws -> (DisplayCandidate, DisplaySelection) {
    let exact = candidates.filter { $0.id == descriptor.id }
    if exact.count == 1 {
        guard structurallyMatches(exact[0], descriptor) else {
            throw NSError(domain: "PetalPresentationObserver", code: 3,
                          userInfo: [NSLocalizedDescriptionKey: "exact SCDisplay ID has inconsistent frame or point dimensions"])
        }
        return (exact[0], .exactID)
    }
    guard exact.isEmpty else {
        throw NSError(domain: "PetalPresentationObserver", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "exact SCDisplay ID is ambiguous"])
    }
    guard cgDisplayOnline else {
        throw NSError(domain: "PetalPresentationObserver", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "coordinator-selected CG display is offline; structural fallback refused"])
    }
    let structural = candidates.filter { structurallyMatches($0, descriptor) }
    guard structural.count == 1 else {
        throw NSError(domain: "PetalPresentationObserver", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: structural.isEmpty ? "no structural SCDisplay candidate" : "structural SCDisplay candidate is ambiguous"])
    }
    return (structural[0], .structuralFallback)
}

private func selectedDisplay(
    content: SCShareableContent,
    sourceWindowID: CGWindowID,
    destinationWindowID: CGWindowID,
    descriptor: DisplayDescriptor,
    sourceCrop: PixelRect,
    destinationCrop: PixelRect
) throws -> (SCDisplay, DisplaySelection) {
    let candidates = content.displays.enumerated().map { index, display in
        DisplayCandidate(index: index, id: display.displayID,
                         frame: PointRect(x: display.frame.minX, y: display.frame.minY, width: display.frame.width, height: display.frame.height),
                         width: Double(display.width), height: Double(display.height))
    }
    print("OBSERVER_DISPLAY_CANDIDATES requested=id=\(descriptor.id) frame=(\(descriptor.frame.x),\(descriptor.frame.y),\(descriptor.frame.width),\(descriptor.frame.height)) scale=\(descriptor.scale) physical=\(descriptor.pixelWidth)x\(descriptor.pixelHeight) available=[\(candidates.map(candidateDescription).joined(separator: "; "))]")
    if let failure = selectionPreflightFailure(
        descriptor: descriptor, candidates: candidates,
        windowFailure: windowAvailabilityFailure(
            sourceCount: content.windows.filter { $0.windowID == sourceWindowID }.count,
            destinationCount: content.windows.filter { $0.windowID == destinationWindowID }.count
        )
    ) {
        if failure.hasPrefix("\(invalidObserverDisplayUnavailable) zero_cells=1") {
            fputs("\(failure)\n", stderr)
        }
        throw NSError(domain: "PetalPresentationObserver", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: failure])
    }
    // The display set was nonempty and its windows passed the preflight above.
    let sources = content.windows.filter { $0.windowID == sourceWindowID }
    let destinations = content.windows.filter { $0.windowID == destinationWindowID }
    let sourceFrame = sources[0].frame
    let destinationFrame = destinations[0].frame
    let cgBounds = CGDisplayBounds(descriptor.id)
    let cgOnline = CGDisplayIsOnline(descriptor.id) != 0
    guard cgOnline, sameFrame(PointRect(x: cgBounds.minX, y: cgBounds.minY, width: cgBounds.width, height: cgBounds.height), descriptor.frame) else {
        throw NSError(domain: "PetalPresentationObserver", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "coordinator-selected CG display is offline or structurally inconsistent"])
    }
    let (candidate, selection) = try resolveDisplayCandidate(candidates, descriptor: descriptor, cgDisplayOnline: cgOnline)
    let display = content.displays[candidate.index]
    guard let sourceWindowPixels = physicalRect(sourceFrame, relativeTo: descriptor),
          let destinationWindowPixels = physicalRect(destinationFrame, relativeTo: descriptor) else {
        throw NSError(domain: "PetalPresentationObserver", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "source/destination windows are not on the required display"])
    }
    let displayPixels = PixelRect(x: 0, y: 0, width: descriptor.pixelWidth, height: descriptor.pixelHeight)
    guard contains(displayPixels, sourceCrop), contains(displayPixels, destinationCrop),
          contains(sourceWindowPixels, sourceCrop), destinationFrameMatchesPlan(destinationWindowPixels, destinationCrop),
          !sourceCrop.intersects(destinationCrop) else {
        throw NSError(domain: "PetalPresentationObserver", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "window/crop transform is invalid, ambiguous, out-of-bounds, or overlapping"])
    }
    print("OBSERVER_DISPLAY_SELECTED mode=\(selection.rawValue) sc_id=\(display.displayID) source_sck=(\(sourceFrame.minX),\(sourceFrame.minY),\(sourceFrame.width),\(sourceFrame.height)) source_px=\(sourceWindowPixels) destination_sck=(\(destinationFrame.minX),\(destinationFrame.minY),\(destinationFrame.width),\(destinationFrame.height)) destination_px=\(destinationWindowPixels)")
    return (display, selection)
}

private func run(_ options: Options) async throws {
    guard let sourceRect = options.sourceRect, let destinationRect = options.destinationRect,
          let sourceWindowID = options.sourceWindowID, let destinationWindowID = options.destinationWindowID,
          let _ = options.displayID else { return }
    let displayDescriptor = try descriptor(from: options)
    let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
    let (display, selection) = try selectedDisplay(content: content, sourceWindowID: sourceWindowID,
                                      destinationWindowID: destinationWindowID, descriptor: displayDescriptor,
                                      sourceCrop: sourceRect, destinationCrop: destinationRect)
    let config = SCStreamConfiguration()
    config.width = displayDescriptor.pixelWidth
    config.height = displayDescriptor.pixelHeight
    config.pixelFormat = kCVPixelFormatType_32BGRA
    config.minimumFrameInterval = CMTime(value: 1, timescale: 60)
    config.queueDepth = 3
    config.showsCursor = false
    let filter = SCContentFilter(display: display, excludingWindows: [])
    let reducer = Reducer(targetSamples: options.samples, warmup: options.warmup)
    let observer = StreamObserver(sourceRect: sourceRect, destinationRect: destinationRect, reducer: reducer)
    let stream = SCStream(filter: filter, configuration: config, delegate: observer)
    try stream.addStreamOutput(observer, type: .screen, sampleHandlerQueue: DispatchQueue(label: "petal.issue613.presentation", qos: .userInteractive))
    try await stream.startCapture()
    print("OBSERVER_READY display_px=\(displayDescriptor.pixelWidth)x\(displayDescriptor.pixelHeight) selection=\(selection.rawValue) source_window_id=\(sourceWindowID) destination_window_id=\(destinationWindowID) raw_pixels_persisted=false sentry=false")
    fflush(stdout)
    let deadline = ProcessInfo.processInfo.systemUptime + options.timeoutSeconds
    while !reducer.complete && ProcessInfo.processInfo.systemUptime < deadline {
        try await Task.sleep(nanoseconds: 10_000_000)
    }
    try await stream.stopCapture()
    guard let summary = reducer.summary() else {
        throw NSError(domain: "PetalPresentationObserver", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "measurement incomplete or timed out"])
    }
    if let outputPath = options.outputPath {
        var csv = "generation,source_ms,destination_ms,latency_ms\n"
        for sample in reducer.samples {
            csv += "\(sample.generation),\(sample.sourceMs),\(sample.destinationMs),\(sample.latencyMs)\n"
        }
        try csv.write(toFile: outputPath, atomically: true, encoding: .utf8)
    }
    let json = try JSONEncoder().encode(summary)
    print("PRESENTATION_RESULT_JSON \(String(decoding: json, as: UTF8.self))")
}

do {
    let options = try parseOptions(CommandLine.arguments)
    if options.selfTest {
        runSelfTest()
        exit(0)
    }
    let semaphore = DispatchSemaphore(value: 0)
    var terminalError: Error?
    Task {
        do { try await run(options) } catch { terminalError = error }
        semaphore.signal()
    }
    while semaphore.wait(timeout: .now() + 0.05) == .timedOut {
        RunLoop.current.run(mode: .default, before: Date(timeIntervalSinceNow: 0.02))
    }
    if let terminalError { throw terminalError }
} catch {
    fputs("presentation-latency-observer: \(error.localizedDescription)\n", stderr)
    exit(3)
}
