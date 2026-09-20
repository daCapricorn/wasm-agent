use anyhow::Result;
use serde_json::json;
use std::time::Duration;
use wasm_agent::composition::{Composition, Packet, Session};

fn config(name: &str) -> Composition {
    Composition::load(format!(
        "{}/configs/agent-{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

async fn receive(session: &mut Session) -> Packet {
    tokio::time::timeout(Duration::from_secs(15), session.events.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

async fn turn(session: &mut Session, id: &str, text: &str) -> Packet {
    session
        .send(Packet::request(
            &session.entry,
            id,
            "turn",
            json!({"text":text}),
        ))
        .await
        .unwrap();
    let answer = receive(session).await;
    assert_eq!(answer.id, id);
    answer
}

async fn event_turn(
    session: &mut Session,
    id: &str,
    text: &str,
) -> (Packet, Vec<String>, Vec<u64>) {
    session
        .send(Packet::request(
            &session.entry,
            id,
            "turn",
            json!({"text":text, "events":true}),
        ))
        .await
        .unwrap();
    let mut kinds = Vec::new();
    let mut sequences = Vec::new();
    loop {
        let packet = receive(session).await;
        assert_eq!(packet.id, id);
        if packet.method == "event" {
            kinds.push(packet.body["type"].as_str().unwrap().to_owned());
            assert_eq!(packet.body["version"], 1);
            assert_eq!(packet.body["turn_id"], id);
            sequences.push(packet.body["sequence"].as_u64().unwrap());
        } else {
            return (packet, kinds, sequences);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn independent_wasm_modules_call_tool_and_recall_state() {
    let mut session = Session::start(config("scripted")).await.unwrap();
    let rejected = turn(&mut session, "invalid", "add nope 3").await;
    assert!(rejected.error.is_some());
    let answer = turn(&mut session, "first", "add 2 3").await;
    assert!(answer.error.is_none(), "{answer:?}");
    assert_eq!(answer.body["text"], "result: 5.0");
    assert_eq!(answer.body["tool_calls"], 1);
    assert_eq!(answer.body["messages"], 4);
    let recalled = turn(&mut session, "second", "recall").await;
    assert_eq!(recalled.body["text"], answer.body["text"]);
    assert_eq!(recalled.body["messages"], 6);
    let mut saw_tool = false;
    let mut saw_manifest = false;
    while let Ok(route) = session.traces.try_recv() {
        saw_tool |= route.from == "loop" && route.to == "tool" && route.method == "add";
        saw_manifest |= route.from == "loop" && route.to == "tool" && route.method == "describe";
    }
    assert!(saw_tool, "must execute the real tool WASM");
    assert!(
        saw_manifest,
        "loop must discover tools from the tool manifest"
    );
    session.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn tau_style_events_cover_tool_turn_and_error_paths() {
    let mut session = Session::start(config("scripted")).await.unwrap();
    let (answer, events, sequences) = event_turn(&mut session, "events", "add 2 3").await;
    assert!(answer.error.is_none(), "{answer:?}");
    assert_eq!(answer.body["text"], "result: 5.0");
    assert_eq!(
        events,
        [
            "agent_start",
            "turn_start",
            "message_start",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
    assert_eq!(sequences, (1..=9).collect::<Vec<_>>());

    let (unknown, unknown_events, _) = event_turn(&mut session, "unknown", "unknown").await;
    assert!(unknown.error.is_none(), "{unknown:?}");
    assert!(unknown.body["text"]
        .as_str()
        .unwrap()
        .contains("unknown tool"));
    assert!(unknown_events
        .iter()
        .any(|kind| kind == "tool_execution_end"));

    let (failed, failed_events, _) = event_turn(&mut session, "failed-tool", "fail").await;
    assert!(failed.error.is_none(), "{failed:?}");
    assert!(failed.body["text"]
        .as_str()
        .unwrap()
        .contains("a must be a number"));
    assert!(failed_events
        .iter()
        .any(|kind| kind == "tool_execution_end"));
    session.stop().await.unwrap();
}

fn fixture() -> Composition {
    let mut config = config("codex");
    let process = config.processes.get_mut("codex").unwrap();
    process.command = "/bin/sh".into();
    process.args = vec!["-c".into(), r#"input=$(cat)
case "$input" in
  *'"role":"tool"'*) printf '%s' '{"role":"assistant","content":"fixture result 5"}' ;;
  *) printf '%s' '{"role":"assistant","content":null,"tool_calls":[{"id":"fixture-call","type":"function","function":{"name":"add","arguments":"{\"a\":2,\"b\":3}"}}]}' ;;
esac"#.into()];
    process.env = vec!["PATH".into()];
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_wasm_bridge_uses_same_loop_state_and_tool() {
    let mut session = Session::start(fixture()).await.unwrap();
    let answer = turn(&mut session, "one", "add 2 3").await;
    assert!(answer.error.is_none(), "{answer:?}");
    assert_eq!(answer.body["tool_calls"], 1);
    assert_eq!(answer.body["text"], "fixture result 5");
    let answer = turn(&mut session, "two", "recall").await;
    assert_eq!(answer.body["messages"], 6);
    assert_eq!(answer.body["text"], "fixture result 5");
    session.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_terminates_process_and_does_not_commit_turn() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("started");
    let mut config = fixture();
    // First invocation blocks; the next returns a valid answer. No network/credentials needed.
    config.processes.get_mut("codex").unwrap().args = vec!["-c".into(),
        r#"if test ! -f "$1"; then printf '%s' "$$" > "$1"; exec sleep 30; else cat >/dev/null; printf '%s' '{"role":"assistant","content":"recovered"}'; fi"#.into(),
        "fixture".into(), marker.to_string_lossy().into_owned()];
    let mut session = Session::start(config).await?;
    session
        .send(Packet::request(
            &session.entry,
            "cancel-me",
            "turn",
            json!({"text":"slow"}),
        ))
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let pid = std::fs::read_to_string(&marker)?;
    session
        .send(Packet::request(
            &session.entry,
            "cancel-me",
            "cancel",
            json!(null),
        ))
        .await?;
    let cancelled = tokio::time::timeout(Duration::from_secs(2), receive(&mut session)).await?;
    assert_eq!(cancelled.error.as_deref(), Some("cancelled"));
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::process::Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let answer = turn(&mut session, "next", "hello").await;
    assert!(answer.error.is_none(), "{answer:?}");
    assert_eq!(answer.body["text"], "recovered");
    assert_eq!(
        answer.body["messages"], 2,
        "cancelled turn must not pollute state"
    );
    session.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn subprocess_failure_reaches_cli() {
    let mut config = fixture();
    config.processes.get_mut("codex").unwrap().command = "/usr/bin/false".into();
    config.processes.get_mut("codex").unwrap().args.clear();
    let mut session = Session::start(config).await.unwrap();
    let result = turn(&mut session, "broken", "hello").await;
    assert!(result.error.is_some());
    session.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn route_permissions_are_enforced() {
    let mut config = config("scripted");
    config
        .modules
        .get_mut("loop")
        .unwrap()
        .routes
        .retain(|route| route != "state");
    let mut session = Session::start(config).await.unwrap();
    session
        .send(Packet::request(
            &session.entry,
            "denied",
            "turn",
            json!({"text":"hi"}),
        ))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(result.unwrap_err().to_string().contains("route denied"));
    assert!(session.stop().await.is_err());
}
