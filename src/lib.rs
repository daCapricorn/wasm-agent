//! Minimal long-running WASIX service host. Provider protocols live in guests.
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use wasmer::Module;
use wasmer_wasix::{
    bin_factory::spawn_exec_module,
    os::task::{process::WasiProcess, TaskJoinHandle},
    runtime::task_manager::tokio::TokioTaskManager,
    wasmer_wasix_types::wasi::Signal,
    Pipe, PluggableRuntime, Runtime, UnsupportedVirtualNetworking, WasiEnv,
};

pub const MAX_EVENT_BYTES: usize = 64 * 1024;
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub version: u32,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug)]
pub struct Config {
    pub module: PathBuf,
    pub env: Vec<(String, String)>,
    /// Explicit opt-in: this PoC grants unrestricted host networking.
    pub allow_network: bool,
}

pub struct Service {
    pub events: mpsc::Receiver<Result<Event>>,
    stdin: Pipe,
    task: TaskJoinHandle,
    process: WasiProcess,
    reader: tokio::task::JoinHandle<()>,
}

impl Service {
    pub async fn start(config: Config) -> Result<Self> {
        let mut runtime = PluggableRuntime::new(Arc::new(TokioTaskManager::default()));
        if !config.allow_network {
            runtime.set_networking_implementation(UnsupportedVirtualNetworking::default());
        }
        let runtime: Arc<dyn Runtime + Send + Sync> = Arc::new(runtime);
        let engine = runtime.engine().clone();
        // Compilation is synchronous; keep it off Tokio's async workers.
        let module = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&config.module)
                .with_context(|| format!("read module {}", config.module.display()))?;
            Module::new(&engine, bytes).context("compile WASM module")
        })
        .await??;
        let (stdin, guest_stdin) = Pipe::channel();
        let (guest_stdout, stdout) = Pipe::channel();
        let env = WasiEnv::builder("service")
            .runtime(runtime.clone())
            .envs(config.env)
            .stdin(Box::new(guest_stdin))
            .stdout(Box::new(guest_stdout))
            .build()?;
        let process = env.process.clone();
        let task = spawn_exec_module(module, env, &runtime)?;
        let (tx, events) = mpsc::channel(32);
        let reader = tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let result = (&mut stdout)
                    .take((MAX_EVENT_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line)
                    .await;
                let event = match result {
                    Ok(0) => break,
                    Ok(_) if line.len() > MAX_EVENT_BYTES => Err(anyhow!("event exceeds 64 KiB")),
                    Ok(_) if line.last() != Some(&b'\n') => Err(anyhow!("unterminated event")),
                    Ok(_) => decode_event(&line),
                    Err(e) => Err(e.into()),
                };
                let failed = event.is_err();
                if tx.send(event).await.is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            events,
            stdin,
            task,
            process,
            reader,
        })
    }

    pub async fn wait(&mut self) -> Result<i32> {
        self.task
            .wait_finished()
            .await
            .map(|code| code.raw())
            .map_err(|e| anyhow!("WASIX service failed: {e}"))
    }

    /// Signals are observed at runtime boundaries, not arbitrary CPU instructions.
    pub async fn stop(&mut self, grace: Duration) -> Result<()> {
        self.stdin
            .write_all(b"{\"version\":1,\"command\":\"stop\"}\n")
            .await?;
        match tokio::time::timeout(grace, self.wait()).await {
            Ok(result) => {
                let code = result?;
                if code != 0 {
                    bail!("service exited with status {code}");
                }
                Ok(())
            }
            Err(_) => {
                self.process.signal_process(Signal::Sigkill);
                let _ = tokio::time::timeout(grace, self.wait()).await;
                bail!("service did not stop cooperatively within {grace:?}; SIGKILL requested")
            }
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.process.signal_process(Signal::Sigkill);
        self.reader.abort();
    }
}

fn decode_event(line: &[u8]) -> Result<Event> {
    let event: Event = serde_json::from_slice(line).context("invalid service event")?;
    if event.version != 1 || event.kind.is_empty() {
        bail!("unsupported event version or empty kind");
    }
    Ok(event)
}
