use agent_modules::{emit, read, reply, Packet};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{self, BufRead};

fn reason(input: &mut impl BufRead, request: &Packet) -> Result<Value> {
    if request.method != "reason" {
        bail!("unknown reasoner method");
    }
    let prompt = format!(
        "You are the reasoning component of a modular agent. Do not use your own tools, shell, files, or browser. Respond ONLY with a JSON assistant message, no markdown. The supplied transcript is the entire conversation. For arithmetic you MUST request the supplied add tool; do not compute it yourself.\n\
         A tool request has this shape: {{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{{\"id\":\"call-1\",\"type\":\"function\",\"function\":{{\"name\":\"add\",\"arguments\":\"{{\\\"a\\\":2,\\\"b\\\":3}}\"}}}}]}}.\n\
         After receiving a tool result, answer with {{\"role\":\"assistant\",\"content\":\"your answer\"}}. Recall earlier results from the transcript when asked.\n\
         INPUT: {}", request.body
    );
    let id = format!("codex-{}", request.id);
    emit(&Packet {
        from: String::new(),
        to: "codex".into(),
        id: id.clone(),
        method: "exec".into(),
        body: json!({"stdin":prompt}),
        error: None,
    })?;
    while let Some(packet) = read(input)? {
        if packet.method == "cancel" && packet.from == request.from && packet.id == request.id {
            emit(&Packet {
                from: String::new(),
                to: "codex".into(),
                id: id.clone(),
                method: "cancel".into(),
                body: Value::Null,
                error: None,
            })?;
            bail!("cancelled");
        }
        if packet.method == "reply" && packet.from == "codex" && packet.id == id {
            if let Some(error) = packet.error {
                bail!("adapter: {error}");
            }
            let text = packet.body["stdout"]
                .as_str()
                .context("missing subprocess output")?;
            return serde_json::from_str(text.trim())
                .context("Codex returned invalid JSON assistant message");
        }
        if packet.method == "reason" {
            reply(&packet, Err(anyhow::anyhow!("busy")))?;
        }
    }
    bail!("host stopped")
}

fn main() -> Result<()> {
    let mut input = io::stdin().lock();
    while let Some(request) = read(&mut input)? {
        if request.method == "cancel" || request.method == "reply" {
            continue;
        }
        let result = reason(&mut input, &request);
        if result
            .as_ref()
            .is_err_and(|error| error.to_string() == "host stopped")
        {
            break;
        }
        reply(&request, result)?;
    }
    Ok(())
}
