// CUBridge — the minimal Swift side of the Computer Use macOS driver.
//
// Rust owns the runtime, the session logic, and (via CoreGraphics FFI) the
// mouse + keyboard. Swift is used ONLY where it is the mature API:
//   - ScreenCaptureKit captures (pixel-accurate, includes the cursor)
//   - NSScreen / NSWorkspace for display names and the frontmost app
//   - NSPasteboard for the clipboard fallback text-input method
//   - AXUIElement for the focused window title
//
// Protocol: one JSON object per line on stdin, one per line on stdout.
// Request:  {"id":1,"method":"displays"} | "capture" | "active" | "permissions"
//           | "clipboard_get" | "clipboard_set"
// Response: {"id":1,"ok":true,"data":{...}}  or  {"id":1,"ok":false,"error":"..."}

import Foundation
import CoreGraphics
import ScreenCaptureKit
import AppKit
import ApplicationServices
import CoreImage
import QuartzCore

// MARK: - Sendable-safe capture delegate holder

final class CaptureBridge: @unchecked Sendable {
    var image: CGImage?
    var error: Error?
    let semaphore = DispatchSemaphore(value: 0)
}

// MARK: - Helpers

func jsonString(_ value: [String: Any]) -> String {
    let data = try! JSONSerialization.data(withJSONObject: value, options: [])
    return String(data: data, encoding: .utf8) ?? "{}"
}

func ok(_ id: Any, _ data: [String: Any]) -> String {
    var m = data
    m["id"] = id
    m["ok"] = true
    return jsonString(m)
}

func err(_ id: Any, _ message: String) -> String {
    return jsonString(["id": id, "ok": false, "error": message])
}

/// Run the main run loop until the semaphore is signalled, so async
/// ScreenCaptureKit callbacks can complete in this synchronous CLI loop.
func waitForSemaphore(_ sem: DispatchSemaphore) {
    while sem.wait(timeout: .now() + 0.05) == .timedOut {
        RunLoop.main.run(mode: .default, before: Date(timeIntervalSinceNow: 0.05))
    }
}

/// waitForSemaphore bounded by a deadline — returns false if the callback
/// never fires. ScreenCaptureKit silently drops the completion callback in
/// some states (e.g. a locked screen on recent macOS), and a bridge stuck
/// here never returns to its readLine loop, so the Rust side would have to
/// time it out and would orphan this process.
func waitForSemaphore(_ sem: DispatchSemaphore, within timeout: TimeInterval) -> Bool {
    let deadline = Date(timeIntervalSinceNow: timeout)
    while sem.wait(timeout: .now() + 0.05) == .timedOut {
        if Date() >= deadline { return false }
        RunLoop.main.run(mode: .default, before: Date(timeIntervalSinceNow: 0.05))
    }
    return true
}

// MARK: - Screen lock detection

/// True when the console session's screen is locked. While locked, recent
/// macOS (26.x) never completes ScreenCaptureKit captures — the bridge would
/// hang until the Rust-side deadline and get orphaned. Detect it up front
/// and fail fast with an actionable error instead.
func screenIsLocked() -> Bool {
    guard let session = CGSessionCopyCurrentDictionary() as? [String: Any] else {
        return false
    }
    return (session["CGSSessionScreenIsLocked"] as? NSNumber)?.boolValue == true
}

func displayBounds(_ id: CGDirectDisplayID) -> [String: Any]? {
    let b = CGDisplayBounds(id)
    return ["x": Double(b.origin.x), "y": Double(b.origin.y),
            "width": Double(b.size.width), "height": Double(b.size.height)]
}

func listDisplays() -> [[String: Any]] {
    var result: [[String: Any]] = []
    let maxDisplays: UInt32 = 32
    var activeDisplays = [CGDirectDisplayID](repeating: 0, count: Int(maxDisplays))
    var count: UInt32 = 0
    let errCode = CGGetActiveDisplayList(maxDisplays, &activeDisplays, &count)
    guard errCode == .success else { return result }
    let main = CGMainDisplayID()
    for i in 0..<Int(count) {
        let d = activeDisplays[i]
        let bounds = displayBounds(d) ?? [:]
        let pw = CGDisplayPixelsWide(d)
        let ph = CGDisplayPixelsHigh(d)
        let bw = (bounds["width"] as? Double) ?? 1.0
        let scale = bw > 0 ? Double(pw) / bw : 1.0
        let name: String = {
            if let s = NSScreen.screens.first(where: { $0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32 == d }) {
                return s.localizedName
            }
            return "Display \(d)"
        }()
        result.append([
            "id": String(d),
            "name": name,
            "bounds": bounds,
            "pixel_width": pw,
            "pixel_height": ph,
            "scale_factor": scale,
            "is_main": d == main,
        ])
    }
    return result
}

// MARK: - Ghost Cursor Overlay (round 8: Pointer Isolation + Visual Cursor)

final class GhostCursorOverlay {
    static let shared = GhostCursorOverlay()

    private var window: NSPanel?
    private var ringLayer: CAShapeLayer?
    private var centerDot: CALayer?
    private var ring2: CAShapeLayer?
    private let windowSize: CGFloat = 56

    private var visible = false

    private func ensureNSApp() {
        if NSApp == nil {
            _ = NSApplication.shared
            NSApp?.setActivationPolicy(.accessory)
        }
    }

    var currentWindowID: CGWindowID? {
        guard let w = window, w.isVisible else { return nil }
        return CGWindowID(w.windowNumber)
    }

    func show(x: Double, y: Double, displayId: String, mode: String) {
        ensureNSApp()
        visible = true
        DispatchQueue.main.async {
            guard let screen = GhostCursorOverlay.screen(forCG: CGPoint(x: x, y: y),
                                                         displayId: displayId) else { return }
            self.configureWindowIfNeeded()
            guard let w = self.window else { return }
            let nsPoint = NSPoint(x: x - self.windowSize / 2,
                                  y: NSMaxY(screen.frame) - y - self.windowSize / 2)
            var frame = NSRect(origin: nsPoint, size: NSSize(width: self.windowSize, height: self.windowSize))
            frame = frame.intersection(screen.frame)
            w.setFrame(frame, display: true)
            self.applyMode(mode, on: screen)
            if !w.isVisible {
                w.orderFrontRegardless()
            }
        }
    }

    func hide() {
        ensureNSApp()
        visible = false
        DispatchQueue.main.async {
            self.window?.orderOut(nil)
        }
    }

    func clickRipple(x: Double, y: Double) {
        ensureNSApp()
        guard visible else { return }
        DispatchQueue.main.async {
            guard let screen = GhostCursorOverlay.screen(forCG: CGPoint(x: x, y: y),
                                                         displayId: nil) else { return }
            self.configureWindowIfNeeded()
            guard let w = self.window else { return }
            let nsPoint = NSPoint(x: x - self.windowSize / 2,
                                  y: NSMaxY(screen.frame) - y - self.windowSize / 2)
            let frame = NSRect(origin: nsPoint, size: NSSize(width: self.windowSize, height: self.windowSize))
            w.setFrame(frame, display: true)
            w.orderFrontRegardless()
            self.playRipple(on: screen)
        }
    }

    private func configureWindowIfNeeded() {
        if window != nil { return }
        let panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: windowSize, height: windowSize),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.isFloatingPanel = true
        panel.ignoresMouseEvents = true
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.isMovable = false
        panel.hidesOnDeactivate = false
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .stationary, .fullScreenAuxiliary, .ignoresCycle]
        panel.isReleasedWhenClosed = false

        let layer = CALayer()
        layer.backgroundColor = NSColor.clear.cgColor
        panel.contentView?.wantsLayer = true
        panel.contentView?.layer = layer
        panel.contentView?.layer?.masksToBounds = false

        let ring = CAShapeLayer()
        ring.fillColor = NSColor.clear.cgColor
        ring.lineWidth = 2.2
        panel.contentView?.layer?.addSublayer(ring)
        ringLayer = ring

        ring2 = CAShapeLayer()
        ring2?.fillColor = NSColor.clear.cgColor
        ring2?.lineWidth = 1.6
        ring2?.opacity = 0
        panel.contentView?.layer?.addSublayer(ring2!)

        let dot = CALayer()
        dot.bounds = CGRect(x: 0, y: 0, width: 5, height: 5)
        dot.cornerRadius = 2.5
        panel.contentView?.layer?.addSublayer(dot)
        centerDot = dot

        window = panel
    }

    private func applyMode(_ mode: String, on screen: NSScreen) {
        let scale = screen.backingScaleFactor
        window?.contentView?.layer?.contentsScale = scale
        guard let contentLayer = window?.contentView?.layer,
              let ring = ringLayer, let dot = centerDot else { return }

        let color: NSColor
        switch mode {
        case "physical_fallback":
            color = NSColor(calibratedRed: 1.0, green: 0.55, blue: 0.0, alpha: 1.0)
        case "paused":
            color = NSColor(calibratedWhite: 0.62, alpha: 0.95)
        case "user_takeover":
            color = NSColor(calibratedRed: 1.0, green: 0.22, blue: 0.2, alpha: 1.0)
        default:
            color = NSColor(calibratedRed: 0.0, green: 0.78, blue: 1.0, alpha: 1.0)
        }
        dot.backgroundColor = color.cgColor

        let center = CGPoint(x: contentLayer.bounds.midX, y: contentLayer.bounds.midY)
        ring.path = CGPath(ellipseIn: CGRect(x: center.x - 10, y: center.y - 10,
                                             width: 20, height: 20), transform: nil)
        ring.strokeColor = color.cgColor
        dot.position = center

        contentLayer.sublayers?.removeAll { $0 is CATextLayer }
        let label = CATextLayer()
        label.string = "AI"
        label.fontSize = 10
        label.font = NSFont.systemFont(ofSize: 10, weight: .semibold)
        label.foregroundColor = color.cgColor
        label.alignmentMode = .center
        label.contentsScale = scale
        let labelW: CGFloat = 24
        label.frame = CGRect(x: contentLayer.bounds.midX - labelW / 2,
                             y: center.y - 26, width: labelW, height: 12)
        contentLayer.addSublayer(label)
    }

    private func playRipple(on screen: NSScreen) {
        guard let contentLayer = window?.contentView?.layer, let ring2 = ring2 else { return }
        let center = CGPoint(x: contentLayer.bounds.midX, y: contentLayer.bounds.midY)
        ring2.strokeColor = NSColor(calibratedRed: 0.0, green: 0.78, blue: 1.0, alpha: 0.9).cgColor
        ring2.removeAllAnimations()
        ring2.opacity = 0.9
        ring2.path = CGPath(ellipseIn: CGRect(x: center.x - 2, y: center.y - 2, width: 4, height: 4),
                            transform: nil)
        let anim = CABasicAnimation(keyPath: "path")
        anim.fromValue = CGPath(ellipseIn: CGRect(x: center.x - 2, y: center.y - 2, width: 4, height: 4), transform: nil)
        anim.toValue = CGPath(ellipseIn: CGRect(x: center.x - 18, y: center.y - 18, width: 36, height: 36), transform: nil)
        anim.duration = 0.22
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 0.9
        fade.toValue = 0.0
        fade.duration = 0.22
        let group = CAAnimationGroup()
        group.animations = [anim, fade]
        group.duration = 0.22
        group.timingFunction = CAMediaTimingFunction(name: .easeOut)
        ring2.add(group, forKey: "ripple")
    }

    static func screen(forCG point: CGPoint, displayId: String?) -> NSScreen? {
        let onScreen: (NSScreen) -> Bool = { sc in
            guard let id = (sc.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32) else { return false }
            let b = CGDisplayBounds(id)
            return point.x >= b.origin.x && point.x < b.origin.x + b.size.width &&
                   point.y >= b.origin.y && point.y < b.origin.y + b.size.height
        }
        if let s = NSScreen.screens.first(where: onScreen) { return s }
        if let d = displayId, let id = UInt64(d),
           let s = NSScreen.screens.first(where: {
               ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32).map(UInt64.init) == id
           }) {
            return s
        }
        return NSScreen.main
    }
}

func handleOverlay(_ params: [String: Any], id: Any) -> String {
    let action = params["action"] as? String ?? ""
    switch action {
    case "show":
        let x = params["x"] as? Double ?? 0
        let y = params["y"] as? Double ?? 0
        let displayId = params["display"] as? String ?? ""
        let mode = params["mode"] as? String ?? "isolated"
        GhostCursorOverlay.shared.show(x: x, y: y, displayId: displayId, mode: mode)
        return ok(id, ["shown": true, "window_id": GhostCursorOverlay.shared.currentWindowID.map(String.init) ?? ""])
    case "hide":
        GhostCursorOverlay.shared.hide()
        return ok(id, ["shown": false])
    case "click_ripple":
        let x = params["x"] as? Double ?? 0
        let y = params["y"] as? Double ?? 0
        GhostCursorOverlay.shared.clickRipple(x: x, y: y)
        return ok(id, ["ripple": true])
    default:
        return err(id, "cursor_overlay requires action: show | hide | click_ripple")
    }
}

// MARK: - Capture (ScreenCaptureKit)

func getShareableContent() throws -> SCShareableContent {
    let sem = DispatchSemaphore(value: 0)
    var out: SCShareableContent?
    var fail: Error?
    Task {
        do {
            out = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        } catch {
            fail = error
        }
        sem.signal()
    }
    while sem.wait(timeout: .now() + 0.05) == .timedOut {
        RunLoop.main.run(mode: .default, before: Date(timeIntervalSinceNow: 0.05))
    }
    if let fail { throw fail }
    guard let out else {
        throw NSError(domain: "CUBridge", code: 1,
                      userInfo: [NSLocalizedDescriptionKey: "Unable to enumerate shareable content (Screen Recording permission required?)"])
    }
    return out
}

func captureDisplay(displayId: CGDirectDisplayID, outputPath: String,
                    showsCursor: Bool, maxWidth: Int, format: String,
                    quality: Int,
                    region: [Double]?) throws -> [String: Any] {
    // ScreenCaptureKit silently hangs (no error, no completion callback) when
    // the calling process lacks Screen Recording permission. Fail fast with a
    // clear message instead. The permission is bound to the executable's
    // ad-hoc signature (cdhash), so rebuilding cubridge resets it — see README.
    guard CGPreflightScreenCaptureAccess() else {
        throw NSError(domain: "CUBridge", code: 5,
                      userInfo: [NSLocalizedDescriptionKey: "Screen Recording permission is not granted for cubridge. Open System Settings > Privacy & Security > Screen Recording and enable cubridge (~/.computer-use/bin/cubridge). Rebuilding cubridge resets this permission — re-grant it after any rebuild."])
    }
    if screenIsLocked() {
        throw NSError(domain: "CUBridge", code: 6,
                      userInfo: [NSLocalizedDescriptionKey: "The screen is locked — unlock the display before capturing"])
    }
    let content = try getShareableContent()
    guard let display = content.displays.first(where: { $0.displayID == displayId }) else {
        throw NSError(domain: "CUBridge", code: 2,
                      userInfo: [NSLocalizedDescriptionKey: "Display \(displayId) not found"])
    }

    // SCDisplay.width/height are pixels; SCStreamConfiguration is in pixels too.
    let config = SCStreamConfiguration()
    config.width = Int(display.width)
    config.height = Int(display.height)
    config.showsCursor = showsCursor
    config.capturesAudio = false
    config.pixelFormat = kCVPixelFormatType_32BGRA
    config.scalesToFit = false

    // Round 8: the agent's Ghost Cursor overlay must never appear in model
    // screenshots (the model would mistake its own cursor for a page element).
    // Exclude it by window id when it exists.
    var excludedWindows: [SCWindow] = []
    if let overlayId = GhostCursorOverlay.shared.currentWindowID,
       let ow = content.windows.first(where: { $0.windowID == overlayId }) {
        excludedWindows.append(ow)
    }
    let filter = SCContentFilter(display: display, excludingWindows: excludedWindows)
    let bridge = CaptureBridge()

    DispatchQueue.main.async {
        SCScreenshotManager.captureImage(contentFilter: filter, configuration: config) { image, error in
            bridge.image = image
            bridge.error = error
            bridge.semaphore.signal()
        }
    }
    // 15s < the Rust-side 30s bridge deadline: if ScreenCaptureKit drops the
    // callback (e.g. a lock race or a wedged WindowServer) the bridge fails
    // itself instead of being timed out and orphaned.
    guard waitForSemaphore(bridge.semaphore, within: 15) else {
        throw NSError(domain: "CUBridge", code: 7,
                      userInfo: [NSLocalizedDescriptionKey: "screen capture did not complete within 15s (screen locked or WindowServer busy?)"])
    }

    if let error = bridge.error {
        throw error
    }
    guard let image = bridge.image else {
        throw NSError(domain: "CUBridge", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "Capture produced no image"])
    }

    var finalImage = image
    // P0-6: window-scoped observe — crop to a pixel rectangle relative to the
    // captured display's top-left. The region arrives in image pixels, so the
    // CGImage crop is direct (top-left origin). The rect is clamped to the
    // display; a window that moved fully off-screen yields a zero-area crop
    // and we fall back to the full frame (the Rust side detects the mismatch
    // and reports TARGET_UNAVAILABLE rather than trusting a wrong image).
    if let region = region, region.count == 4 {
        let x = region[0].rounded(.down)
        let y = region[1].rounded(.down)
        let w = region[2].rounded(.up)
        let h = region[3].rounded(.up)
        let cx = max(x, 0)
        let cy = max(y, 0)
        let cw = min(w, Double(image.width) - cx)
        let ch = min(h, Double(image.height) - cy)
        if cw > 1 && ch > 1,
           let cropped = image.cropping(to: CGRect(x: cx, y: cy, width: cw, height: ch)) {
            finalImage = cropped
        }
    }
    // Downscale so the width does not exceed maxWidth. P0-1: this MUST draw
    // `finalImage` (the window crop), never the original full-display `image`.
    // Drawing `image` here re-paints the whole desktop and silently breaks
    // target-window isolation for every crop that needs downscaling.
    if maxWidth > 0 && finalImage.width > maxWidth {
        let sourceImage = finalImage
        let scale = Double(maxWidth) / Double(sourceImage.width)
        let h = Int(Double(sourceImage.height) * scale)
        let ctx = CGContext(data: nil, width: maxWidth, height: h,
                            bitsPerComponent: 8, bytesPerRow: maxWidth * 4,
                            space: CGColorSpaceCreateDeviceRGB(),
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        ctx?.interpolationQuality = .high
        ctx?.draw(sourceImage, in: CGRect(x: 0, y: 0, width: maxWidth, height: h))
        if let scaled = ctx?.makeImage() {
            finalImage = scaled
        }
    }

    let rep = NSBitmapImageRep(cgImage: finalImage)
    let compression = Double(max(1, min(100, quality))) / 100.0
    let data: Data?
    if format == "jpeg" {
        data = rep.representation(using: .jpeg,
                                  properties: [.compressionFactor: compression])
    } else {
        data = rep.representation(using: .png, properties: [:])
    }
    guard let data else {
        throw NSError(domain: "CUBridge", code: 4,
                      userInfo: [NSLocalizedDescriptionKey: "Image encoding failed"])
    }
    try data.write(to: URL(fileURLWithPath: outputPath))

    var info: [String: Any] = [:]
    info["width"] = finalImage.width
    info["height"] = finalImage.height
    let frame = display.frame
    info["display_scale_factor"] = frame.width > 0 ? Double(display.width) / Double(frame.width) : 1.0
    info["bytes"] = data.count
    info["format"] = format
    info["overlay_excluded"] = !excludedWindows.isEmpty
    return info
}

// MARK: - Active app + window

func activeAppInfo() -> [String: Any] {
    guard let app = NSWorkspace.shared.frontmostApplication else { return [:] }
    var info: [String: Any] = [
        "bundle_id": app.bundleIdentifier ?? "unknown",
        "name": app.localizedName ?? "unknown",
    ]
    // P0-5: the strict focus guard compares bundle + pid + window. The frontmost
    // app's own pid is authoritative — a recycled pid under the same bundle is
    // NOT the target window.
    let pid = app.processIdentifier
    info["pid"] = pid
    // Window title via Accessibility (requires the Accessibility permission).
    let appElement = AXUIElementCreateApplication(pid)
    var title: CFTypeRef?
    let res = AXUIElementCopyAttributeValue(appElement, kAXFocusedWindowAttribute as CFString, &title)
    if res == .success, let window = title {
        var windowTitle: CFTypeRef?
        if AXUIElementCopyAttributeValue(window as! AXUIElement, kAXTitleAttribute as CFString, &windowTitle) == .success,
           let t = windowTitle as? String {
            info["window_title"] = t
        }
    }
    // Window id of the frontmost normal window owned by this pid.
    let opts: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
    if let list = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] {
        for w in list {
            let ownerPID = (w[kCGWindowOwnerPID as String] as? Int) ?? -1
            let layer = (w[kCGWindowLayer as String] as? Int) ?? Int.max
            if ownerPID == pid && layer == 0 {
                if let num = w[kCGWindowNumber as String] as? Int {
                    info["window_id"] = num
                }
                break
            }
        }
    }
    return info
}

// MARK: - Target window resolution (round 9 / P0-4)

/// Resolve a session target to a concrete window. Rules:
/// - window_id provided: verify it exists (PID/bundle must match if given).
/// - pid only: the app's frontmost normal window.
/// - bundle_id only: the app's frontmost visible normal window.
func resolveTarget(_ params: [String: Any]) -> [String: Any] {
    let windowID = params["window_id"] as? Int
    let pid = params["pid"] as? Int
    let bundleID = params["bundle_id"] as? String

    let opts: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
    guard let list = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] else {
        return ["found": false, "reason": "window list unavailable"]
    }

    // Build candidate windows: on-screen normal windows (exclude overlay).
    func isNormal(_ w: [String: Any]) -> Bool {
        let layer = (w[kCGWindowLayer as String] as? Int) ?? Int.max
        let alpha = (w[kCGWindowAlpha as String] as? Double) ?? 0.0
        let bounds = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
        let wdt = (bounds["Width"] as? Double) ?? 0
        let hgt = (bounds["Height"] as? Double) ?? 0
        return layer == 0 && alpha > 0.0 && wdt > 0 && hgt > 0
    }
    let windows = list.filter(isNormal)

    func windowEntry(_ w: [String: Any]) -> [String: Any]? {
        let ownerPID = (w[kCGWindowOwnerPID as String] as? Int) ?? 0
        let num = (w[kCGWindowNumber as String] as? Int) ?? 0
        let bounds = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
        guard let x = bounds["X"] as? Double,
              let y = bounds["Y"] as? Double,
              let wdt = bounds["Width"] as? Double,
              let hgt = bounds["Height"] as? Double else {
            return nil
        }
        // P0-4: the window's OWN identity bundle id (from the owner PID's
        // NSRunningApplication) — never the frontmost app's. The Focus Guard
        // and identity backfill depend on this being the target window's
        // owner, so it must NOT be borrowed from the active application.
        let bundleID = NSRunningApplication(processIdentifier: pid_t(ownerPID))?.bundleIdentifier
        return [
            "window_id": num,
            "pid": ownerPID,
            "bundle_id": bundleID ?? "unknown",
            "bounds": ["x": x, "y": y, "width": wdt, "height": hgt],
            "title": w[kCGWindowName as String] as? String ?? ""
        ]
    }

    // 1. window_id provided: exact match.
    if let wid = windowID {
        for w in windows {
            if (w[kCGWindowNumber as String] as? Int) == wid {
                let e = windowEntry(w)
                if let e = e {
                    // Verify PID/bundle if provided.
                    if let p = pid, e["pid"] as? Int != p {
                        return ["found": false, "reason": "pid_mismatch"]
                    }
                    if let b = bundleID {
                        if let app = NSRunningApplication(processIdentifier: pid_t(e["pid"] as! Int)),
                           app.bundleIdentifier != b {
                            return ["found": false, "reason": "bundle_mismatch"]
                        }
                    }
                    return ["found": true, "window": e]
                }
            }
        }
        return ["found": false, "reason": "window_not_found"]
    }

    // 2. pid only: frontmost normal window of that PID.
    if let p = pid {
        for w in windows {
            if (w[kCGWindowOwnerPID as String] as? Int) == p {
                if let e = windowEntry(w) {
                    return ["found": true, "window": e]
                }
            }
        }
        return ["found": false, "reason": "pid_no_window"]
    }

    // 3. bundle_id only: frontmost visible normal window of that bundle.
    if let b = bundleID {
        for w in windows {
            let ownerPID = (w[kCGWindowOwnerPID as String] as? Int) ?? 0
            if let app = NSRunningApplication(processIdentifier: pid_t(ownerPID)),
               app.bundleIdentifier == b {
                if let e = windowEntry(w) {
                    return ["found": true, "window": e]
                }
            }
        }
        return ["found": false, "reason": "bundle_no_window"]
    }

    return ["found": false, "reason": "no_target_specified"]
}

// MARK: - Hit test (round 8 / P0-1)

/// Topmost interactive window at a global point, in CGWindowList z-order
/// (back-to-front), so the FIRST normal window whose bounds contain the point
/// is the one that would receive a Direct CG click there. Rules mirror
/// `resolveTarget`'s `isNormal`: layer == 0, alpha > 0, wdt > 0, hgt > 0 —
/// overlay/menu-bar windows (layer > 0) and zero-size stubs never count.
/// Returns `["found": false, "reason": "no_window_at_point"]` when no normal
/// window contains the point.
func windowAtPoint(_ params: [String: Any]) -> [String: Any] {
    guard let x = params["x"] as? Double, let y = params["y"] as? Double else {
        return ["found": false, "reason": "requires x and y (Doubles)"]
    }
    let opts: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
    guard let list = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] else {
        return ["found": false, "reason": "window list unavailable"]
    }
    func isNormal(_ w: [String: Any]) -> Bool {
        let layer = (w[kCGWindowLayer as String] as? Int) ?? Int.max
        let alpha = (w[kCGWindowAlpha as String] as? Double) ?? 0.0
        let bounds = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
        let wdt = (bounds["Width"] as? Double) ?? 0
        let hgt = (bounds["Height"] as? Double) ?? 0
        return layer == 0 && alpha > 0.0 && wdt > 0 && hgt > 0
    }
    for w in list {
        guard isNormal(w) else { continue }
        let bounds = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
        guard let wx = bounds["X"] as? Double,
              let wy = bounds["Y"] as? Double,
              let wdt = bounds["Width"] as? Double,
              let hgt = bounds["Height"] as? Double else {
            continue
        }
        // The point is inside this window's bounds. Same coordinate space as
        // the bounds that `resolve_target` returns (global logical points), so
        // the runtime can compare hit-test results against the target window.
        if x >= wx && x < wx + wdt && y >= wy && y < wy + hgt {
            let ownerPID = (w[kCGWindowOwnerPID as String] as? Int) ?? 0
            let num = (w[kCGWindowNumber as String] as? Int) ?? 0
            let bundleID = NSRunningApplication(processIdentifier: pid_t(ownerPID))?.bundleIdentifier
            return [
                "found": true,
                "window": [
                    "window_id": num,
                    "pid": ownerPID,
                    "bundle_id": bundleID ?? "unknown",
                ],
            ]
        }
    }
    return ["found": false, "reason": "no_window_at_point"]
}

// MARK: - Permissions

func permissionStatus() -> [String: Any] {
    return [
        "screen_recording": CGPreflightScreenCaptureAccess(),
        "accessibility": AXIsProcessTrusted(),
    ]
}

// MARK: - Clipboard (for the clipboard text-input fallback)

func clipboardGet() -> String? {
    let pb = NSPasteboard.general
    guard let type = pb.types?.first, let data = pb.data(forType: type) else { return nil }
    let base64 = data.base64EncodedString()
    return "\(type.rawValue):\(base64)"
}

func clipboardSet(_ payload: String) -> Bool {
    let pb = NSPasteboard.general
    pb.clearContents()
    let parts = payload.split(separator: ":", maxSplits: 1).map(String.init)
    guard parts.count == 2 else { return false }
    let typeName = parts[0]
    guard let data = Data(base64Encoded: parts[1]) else { return false }
    let type = NSPasteboard.PasteboardType(typeName)
    let ok = pb.setData(data, forType: type)
    return ok
}

// MARK: - Command dispatch

func handle(_ method: String, _ params: [String: Any], id: Any) -> String {
    switch method {
    case "displays":
        return ok(id, ["displays": listDisplays()])
    case "capture":
        guard let displayStr = params["display"] as? String,
              let displayId = CGDirectDisplayID(displayStr),
              let output = params["output"] as? String else {
            return err(id, "capture requires display (string) and output (string)")
        }
        do {
            let info = try captureDisplay(
                displayId: displayId,
                outputPath: output,
                showsCursor: params["shows_cursor"] as? Bool ?? true,
                maxWidth: params["max_width"] as? Int ?? 0,
                format: params["format"] as? String ?? "png",
                quality: params["quality"] as? Int ?? 85,
                region: params["region"] as? [Double])
            return ok(id, info)
        } catch {
            let desc = (error as NSError).userInfo[NSLocalizedDescriptionKey] as? String ?? error.localizedDescription
            return err(id, desc)
        }
    case "cursor_overlay":
        return handleOverlay(params, id: id)
    case "active":
        return ok(id, activeAppInfo())
    case "resolve_target":
        return ok(id, resolveTarget(params))
    case "window_at_point":
        return ok(id, windowAtPoint(params))
    case "permissions":
        return ok(id, permissionStatus())
    case "clipboard_get":
        if let payload = clipboardGet() {
            return ok(id, ["payload": payload])
        }
        return ok(id, ["payload": ""])
    case "clipboard_set":
        let payload = params["payload"] as? String ?? ""
        return ok(id, ["set": clipboardSet(payload)])
    default:
        return err(id, "unknown method \(method)")
    }
}

// MARK: - Main loop

while let line = readLine() {
    guard let data = line.data(using: .utf8),
          let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
          let method = obj["method"] as? String else {
        print(jsonString(["ok": false, "error": "invalid request"]))
        fflush(stdout)
        continue
    }
    let id = obj["id"] ?? NSNull()
    let params = obj["params"] as? [String: Any] ?? [:]
    print(handle(method, params, id: id))
    fflush(stdout)
}
