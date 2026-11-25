import AppKit
import ApplicationServices

// A native editor owned by the test, for Voiceless's delivery. Its fields are
// ordinary AppKit controls; nothing here reads another application. One
// window with three fields: two to tell apart, one protected.
//
//   editor-serve.swift    runs the window as an application
//   editor-guard.swift    runs the delivery guard of Voiceless's bridge, linked in

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
            let field: NSTextField = index == 2 ? NSSecureTextField(frame: rect) : NSTextField(string: "")
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

    /// The field being edited, if any.
    var editing: Int? { fields.firstIndex { $0.currentEditor() != nil } }

    /// What the outside may know: the texts, which field edits, whether the window is in front.
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

/// Let the application handle what was asked of it.
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
