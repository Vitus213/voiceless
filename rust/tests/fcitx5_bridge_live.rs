//! End-to-end check against a running Fcitx5 with the Voiceless bridge
//! module. Opt-in with `VOICELESS_FCITX5_BRIDGE_TEST=1`; `just bridge-test`
//! runs it inside a private D-Bus session so the desktop's Fcitx5 and the
//! focused window are never touched.

use futures_util::StreamExt;
use voiceless::config::InjectConfig;
use voiceless::stream::inject_text;

const FRONTEND_PATH: &str = "/org/freedesktop/portal/inputmethod";
const FRONTEND_INTERFACE: &str = "org.fcitx.Fcitx.InputMethod1";
const CONTEXT_INTERFACE: &str = "org.fcitx.Fcitx.InputContext1";

#[tokio::test]
async fn commit_reaches_the_focused_input_context() {
    if std::env::var_os("VOICELESS_FCITX5_BRIDGE_TEST").is_none() {
        eprintln!("skipped: set VOICELESS_FCITX5_BRIDGE_TEST=1");
        return;
    }
    let connection = zbus::Connection::session().await.expect("session bus");

    // Play the role of an application: create and focus an input context
    // through Fcitx5's D-Bus frontend.
    let reply = connection
        .call_method(
            Some("org.fcitx.Fcitx5"),
            FRONTEND_PATH,
            Some(FRONTEND_INTERFACE),
            "CreateInputContext",
            &(vec![("program", "voiceless-test")],),
        )
        .await
        .expect("create input context");
    let (context, _uuid): (zbus::zvariant::OwnedObjectPath, Vec<u8>) =
        reply.body().deserialize().expect("input context reply");

    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(CONTEXT_INTERFACE)
        .unwrap()
        .member("CommitString")
        .unwrap()
        .build();
    let mut commits = zbus::MessageStream::for_match_rule(rule, &connection, None)
        .await
        .expect("subscribe to CommitString");

    connection
        .call_method(
            Some("org.fcitx.Fcitx5"),
            context.as_str(),
            Some(CONTEXT_INTERFACE),
            "FocusIn",
            &(),
        )
        .await
        .expect("focus input context");

    let config = InjectConfig {
        prefer: "fcitx5".to_owned(),
        clipboard_fallback: false,
        ..InjectConfig::default()
    };
    let result = inject_text(&config, "你好 voiceless\n第二行").await;
    assert!(result.ok, "bridge commit failed: {result:?}");
    assert_eq!(result.method, "fcitx5");

    let message = tokio::time::timeout(std::time::Duration::from_secs(2), commits.next())
        .await
        .expect("CommitString signal within 2s")
        .expect("signal stream open")
        .expect("signal message");
    let (text,): (String,) = message.body().deserialize().expect("commit text");
    assert_eq!(text, "你好 voiceless\n第二行");
}
