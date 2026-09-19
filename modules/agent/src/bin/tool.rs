use agent_modules::serve;
use anyhow::{bail, Context, Result};
use serde_json::json;

fn main() -> Result<()> {
    serve(|request| {
        if request.method == "describe" {
            return Ok(json!([
                {
                    "type": "function",
                    "function": {
                        "name": "add",
                        "description": "Add two numbers. Use this tool for arithmetic.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "a": {"type": "number"},
                                "b": {"type": "number"}
                            },
                            "required": ["a", "b"],
                            "additionalProperties": false
                        }
                    }
                }
            ]));
        }
        if request.method != "add" {
            bail!("unknown tool");
        }
        let a = request.body["a"].as_f64().context("a must be a number")?;
        let b = request.body["b"].as_f64().context("b must be a number")?;
        let sum = a + b;
        if !sum.is_finite() {
            bail!("sum must be finite");
        }
        Ok(json!(sum))
    })
}
