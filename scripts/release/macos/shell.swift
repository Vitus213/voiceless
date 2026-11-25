import AppKit
import ApplicationServices

// The desktop shell of the installed candidate, seen from outside: window
// numbers, layers and sizes, the runtime's own identity and the status item's
// menu bar. No titles and no contents are read. Apart from hiding the
// application and starting the second launcher, two things a person does from
// the Dock and the Finder, nothing is touched.
//
//   shell <Voiceless.app/Contents/MacOS/voiceless-desktop> <pid of its host> <console-endpoint.json>

let arguments = CommandLine.arguments
guard arguments.count == 4, let pid = Int32(arguments[2]) else {
    print("usage: shell <executable in the bundle> <pid of its host> <console-endpoint.json>")
    exit(64)
}
let executable = URL(fileURLWithPath: arguments[1])
let descriptor = URL(fileURLWithPath: arguments[3])

func runtime() -> String? {
    let object = try? JSONSerialization.jsonObject(with: Data(contentsOf: descriptor))
    return (object as? [String: Any])?["runtime_id"] as? String
}

/// The app's ordinary windows: layer 0 and wide enough to be the console.
func windows() -> [(number: Int, onscreen: Bool)] {
    let rows = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []
    return rows.compactMap { row in
        guard row[kCGWindowOwnerPID as String] as? Int32 == pid,
              row[kCGWindowLayer as String] as? Int == 0,
              let rect = row[kCGWindowBounds as String] as? [String: Double], rect["Width", default: 0] > 800,
              let number = row[kCGWindowNumber as String] as? Int else { return nil }
        return (number, row[kCGWindowIsOnscreen as String] as? Bool ?? false)
    }
}

func observe(_ stage: String, _ visible: Bool, seconds: TimeInterval) -> Int? {
    let deadline = Date(timeIntervalSinceNow: seconds)
    repeat {
        if let found = windows().first(where: { $0.onscreen == visible }) {
            return found.number
        }
        Thread.sleep(forTimeInterval: 0.1)
    } while Date() < deadline
    print("\(stage): no \(visible ? "onscreen" : "hidden") window, saw \(windows())")
    return nil
}

var report: [String: Any] = [:]
let initial = observe("login start", false, seconds: 15)
let original = runtime()
let opener = Process()
opener.executableURL = URL(fileURLWithPath: "/usr/bin/open")
opener.arguments = [executable.path]
try opener.run()
opener.waitUntilExit()
let shown = observe("Launch Services reopen", true, seconds: 10)
let app = NSRunningApplication(processIdentifier: pid)!
report["hide_accepted"] = app.hide()
let hidden = observe("app hidden", false, seconds: 6)
let secondary = Process()
secondary.executableURL = executable
try secondary.run()
let limit = Date(timeIntervalSinceNow: 15)
while secondary.isRunning && Date() < limit { Thread.sleep(forTimeInterval: 0.1) }
if secondary.isRunning { secondary.terminate() }
report["second_launcher"] = secondary.isRunning ? "stayed resident" : "exited \(secondary.terminationStatus)"
let reopened = observe("second launcher", true, seconds: 10)
var attribute: CFTypeRef?
let status = AXUIElementCopyAttributeValue(AXUIElementCreateApplication(pid), kAXExtrasMenuBarAttribute as CFString, &attribute)
report["status_item"] = status == .success && attribute != nil
report["status_item_error"] = status.rawValue
report["window"] = ["kept_hidden_at_login_start": initial != nil, "reopened_by_launch_services": shown != nil,
                    "hidden_on_request": hidden != nil, "reopened_by_second_launcher": reopened != nil,
                    "one_window": initial != nil && initial == shown && shown == reopened, "number": initial ?? 0]
report["runtime_kept"] = original != nil && original == runtime()
report["passed"] = [initial != nil, shown != nil, hidden != nil, reopened != nil, initial == shown, shown == reopened,
                    report["status_item"] as! Bool, report["runtime_kept"] as! Bool,
                    (report["second_launcher"] as! String).hasPrefix("exited")].allSatisfy { $0 }
let data = try! JSONSerialization.data(withJSONObject: report, options: [.sortedKeys])
print(String(data: data, encoding: .utf8)!)
exit((report["passed"] as! Bool) ? 0 : 1)
