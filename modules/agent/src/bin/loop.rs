use agent_modules::{emit, read, reply, AgentEvent, Packet};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{self, BufRead};

struct Loop {
    sequence: u64,
    turn: String,
    caller: String,
    events: bool,
    event_sequence: u64,
}

impl Loop {
    fn event(&mut self, kind: &str, data: Value) -> Result<()> {
        if !self.events {
            return Ok(());
        }
        self.event_sequence += 1;
        let event = AgentEvent {
            version: 1,
            sequence: self.event_sequence,
            turn_id: self.turn.clone(),
            kind: kind.into(),
            data,
        };
        emit(&Packet {
            from: String::new(),
            to: "cli".into(),
            id: self.turn.clone(),
            method: "event".into(),
            body: serde_json::to_value(event)?,
            error: None,
        })
    }

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
        self.event("agent_start", json!({"turn_id": self.turn.clone(), "text": text}))?;
        self.event("turn_start", json!({"turn_id": self.turn}))?;
        let loaded = self.call(input, "state", "load", Value::Null, true)?;
        let mut messages = loaded.as_array().context("invalid state")?.clone();
        messages.push(json!({"role":"user","content":text}));
        // Tool modules own their manifest; the loop only dispatches declared names.
        let tools = self.call(input, "tool", "describe", Value::Null, true)?;
        let tool_specs = tools.as_array().context("tool manifest must be an array")?;
        let max_turns = request.body["max_turns"].as_u64().unwrap_or(8);
        let max_tool_calls = request.body["max_tool_calls"].as_u64().unwrap_or(4);
        if !(1..=64).contains(&max_turns) || !(1..=32).contains(&max_tool_calls) {
            bail!("invalid loop limits");
        }
        let mut tool_count = 0;
        for turn in 0..max_turns {
            self.event("message_start", json!({"turn": turn + 1}))?;
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
                if calls.len() as u64 > max_tool_calls {
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
                    self.event(
                        "tool_execution_start",
                        json!({"tool_call_id": id, "name": name, "arguments": arguments.clone()}),
                    )?;
                    let declared = tool_specs.iter().any(|tool| {
                        tool["type"] == "function" && tool["function"]["name"] == name
                    });
                    let result = if declared {
                        match self.call(input, "tool", name, arguments, true) {
                            Ok(value) => value,
                            Err(error) if error.to_string() == "cancelled" => return Err(error),
                            Err(error) => json!({"error":error.to_string()}),
                        }
                    } else {
                        json!({"error":"unknown tool"})
                    };
                    self.event(
                        "tool_execution_end",
                        json!({"tool_call_id": id, "name": name, "result": result.clone()}),
                    )?;
                    tool_count += 1;
                    messages.push(
                        json!({"role":"tool","tool_call_id":id,"content":result.to_string()}),
                    );
                }
            } else {
                let content = answer["content"]
                    .as_str()
                    .context("answer missing content")?;
                self.event("message_end", json!({"turn": turn + 1}))?;
                // Commit completed turns atomically. Once saving starts, completion wins cancellation.
                self.call(input, "state", "save", json!(messages), false)?;
                self.event("turn_end", json!({"tool_calls": tool_count}))?;
                self.event("agent_end", json!({"messages": messages.len()}))?;
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
        events: false,
        event_sequence: 0,
    };
    while let Some(request) = read(&mut input)? {
        if request.method != "turn" {
            continue;
        }
        agent.turn = request.id.clone();
        agent.caller = request.from.clone();
        agent.events = request.body["events"].as_bool().unwrap_or(false);
        agent.event_sequence = 0;
        let result = agent.run(&mut input, &request);
        if let Err(error) = &result {
            agent.event("error", json!({"message": error.to_string()}))?;
        }
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
