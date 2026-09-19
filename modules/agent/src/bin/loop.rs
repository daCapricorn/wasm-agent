use agent_modules::{emit, read, reply, Packet};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{self, BufRead};

struct Loop {
    sequence: u64,
    turn: String,
    caller: String,
}

impl Loop {
    fn call(
        &mut self,
        input: &mut impl BufRead,
        to: &str,
        method: &str,
        body: Value,
        cancellable: bool,
    ) -> Result<Value> {
        self.sequence += 1;
        let id = format!("{}-{}", self.turn, self.sequence);
        emit(&Packet {
            from: String::new(),
            to: to.into(),
            id: id.clone(),
            method: method.into(),
            body,
            error: None,
        })?;
        while let Some(packet) = read(input)? {
            if packet.method == "cancel"
                && packet.from == self.caller
                && packet.id == self.turn
                && cancellable
            {
                emit(&Packet {
                    from: String::new(),
                    to: to.into(),
                    id: id.clone(),
                    method: "cancel".into(),
                    body: Value::Null,
                    error: None,
                })?;
                bail!("cancelled");
            }
            if packet.method == "reply" && packet.id == id && packet.from == to {
                if let Some(error) = packet.error {
                    bail!("{to}: {error}");
                }
                return Ok(packet.body);
            }
            if packet.method == "turn" {
                reply(&packet, Err(anyhow::anyhow!("busy")))?;
            }
        }
        bail!("host stopped")
    }

    fn run(&mut self, input: &mut impl BufRead, request: &Packet) -> Result<Value> {
        let text = request.body["text"].as_str().context("missing text")?;
        let loaded = self.call(input, "state", "load", Value::Null, true)?;
        let mut messages = loaded.as_array().context("invalid state")?.clone();
        messages.push(json!({"role":"user","content":text}));
        let tools = json!([{"type":"function","function":{"name":"add","description":"Add two numbers. Use this tool for arithmetic.","parameters":{"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"],"additionalProperties":false}}}]);
        let mut tool_count = 0;
        for _ in 0..8 {
            let answer = self.call(
                input,
                "reasoner",
                "reason",
                json!({"messages":messages,"tools":tools}),
                true,
            )?;
            if answer["role"] != "assistant" {
                bail!("reasoner returned invalid role");
            }
            messages.push(answer.clone());
            let calls = answer
                .get("tool_calls")
                .and_then(Value::as_array)
                .filter(|v| !v.is_empty());
            if let Some(calls) = calls {
                if calls.len() > 4 {
                    bail!("too many tool calls");
                }
                for call in calls {
                    let id = call["id"].as_str().context("tool call missing id")?;
                    let function = &call["function"];
                    let name = function["name"].as_str().context("tool name missing")?;
                    let arguments: Value = serde_json::from_str(
                        function["arguments"]
                            .as_str()
                            .context("arguments must be JSON string")?,
                    )?;
                    // Tool selection and validation belong to this loop module.
                    let result = if name == "add" {
                        match self.call(input, "tool", name, arguments, true) {
                            Ok(value) => value,
                            Err(error) if error.to_string() == "cancelled" => return Err(error),
                            Err(error) => json!({"error":error.to_string()}),
                        }
                    } else {
                        json!({"error":"unknown tool"})
                    };
                    tool_count += 1;
                    messages.push(
                        json!({"role":"tool","tool_call_id":id,"content":result.to_string()}),
                    );
                }
            } else {
                let content = answer["content"]
                    .as_str()
                    .context("answer missing content")?;
                // Commit completed turns atomically. Once saving starts, completion wins cancellation.
                self.call(input, "state", "save", json!(messages), false)?;
                return Ok(
                    json!({"text":content,"tool_calls":tool_count,"messages":messages.len()}),
                );
            }
        }
        bail!("reasoning step limit exceeded")
    }
}

fn main() -> Result<()> {
    let mut input = io::stdin().lock();
    let mut agent = Loop {
        sequence: 0,
        turn: String::new(),
        caller: String::new(),
    };
    while let Some(request) = read(&mut input)? {
        if request.method != "turn" {
            continue;
        }
        agent.turn = request.id.clone();
        agent.caller = request.from.clone();
        let result = agent.run(&mut input, &request);
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
