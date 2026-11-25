import AppKit
import ApplicationServices
import AVFoundation
import Foundation

// Read-only observation of the login session and of what this process may
// do. Preflight calls only, so nothing here can show a consent dialog.
let session = (CGSessionCopyCurrentDictionary() as? [String: Any]) ?? [:]
let screen = NSScreen.screens.first?.frame ?? .zero
let onScreen = (CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]]) ?? []
let report: [String: Any] = [
    "on_console": session["kCGSSessionOnConsoleKey"] as? Bool ?? false,
    "locked": session["CGSSessionScreenIsLocked"] as? Bool ?? false,
    "accessibility": AXIsProcessTrusted(),
    "listen": CGPreflightListenEventAccess(),
    "post": CGPreflightPostEventAccess(),
    "microphone": AVCaptureDevice.authorizationStatus(for: .audio).rawValue,
    "screen": ["width": Int(screen.width), "height": Int(screen.height)],
    "frontmost": NSWorkspace.shared.frontmostApplication?.bundleIdentifier ?? "",
    "windows_on_screen": onScreen.count,
    // Numbers and layers only: no titles and no contents.
    "window_layers": onScreen.compactMap { ($0[kCGWindowLayer as String] as? Int) ?? nil }.sorted(),
]
let data = try! JSONSerialization.data(withJSONObject: report, options: [.sortedKeys])
print(String(data: data, encoding: .utf8)!)
