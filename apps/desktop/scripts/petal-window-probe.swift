// Live window observer + synthetic pointer driver for the #416 acceptance run.
//
// Why a separate native tool: #416 is a race between a USER drag-resize of a
// receiver panel and a source-side resize. The repo's existing tests exercise
// only the pure decision helpers, never the real `WindowEvent::Resized` chain
// (see CLAUDE.md, "Native window-lifecycle changes need a live-exercising
// test"). Driving the panel's real resize handle with real posted mouse events
// and reading the panel's real WindowServer frame is the only way to exercise
// that chain from outside the app.
//
// Subcommands:
//   --find <ownerSubstring>            JSON array of matching on-screen windows
//   --sample <ms> <intervalMs> <owner> JSONL frames for every matching window
//   --drag <x1> <y1> <x2> <y2> <steps> <stepMs>   posted left-button drag
//   --press <x> <y>                    posted left-button down (no up)
//   --release <x> <y>                  posted left-button up
//   --move <x> <y>                     posted buttonless move
// Coordinates are global, top-left origin -- CGEvent's own space.

import AppKit
import Foundation

private func windows(owner: String?) -> [[String: Any]] {
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    var out: [[String: Any]] = []
    for (index, w) in list.enumerated() {
        let ownerName = w[kCGWindowOwnerName as String] as? String ?? ""
        if let owner, !ownerName.localizedCaseInsensitiveContains(owner) { continue }
        let b = w[kCGWindowBounds as String] as? [String: CGFloat] ?? [:]
        out.append([
            "z": index,
            "windowNumber": w[kCGWindowNumber as String] as? Int ?? -1,
            "owner": ownerName,
            "name": w[kCGWindowName as String] as? String ?? "",
            "layer": w[kCGWindowLayer as String] as? Int ?? -1,
            "pid": w[kCGWindowOwnerPID as String] as? Int ?? -1,
            "x": Double(b["X"] ?? 0), "y": Double(b["Y"] ?? 0),
            "w": Double(b["Width"] ?? 0), "h": Double(b["Height"] ?? 0),
        ])
    }
    return out
}

private func emit(_ value: Any) {
    guard let data = try? JSONSerialization.data(withJSONObject: value),
          let text = String(data: data, encoding: .utf8) else { return }
    FileHandle.standardOutput.write(Data((text + "\n").utf8))
}

private func post(_ type: CGEventType, _ point: CGPoint) {
    guard let event = CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: point, mouseButton: .left) else { return }
    // See remote-control-photon-sentinel.swift: a synthesized mouse-down with
    // click state 0 is dropped by AppKit and looks like a delivery failure.
    event.setIntegerValueField(.mouseEventClickState, value: 1)
    event.setIntegerValueField(.mouseEventButtonNumber, value: 0)
    event.post(tap: .cgSessionEventTap)
}

let args = CommandLine.arguments
guard args.count > 1 else {
    FileHandle.standardError.write(Data("usage: --find|--sample|--drag|--press|--release|--move\n".utf8))
    exit(2)
}

switch args[1] {
case "--find":
    emit(windows(owner: args.count > 2 ? args[2] : nil))
case "--sample":
    let durationMs = Double(args[2]) ?? 2000
    let intervalMs = Double(args[3]) ?? 25
    let owner = args.count > 4 ? args[4] : nil
    let deadline = Date().addingTimeInterval(durationMs / 1000)
    while Date() < deadline {
        emit(["tMs": Int(Date().timeIntervalSince1970 * 1000), "windows": windows(owner: owner)])
        Thread.sleep(forTimeInterval: intervalMs / 1000)
    }
case "--drag":
    let x1 = Double(args[2])!, y1 = Double(args[3])!
    let x2 = Double(args[4])!, y2 = Double(args[5])!
    let steps = Int(args[6]) ?? 10
    let stepMs = Double(args[7]) ?? 40
    post(.mouseMoved, CGPoint(x: x1, y: y1))
    Thread.sleep(forTimeInterval: 0.05)
    post(.leftMouseDown, CGPoint(x: x1, y: y1))
    Thread.sleep(forTimeInterval: stepMs / 1000)
    for step in 1...steps {
        let t = Double(step) / Double(steps)
        post(.leftMouseDragged, CGPoint(x: x1 + (x2 - x1) * t, y: y1 + (y2 - y1) * t))
        Thread.sleep(forTimeInterval: stepMs / 1000)
    }
    post(.leftMouseUp, CGPoint(x: x2, y: y2))
    emit(["ok": true, "from": ["x": x1, "y": y1], "to": ["x": x2, "y": y2], "steps": steps])
case "--press":
    let x = Double(args[2])!, y = Double(args[3])!
    post(.mouseMoved, CGPoint(x: x, y: y))
    Thread.sleep(forTimeInterval: 0.05)
    post(.leftMouseDown, CGPoint(x: x, y: y))
    emit(["ok": true, "action": "press"])
case "--release":
    let x = Double(args[2])!, y = Double(args[3])!
    post(.leftMouseUp, CGPoint(x: x, y: y))
    emit(["ok": true, "action": "release"])
case "--move":
    let x = Double(args[2])!, y = Double(args[3])!
    post(.leftMouseDragged, CGPoint(x: x, y: y))
    emit(["ok": true, "action": "move"])
case "--hit":
    // Topmost on-screen window containing a point, across ALL apps. A posted
    // mouse event is hit-tested against the real window stack, so a gesture
    // aimed at a covered handle silently does nothing -- indistinguishable
    // from "the fix is broken" unless the harness checks first.
    let px = Double(args[2])!, py = Double(args[3])!
    let hits = windows(owner: nil).filter { w in
        guard let x = w["x"] as? Double, let y = w["y"] as? Double,
              let ww = w["w"] as? Double, let hh = w["h"] as? Double,
              let layer = w["layer"] as? Int, layer == 0
        else { return false }
        return px >= x && px <= x + ww && py >= y && py <= y + hh
    }
    emit(hits.first ?? [:])
case "--hover":
    // Buttonless move. `--move` posts a DRAG (button-held) event, which is the
    // wrong thing to use before the mouse-down: AppKit routes a drag to
    // whatever owns the current mouse-down session, so using it as a "put the
    // cursor here first" step can deliver it somewhere unrelated.
    let x = Double(args[2])!, y = Double(args[3])!
    post(.mouseMoved, CGPoint(x: x, y: y))
    emit(["ok": true, "action": "hover"])
case "--activate":
    // Raise the owning app WITHOUT osascript/System Events, which needs an
    // Automation TCC grant for whatever parent process runs this harness and
    // fails silently when it is missing -- an invisible way for every posted
    // event to land on a covered window.
    let pid = pid_t(args[2])!
    if let app = NSRunningApplication(processIdentifier: pid) {
        let ok = app.activate(options: [.activateAllWindows])
        emit(["ok": ok, "action": "activate", "pid": Int(pid)])
    } else {
        emit(["ok": false, "action": "activate", "pid": Int(pid), "error": "no such process"])
    }
default:
    FileHandle.standardError.write(Data("unknown subcommand \(args[1])\n".utf8))
    exit(2)
}
