import AppKit
import ApplicationServices

// A native editor owned by the test, for Voiceless's delivery. Its fields are
// ordinary AppKit controls; nothing here reads another application.
//
//   editor serve <directory>   one window with three fields. status.json mirrors them,
//                              command.json steers them, a file named stop ends it.
//   editor guard <directory>   the delivery guard of Voiceless's own bridge, linked in:
//                              text may only reach the field that had focus when it was asked for.

final class Editor: NSObject {
    let window = NSWindow(contentRect: NSRect(x: 160, y: 160, width: 560, height: 230),
                          styleMask: [.titled, .closable], backing: .buffered, defer: false)
    let fields: [NSTextField]
    var applied = 0

    override init() {
        let captions = ["First field", "Second field", "Protected field"]
        var inputs: [NSTextField] = []
        var labels: [NSTextField] = []
        for (index, caption) in captions.enumerated() {
            let rect = NSRect(x: 24, y: 142 - 62 * index, width: 512, height: 28)
            let field: NSTextField = index == 2 ? NSSecureTextField(frame: rect)
                                                : NSTextField(string: "", target: nil, action: nil)
            field.frame = rect
            field.placeholderString = "empty"
            let title = NSTextField(labelWithString: caption)
            title.frame = NSRect(x: 24, y: 176 - 62 * index, width: 300, height: 18)
            inputs.append(field)
            labels.append(title)
        }
        fields = inputs
        super.init()
        window.title = "Voiceless native editor fixture"
        window.isReleasedWhenClosed = false
        for control in labels + inputs {
            window.contentView?.addSubview(control)
        }
    }

    /// Bring the window forward with one field editing, or hide the application (`nil`).
    func show(_ index: Int?) {
        let app = NSApplication.shared
        if index == nil {
            app.hide(nil)
            return
        }
        app.unhide(nil)
        app.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        if let index, fields.indices.contains(index) {
            window.makeFirstResponder(fields[index])
            fields[index].selectText(nil)
        }
    }

    var editing: Int? { fields.firstIndex { $0.currentEditor() != nil } }

    func state() -> [String: Any] {
        ["fields": fields.map(\.stringValue),
         "editing": editing.map { $0 as Any } ?? NSNull(),
         "key": window.isKeyWindow,
         "visible": window.isVisible,
         "frontmost": NSWorkspace.shared.frontmostApplication?.processIdentifier == ProcessInfo.processInfo.processIdentifier,
         "accessibility": AXIsProcessTrusted(),
         "post": CGPreflightPostEventAccess(),
         "applied": applied]
    }
}

func settle(_ seconds: TimeInterval) {
    RunLoop.current.run(until: Date(timeIntervalSinceNow: seconds))
}

func emit(_ value: [String: Any]) {
    let data = try! JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    print(String(data: data, encoding: .utf8)!)
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

/// The window, editing its first field, as an application of the guest's screen.
func launch() -> Editor {
    let app = NSApplication.shared
    app.setActivationPolicy(.regular)
    app.finishLaunching()
    let editor = Editor()
    editor.show(0)
    return editor
}

func serve(_ directory: URL) -> Never {
    let app = NSApplication.shared
    let editor = launch()
    var last = Data()
    let timer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { _ in
        if let command = try? Data(contentsOf: directory.appendingPathComponent("command.json")),
           let order = (try? JSONSerialization.jsonObject(with: command)) as? [String: Any],
           let identifier = order["id"] as? Int, identifier != editor.applied {
            editor.applied = identifier
            editor.show(order["field"] as? Int)
            last = Data()
        }
        let state = try! JSONSerialization.data(withJSONObject: editor.state(), options: [.sortedKeys])
        if state != last {
            try? state.write(to: directory.appendingPathComponent("status.json"))
            last = state
        }
        if FileManager.default.fileExists(atPath: directory.appendingPathComponent("stop").path) {
            app.terminate(nil)
        }
    }
    RunLoop.main.add(timer, forMode: .common)
    app.run()
    fail("the fixture window was closed")
}

/// The identity of the focused input as Voiceless reads it.
func focusKey() -> UInt64 {
    var bundle: UnsafeMutablePointer<CChar>?, title: UnsafeMutablePointer<CChar>?
    let key = vl_focus(&bundle, &title)
    for pointer in [bundle, title] {
        if let pointer { free(pointer) }
    }
    return key
}

func paste(_ text: String, into key: UInt64) -> Int32 {
    let bytes = Array(text.utf8)
    return vl_paste(bytes, bytes.count, key)
}

func guardCheck(_ text: String) -> Never {
    let editor = launch()
    let fields = editor.fields
    var report: [String: Any] = ["text": text]
    editor.show(0)
    settle(0.6)
    let first = focusKey()
    // Another field took focus: the stale identity must be refused.
    editor.show(1)
    settle(0.6)
    report["refused_after_focus_change"] = paste("must never arrive", into: first)
    settle(0.5)
    report["second_after_refusal"] = fields[1].stringValue
    report["first_after_refusal"] = fields[0].stringValue
    // The original field has focus again: the text arrives, the clipboard is put back.
    editor.show(0)
    settle(0.6)
    let pasteboard = NSPasteboard.general
    pasteboard.clearContents()
    pasteboard.setString("unchanged clipboard", forType: .string)
    report["delivered"] = paste(text, into: first)
    settle(0.7)
    report["first_after_delivery"] = fields[0].stringValue
    report["second_after_delivery"] = fields[1].stringValue
    report["clipboard_after_delivery"] = pasteboard.string(forType: .string) ?? ""
    // A protected field is never read as a target: Voiceless sees no identity for it,
    // and its own caller refuses to paste without one.
    editor.show(2)
    settle(0.6)
    let protected = focusKey()
    report["protected_focus_key"] = protected
    report["protected_before"] = fields[2].stringValue
    // Evidence for a hardening note only: what the bridge entry point does with that identity.
    report["bridge_result_for_key_zero"] = paste("key zero probe", into: 0)
    settle(0.5)
    report["protected_after_bridge_call_with_key_zero"] = fields[2].stringValue
    report["fixture_accessibility"] = AXIsProcessTrusted()
    report["fixture_post"] = CGPreflightPostEventAccess()
    emit(report)
    exit(0)
}

let arguments = CommandLine.arguments
guard arguments.count == 3 else {
    fail("usage: editor serve <directory> | editor guard <directory>")
}
let directory = URL(fileURLWithPath: arguments[2], isDirectory: true)
switch arguments[1] {
case "serve": serve(directory)
case "guard": guardCheck("Voiceless 中文 Ω")
default: fail("unknown mode \(arguments[1])")
}
