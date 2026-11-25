import AppKit
import ApplicationServices

// The delivery guard of Voiceless's own macOS bridge, linked in: text may only
// reach the field that had focus when the delivery was asked for, a protected
// field is no target, and the clipboard is put back afterwards.
//
//   editor guard <directory>

/// The identity of the focused input as Voiceless reads it; 0 when there is none.
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
guard arguments.count == 3, arguments[1] == "guard" else {
    fail("usage: editor guard <directory>")
}
guardCheck("Voiceless 中文 Ω")
