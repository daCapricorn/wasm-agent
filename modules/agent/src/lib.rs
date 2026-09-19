use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

pub const LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Packet {
    #[serde(default)]
    pub from: String,
    pub to: String,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub body: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn read(input: &mut impl BufRead) -> Result<Option<Packet>> {
    let mut line = Vec::new();
    use std::io::Read;
    let count = input
        .take((LIMIT + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if count == 0 {
        return Ok(None);
    }
    if count > LIMIT || line.last() != Some(&b'\n') {
        bail!("invalid input frame");
    }
    let value: Value = serde_json::from_slice(&line)?;
    if value["version"] == 1 && value["command"] == "stop" {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value(value)?))
}

pub fn emit(packet: &Packet) -> Result<()> {
    let line = serde_json::to_vec(&json!({"version":1,"kind":"message","data":packet}))?;
    if line.len() + 1 > LIMIT {
        bail!("output frame exceeds limit");
    }
    let mut out = io::stdout().lock();
    out.write_all(&line)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

pub fn reply(request: &Packet, result: Result<Value>) -> Result<()> {
    let (body, error) = match result {
        Ok(body) => (body, None),
        Err(error) => (Value::Null, Some(error.to_string())),
    };
    emit(&Packet {
        from: String::new(),
        to: request.from.clone(),
        id: request.id.clone(),
        method: "reply".into(),
        body,
        error,
    })
}

pub fn serve(mut handler: impl FnMut(&Packet) -> Result<Value>) -> Result<()> {
    let mut input = io::stdin().lock();
    while let Some(packet) = read(&mut input)? {
        if packet.method == "cancel" {
            continue;
        }
        let result = handler(&packet);
        reply(&packet, result)?;
    }
    Ok(())
}

/// Deterministic fixture protocol: `add 2 3`, then `recall`.
pub fn scripted(messages: &[Value]) -> Result<Value> {
    let last = messages.last().context("missing messages")?;
    if last["role"] == "tool" {
        return Ok(
            json!({"role":"assistant","content":format!("result: {}", last["content"].as_str().unwrap_or(""))}),
        );
    }
    let text = last["content"].as_str().context("missing text")?;
    if text == "recall" {
        let previous = messages
            .iter()
            .rev()
            .find(|m| m["role"] == "assistant" && m["content"].is_string());
        return Ok(
            json!({"role":"assistant","content":previous.map(|m| m["content"].clone()).unwrap_or(json!("no previous answer"))}),
        );
    }
    if text == "unknown" {
        return Ok(json!({
            "role":"assistant",
            "content":null,
            "tool_calls":[{"id":"missing-1","type":"function","function":{"name":"missing","arguments":"{}"}}]
        }));
    }
    if text == "fail" {
        return Ok(json!({
            "role":"assistant",
            "content":null,
            "tool_calls":[{"id":"invalid-1","type":"function","function":{"name":"add","arguments":"{\"a\":\"bad\",\"b\":1}"}}]
        }));
    }
    let parts: Vec<_> = text.split_whitespace().collect();
    if parts.len() == 3 && parts[0] == "add" {
        let a: f64 = parts[1].parse()?;
        let b: f64 = parts[2].parse()?;
        return Ok(
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"sum-1","type":"function","function":{"name":"add","arguments":json!({"a":a,"b":b}).to_string()}}]}),
        );
    }
    Ok(json!({"role":"assistant","content":format!("echo: {text}")}))
}
