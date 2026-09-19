use agent_modules::serve;
use anyhow::{bail, Context, Result};
use serde_json::json;

fn main() -> Result<()> {
    serve(|request| {
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
