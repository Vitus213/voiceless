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

/// fcitx::CapabilityFlag::Preedit | FormattedPreedit: the client renders
/// preedit inline (the case for GTK/Qt apps).
const PREEDIT_CAPABILITY: u64 = (1 << 1) | (1 << 4);

#[tokio::test]
async fn preedit_shows_inline_then_clears_before_the_commit() {
    if std::env::var_os("VOICELESS_FCITX5_BRIDGE_TEST").is_none() {
        eprintln!("skipped: set VOICELESS_FCITX5_BRIDGE_TEST=1");
        return;
    }
    let connection = zbus::Connection::session().await.expect("session bus");
    let reply = connection
        .call_method(
            Some("org.fcitx.Fcitx5"),
            FRONTEND_PATH,
            Some(FRONTEND_INTERFACE),
            "CreateInputContext",
            &(vec![("program", "voiceless-preedit-test")],),
        )
        .await
        .expect("create input context");
    let (context, _uuid): (zbus::zvariant::OwnedObjectPath, Vec<u8>) =
        reply.body().deserialize().expect("input context reply");
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(CONTEXT_INTERFACE)
        .unwrap()
        .path(context.as_str())
        .unwrap()
        .build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &connection, None)
        .await
        .expect("subscribe to context signals");
    for (method, body) in [
        ("SetCapability", Some(PREEDIT_CAPABILITY)),
        ("FocusIn", None),
    ] {
        let call = match body {
            Some(flags) => {
                connection
                    .call_method(
                        Some("org.fcitx.Fcitx5"),
                        context.as_str(),
                        Some(CONTEXT_INTERFACE),
                        method,
                        &(flags,),
                    )
                    .await
            }
            None => {
                connection
                    .call_method(
                        Some("org.fcitx.Fcitx5"),
                        context.as_str(),
                        Some(CONTEXT_INTERFACE),
                        method,
                        &(),
                    )
                    .await
            }
        };
        call.unwrap_or_else(|error| panic!("{method}: {error}"));
    }

    let client = voiceless::fcitx5::Fcitx5Client::spawn();
    client.preedit("你好");
    client.preedit("你好世界");
    // Speech takes time: let the live partial land before the final commit
    // (a commit queued in the same batch would supersede it unshown).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let reply = client.commit("你好世界。").await.expect("commit reply");
    assert!(reply.0, "commit failed: {reply:?}");

    // Collect UpdateFormattedPreedit texts and the commit, in order.
    let mut seen = Vec::new();
    while !seen
        .iter()
        .any(|entry: &String| entry.starts_with("commit:"))
    {
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), signals.next())
            .await
            .expect("signal within 2s")
            .expect("stream open")
            .expect("signal");
        let header = message.header();
        match header.member().map(|member| member.as_str()) {
            Some("UpdateFormattedPreedit") => {
                let (segments, _cursor): (Vec<(String, i32)>, i32) =
                    message.body().deserialize().expect("preedit body");
                seen.push(format!(
                    "preedit:{}",
                    segments
                        .into_iter()
                        .map(|(text, _)| text)
                        .collect::<String>()
                ));
            }
            Some("CommitString") => {
                let (text,): (String,) = message.body().deserialize().expect("commit body");
                seen.push(format!("commit:{text}"));
            }
            _ => {}
        }
    }
    let commit = seen.iter().position(|entry| entry == "commit:你好世界。");
    let shown = seen.iter().position(|entry| entry == "preedit:你好世界");
    let cleared = seen.iter().rposition(|entry| entry == "preedit:");
    assert!(shown.is_some(), "newest preedit shown: {seen:?}");
    assert!(
        cleared > shown && cleared < commit,
        "preedit cleared before the commit: {seen:?}"
    );
}
