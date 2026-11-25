import AppKit

// The native editor fixture as an application of the guest's screen.
//
//   editor serve <directory>
//
// status.json mirrors the fields as they change; command.json, as
// {"id": n, "field": 0 | 1 | 2 | null}, brings one field forward to edit or
// hides the application; a file named stop ends it.

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

@main
struct Fixture {
    static func main() {
        let arguments = CommandLine.arguments
        guard arguments.count == 3, arguments[1] == "serve" else {
            fail("usage: editor serve <directory>")
        }
        serve(URL(fileURLWithPath: arguments[2], isDirectory: true))
    }
}
