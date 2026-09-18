use anyhow::{bail, Context, Result};
use clap::Parser;
use std::{path::PathBuf, time::Duration};
use wasm_agent::{Config, Service};

#[derive(Parser)]
#[command(about = "Run a long-lived WASIX capability and receive JSON events")]
struct Args {
    #[arg(
        long,
        default_value = "modules/market/target/wasm32-wasmer-wasi/release/market.wasm"
    )]
    module: PathBuf,
    /// Grant unrestricted host networking to this trusted PoC module.
    #[arg(long)]
    allow_network: bool,
    /// Forward only this named environment variable; repeat as needed.
    #[arg(long = "env")]
    env_names: Vec<String>,
    /// Stop after this many seconds; 0 means run until Ctrl-C.
    #[arg(long, default_value_t = 10)]
    duration: u64,
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run(Args::parse()));
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

async fn run(args: Args) -> Result<()> {
    let env = args
        .env_names
        .into_iter()
        .map(|name| {
            let value = std::env::var(&name)
                .with_context(|| format!("missing environment variable {name}"))?;
            Ok((name, value))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut service = Service::start(Config {
        module: args.module,
        env,
        allow_network: args.allow_network,
    })
    .await?;
    eprintln!("service started; Ctrl-C to stop");
    let deadline = async {
        if args.duration == 0 {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(Duration::from_secs(args.duration)).await;
    };
    tokio::pin!(deadline);
    let outcome: Result<()> = loop {
        tokio::select! {
            _ = &mut deadline => break Ok(()),
            result = tokio::signal::ctrl_c() => break result.map_err(Into::into),
            event = service.events.recv() => match event {
                Some(Ok(event)) => println!("{}", serde_json::to_string(&event)?),
                Some(Err(error)) => break Err(error),
                None => {
                    let code = service.wait().await?;
                    bail!("service ended unexpectedly with status {code}");
                }
            }
        }
    };
    let stop = service.stop(Duration::from_secs(3)).await;
    outcome?;
    stop?;
    eprintln!("service stopped (exit 0)");
    Ok(())
}
