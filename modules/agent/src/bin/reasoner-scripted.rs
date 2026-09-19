use agent_modules::{scripted, serve};
use anyhow::{bail, Context, Result};

fn main() -> Result<()> {
    serve(|request| {
        if request.method != "reason" {
            bail!("unknown reasoner method");
        }
        scripted(
            request.body["messages"]
                .as_array()
                .context("missing messages")?,
        )
    })
}
