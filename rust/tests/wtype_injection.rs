//! Contract tests for the wtype injection argument builder: newlines become
//! Shift+Enter so a chat input breaks lines without submitting the message.
//! Pure function under test (`stream::wtype_invocations`); no process is
//! spawned.

use voiceless::stream::wtype_invocations;

#[test]
fn newlines_become_shift_enter() {
    assert_eq!(
        wtype_invocations("第一行\n第二行"),
        vec![
            vec!["--", "第一行"],
            vec!["-M", "shift", "-k", "Return"],
            vec!["--", "第二行"],
        ]
    );
}

#[test]
fn single_line_needs_no_enter() {
    assert_eq!(wtype_invocations("一行"), vec![vec!["--", "一行"]]);
}

#[test]
fn blank_lines_are_preserved() {
    assert_eq!(
        wtype_invocations("a\n\nb"),
        vec![
            vec!["--", "a"],
            vec!["-M", "shift", "-k", "Return"],
            vec!["--", ""],
            vec!["-M", "shift", "-k", "Return"],
            vec!["--", "b"],
        ]
    );
}
