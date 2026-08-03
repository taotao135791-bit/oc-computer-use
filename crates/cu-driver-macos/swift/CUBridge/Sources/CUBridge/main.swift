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
                    quality: Int) throws -> [String: Any] {
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

    let filter = SCContentFilter(display: display, excludingWindows: [])
    let bridge = CaptureBridge()

    DispatchQueue.main.async {
        SCScreenshotManager.captureImage(contentFilter: filter, configuration: config) { image, error in
            bridge.image = image
            bridge.error = error
            bridge.semaphore.signal()
        }
    }
    waitForSemaphore(bridge.semaphore)

    if let error = bridge.error {
        throw error
    }
    guard let image = bridge.image else {
        throw NSError(domain: "CUBridge", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "Capture produced no image"])
    }

    var finalImage = image
    // Downscale so the width does not exceed maxWidth.
    if maxWidth > 0 && image.width > maxWidth {
        let scale = Double(maxWidth) / Double(image.width)
        let h = Int(Double(image.height) * scale)
        let ctx = CGContext(data: nil, width: maxWidth, height: h,
                            bitsPerComponent: 8, bytesPerRow: maxWidth * 4,
                            space: CGColorSpaceCreateDeviceRGB(),
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        ctx?.interpolationQuality = .high
        ctx?.draw(image, in: CGRect(x: 0, y: 0, width: maxWidth, height: h))
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
    return info
}

// MARK: - Active app + window

func activeAppInfo() -> [String: Any] {
    guard let app = NSWorkspace.shared.frontmostApplication else { return [:] }
    var info: [String: Any] = [
        "bundle_id": app.bundleIdentifier ?? "unknown",
        "name": app.localizedName ?? "unknown",
    ]
    // Window title via Accessibility (requires the Accessibility permission).
    let pid = app.processIdentifier
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
    return info
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
                quality: params["quality"] as? Int ?? 85)
            return ok(id, info)
        } catch {
            let desc = (error as NSError).userInfo[NSLocalizedDescriptionKey] as? String ?? error.localizedDescription
            return err(id, desc)
        }
    case "active":
        return ok(id, activeAppInfo())
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
