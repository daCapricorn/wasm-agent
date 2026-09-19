use agent_modules::serve;
use anyhow::{bail, Result};
use serde_json::{json, Value};

fn main() -> Result<()> {
    let mut messages = Vec::<Value>::new();
    serve(|request| match request.method.as_str() {
        "load" => Ok(json!(messages)),
        "save" => {
            let next = request
                .body
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("expected messages"))?;
            if next.len() > 128 {
                bail!("conversation limit reached (128 messages)");
            }
            messages = next.clone();
            Ok(json!({"saved":true}))
        }
        _ => bail!("unknown state method"),
    })
}
