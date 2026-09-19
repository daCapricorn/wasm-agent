use anyhow::{Context, Result};
use clap::Parser;
use serde_json::json;
use std::{collections::VecDeque, io::BufRead, path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use wasm_agent::composition::{Composition, Packet, Session};

#[derive(Parser)]
#[command(about = "Compose WASM modules into a terminal agent")]
struct Args {
    #[arg(long, default_value = "configs/agent-scripted.json")]
    config: PathBuf,
    #[arg(long)]
    trace: bool,
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
    let mut session = Session::start(Composition::load(args.config)?).await?;
    let (sender, mut lines) = mpsc::channel(32);
    // Dedicated thread: blocking terminal stdin must not keep Tokio shutdown waiting.
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            if sender.blocking_send(line).is_err() {
                break;
            }
        }
    });
    eprintln!("Ready. Enter a prompt; /cancel cancels the active turn; /quit exits.");
    let mut queue = VecDeque::new();
    let mut active: Option<String> = None;
    let mut next_id = 0;
    let mut eof = false;
    let outcome: Result<()> = async {
        loop {
            if active.is_none() {
                if let Some(text) = queue.pop_front() {
                    next_id += 1;
                    let id = format!("turn-{next_id}");
                    session.send(Packet::request(&session.entry, &id, "turn", json!({"text":text}))).await?;
                    active = Some(id);
                } else if eof { break; }
            }
            tokio::select! {
                Some(route) = session.traces.recv(), if args.trace => {
                    eprintln!("{} -> {} [{}] {}", route.from, route.to, route.id, route.method);
                },
                line = lines.recv(), if !eof => match line {
                    Some(Ok(line)) if line == "/quit" => break,
                    Some(Ok(line)) if line == "/cancel" => {
                        if let Some(id) = &active { session.send(Packet::request(&session.entry, id, "cancel", json!(null))).await?; }
                    }
                    Some(Ok(line)) if !line.trim().is_empty() => {
                        if queue.len() < 32 { queue.push_back(line); } else { eprintln!("Input queue full"); }
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => eof = true,
                    _ => {},
                },
                _ = tokio::signal::ctrl_c() => {
                    if let Some(id) = &active { session.send(Packet::request(&session.entry, id, "cancel", json!(null))).await?; }
                    else { break; }
                },
                event = session.events.recv() => {
                    let packet = event.context("session ended")??;
                    if active.as_ref() == Some(&packet.id) {
                        println!("{}", serde_json::to_string(&packet)?);
                        active = None;
                    }
                }
            }
        }
        Ok(())
    }.await;
    let stopped = session.stop().await;
    outcome?;
    stopped
}
