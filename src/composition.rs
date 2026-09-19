//! Generic message routing and configured subprocess capabilities; no agent policy.
use crate::{Config, Event, Service, MAX_EVENT_BYTES};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Packet {
    pub fn request(to: &str, id: &str, method: &str, body: Value) -> Self {
        Self {
            from: "cli".into(),
            to: to.into(),
            id: id.into(),
            method: method.into(),
            body,
            error: None,
        }
    }
    fn response(&self, body: Value, error: Option<String>) -> Self {
        Self {
            from: self.to.clone(),
            to: self.from.clone(),
            id: self.id.clone(),
            method: "reply".into(),
            body,
            error,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleConfig {
    pub path: String,
    pub routes: Vec<String>,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Composition {
    pub entry: String,
    pub modules: BTreeMap<String, ModuleConfig>,
    #[serde(default)]
    pub processes: BTreeMap<String, ProcessConfig>,
}

impl Composition {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().canonicalize()?;
        let mut config: Self = serde_json::from_slice(&std::fs::read(&path)?)?;
        for module in config.modules.values_mut() {
            module.path = path
                .parent()
                .unwrap()
                .join(&module.path)
                .to_string_lossy()
                .into_owned();
        }
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> Result<()> {
        if !self.modules.contains_key(&self.entry) {
            bail!("entry module missing");
        }
        if self.modules.contains_key("cli") || self.processes.contains_key("cli") {
            bail!("cli is reserved");
        }
        for (name, module) in &self.modules {
            if self.processes.contains_key(name) {
                bail!("duplicate endpoint {name}");
            }
            for to in &module.routes {
                if to != "cli" && !self.modules.contains_key(to) && !self.processes.contains_key(to)
                {
                    bail!("unknown route {name} -> {to}");
                }
            }
        }
        for process in self.processes.values() {
            if process.timeout_seconds == 0 || process.timeout_seconds > 300 {
                bail!("invalid process timeout");
            }
        }
        Ok(())
    }
}

enum Input {
    Message(Packet),
    Stop,
}

#[derive(Debug)]
pub struct Route {
    pub from: String,
    pub to: String,
    pub id: String,
    pub method: String,
}

pub struct Session {
    pub events: mpsc::Receiver<Result<Packet>>,
    pub traces: mpsc::Receiver<Route>,
    commands: mpsc::Sender<Input>,
    task: Option<JoinHandle<Result<()>>>,
    pub entry: String,
}

impl Session {
    pub async fn start(config: Composition) -> Result<Self> {
        Self::start_with_loader(config, crate::module_cache::load).await
    }

    /// Keep routing/lifecycle identical when comparing compiler and artifact loaders.
    pub async fn start_with_loader(
        config: Composition,
        loader: impl Fn(&wasmer::Engine, &Path) -> Result<wasmer::Module> + Send + Sync + 'static,
    ) -> Result<Self> {
        let loader = std::sync::Arc::new(loader);
        config.validate()?;
        let mut services = BTreeMap::new();
        // Load every module before accepting input. Partial startup drops already loaded services.
        for (name, module) in &config.modules {
            let loader = loader.clone();
            let env = module
                .env
                .iter()
                .map(|key| {
                    Ok((
                        key.clone(),
                        std::env::var(key).with_context(|| format!("missing {key}"))?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            services.insert(
                name.clone(),
                Service::start_with_loader(
                    Config {
                        module: module.path.clone().into(),
                        env,
                        allow_network: module.network,
                    },
                    move |engine, path| loader(engine, path),
                )
                .await?,
            );
        }
        let (commands, inputs) = mpsc::channel(32);
        let (output, events) = mpsc::channel(32);
        let (trace, traces) = mpsc::channel(128);
        let entry = config.entry.clone();
        let task = tokio::spawn(run(config, services, inputs, output, trace));
        Ok(Self {
            events,
            traces,
            commands,
            task: Some(task),
            entry,
        })
    }
    pub async fn send(&self, packet: Packet) -> Result<()> {
        if serde_json::to_vec(&packet)?.len() + 1 > MAX_EVENT_BYTES {
            bail!("input frame too large");
        }
        self.commands
            .send(Input::Message(packet))
            .await
            .map_err(|_| anyhow!("session stopped"))
    }
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.commands.send(Input::Stop).await;
        self.task.take().unwrap().await?
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn run(
    config: Composition,
    mut services: BTreeMap<String, Service>,
    mut inputs: mpsc::Receiver<Input>,
    output: mpsc::Sender<Result<Packet>>,
    trace: mpsc::Sender<Route>,
) -> Result<()> {
    let (incoming, mut messages) = mpsc::channel::<(String, Result<Event>)>(32);
    let mut readers = JoinSet::new();
    for (name, service) in &mut services {
        let mut events = std::mem::replace(&mut service.events, mpsc::channel(1).1);
        let sender = incoming.clone();
        let name = name.clone();
        readers.spawn(async move {
            while let Some(event) = events.recv().await {
                if sender.send((name.clone(), event)).await.is_err() {
                    return;
                }
            }
            let _ = sender
                .send((name, Err(anyhow!("module output closed"))))
                .await;
        });
    }
    drop(incoming);
    let mut jobs = JoinSet::<(String, String, Packet)>::new();
    let mut active = BTreeMap::<(String, String), tokio::task::AbortHandle>::new();
    let outcome: Result<()> = async {
        loop {
            let packet = tokio::select! {
                input = inputs.recv() => match input {
                    Some(Input::Message(mut packet)) => {
                        packet.from = "cli".into();
                        if packet.to != config.entry { bail!("CLI may only address configured entry"); }
                        packet
                    }
                    _ => break,
                },
                message = messages.recv() => {
                    let (source, event) = message.context("all modules exited")?;
                    let event = event.with_context(|| format!("module {source}"))?;
                    if event.kind != "message" { bail!("module {source} emitted invalid event kind"); }
                    let mut packet: Packet = serde_json::from_value(event.data)?;
                    packet.from = source.clone();
                    if !config.modules[&source].routes.contains(&packet.to) { bail!("route denied: {source} -> {}", packet.to); }
                    packet
                },
                completion = jobs.join_next(), if !jobs.is_empty() => {
                    match completion {
                        Some(Ok((source, id, packet))) => {
                            if active.remove(&(source, id)).is_none() { continue; }
                            packet
                        }
                        Some(Err(error)) if error.is_cancelled() => continue,
                        Some(Err(error)) => return Err(error.into()),
                        None => continue,
                    }
                }
            };
            if packet.id.is_empty() || packet.method.is_empty() { bail!("empty message id or method"); }
            let _ = trace.try_send(Route { from:packet.from.clone(), to:packet.to.clone(), id:packet.id.clone(), method:packet.method.clone() });
            if packet.to == "cli" {
                output.send(Ok(packet)).await.map_err(|_| anyhow!("CLI disconnected"))?;
            } else if let Some(service) = services.get_mut(&packet.to) {
                service.send(&serde_json::to_value(packet)?).await?;
            } else if let Some(process) = config.processes.get(&packet.to) {
                let key = (packet.from.clone(), packet.id.clone());
                if packet.method == "cancel" {
                    if let Some(handle) = active.remove(&key) { handle.abort(); }
                    continue;
                }
                if packet.method != "exec" { bail!("unsupported process adapter method"); }
                if !active.is_empty() { // Bound subprocess concurrency for this PoC.
                    let error = packet.response(Value::Null, Some("process adapter busy".into()));
                    services.get_mut(&error.to).context("unknown process caller")?.send(&serde_json::to_value(error)?).await?;
                    continue;
                }
                let process = process.clone();
                let handle = jobs.spawn(async move {
                    let result = tokio::time::timeout(Duration::from_secs(process.timeout_seconds), execute(&process, &packet.body)).await;
                    let response = match result {
                        Ok(Ok(body)) => packet.response(body, None),
                        Ok(Err(error)) => packet.response(Value::Null, Some(error.to_string())),
                        Err(_) => packet.response(Value::Null, Some("process timed out".into())),
                    };
                    (packet.from, packet.id, response)
                });
                active.insert(key, handle);
            } else { bail!("unknown destination"); }
        }
        Ok(())
    }.await;
    if let Err(error) = &outcome {
        let _ = output.try_send(Err(anyhow!("{error:#}")));
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    readers.abort_all();
    let mut stops = JoinSet::new();
    for (_, mut service) in services {
        stops.spawn(async move { service.stop(Duration::from_secs(3)).await });
    }
    let mut stop_error = None;
    while let Some(result) = stops.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                stop_error = Some(error);
            }
            Err(error) => {
                stop_error = Some(error.into());
            }
        }
    }
    outcome?;
    if let Some(error) = stop_error {
        return Err(error);
    }
    Ok(())
}

async fn execute(config: &ProcessConfig, body: &Value) -> Result<Value> {
    let text = body["stdin"]
        .as_str()
        .context("process stdin must be text")?;
    let directory = tempfile::tempdir()?;
    let mut command = tokio::process::Command::new(&config.command);
    command
        .args(&config.args)
        .current_dir(directory.path())
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for key in &config.env {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().context("launch configured process")?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let write = async move {
        stdin.write_all(text.as_bytes()).await?;
        drop(stdin);
        Ok::<_, std::io::Error>(())
    };
    let read = async {
        let mut bytes = Vec::new();
        stdout
            .take((MAX_EVENT_BYTES / 2 + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > MAX_EVENT_BYTES / 2 {
            bail!("process output exceeds limit");
        }
        Ok::<_, anyhow::Error>(bytes)
    };
    let (_, bytes) = tokio::try_join!(async { write.await.map_err(anyhow::Error::from) }, read)?;
    let status = child.wait().await?;
    if !status.success() {
        bail!("process exited with {status}");
    }
    Ok(json!({"stdout":String::from_utf8(bytes)?}))
}
