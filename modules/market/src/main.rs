use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tungstenite::{client::IntoClientRequest, stream::MaybeTlsStream, Message, WebSocket};

fn emit(kind: &str, data: Value) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{}", json!({"version":1,"kind":kind,"data":data}))?;
    out.flush()?;
    Ok(())
}
struct Config {
    url: String,
    symbol: String,
    above: Option<f64>,
    source: String,
}
impl Config {
    fn load() -> Result<Self> {
        let source = std::env::var("MARKET_SOURCE").unwrap_or_else(|_| "mock".into());
        if source != "mock" && source != "alpaca" {
            bail!("MARKET_SOURCE must be mock or alpaca");
        }
        let default_url = if source == "alpaca" {
            "wss://stream.data.alpaca.markets/v2/iex"
        } else {
            "ws://127.0.0.1:18765"
        };
        let url = std::env::var("MARKET_URL").unwrap_or_else(|_| default_url.into());
        if source == "alpaca" && !url.starts_with("wss://stream.data.alpaca.markets/") {
            bail!("Alpaca credentials may only be sent to wss://stream.data.alpaca.markets/");
        }
        let above = std::env::var("MARKET_ABOVE")
            .ok()
            .map(|v| v.parse::<f64>())
            .transpose()?;
        if above.is_some_and(|v| !v.is_finite() || v <= 0.0) {
            bail!("invalid MARKET_ABOVE");
        }
        Ok(Self {
            source,
            url,
            above,
            symbol: std::env::var("MARKET_SYMBOL").unwrap_or_else(|_| "AAPL".into()),
        })
    }
}
fn connect(config: &Config) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let request = config.url.as_str().into_client_request()?;
    let uri = request.uri();
    let host = uri.host().context("missing host")?;
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let mut stream = None;
    for address in (host, port).to_socket_addrs()? {
        if let Ok(socket) = TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
            stream = Some(socket);
            break;
        }
    }
    let stream = stream.context("connection failed")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(64 * 1024))
        .max_frame_size(Some(64 * 1024));
    let (socket, _) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map_err(|_| anyhow::anyhow!("WebSocket/TLS handshake failed"))?;
    match socket.get_ref() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(Duration::from_millis(200)))?,
        MaybeTlsStream::Rustls(s) => s.sock.set_read_timeout(Some(Duration::from_millis(200)))?,
        _ => bail!("unsupported transport"),
    }
    Ok(socket)
}
fn send(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, value: Value) -> Result<()> {
    socket.send(Message::Text(value.to_string().into()))?;
    Ok(())
}
fn session(config: &Config, stop: &AtomicBool) -> Result<()> {
    let mut socket = connect(config)?;
    emit(
        "status",
        json!({"state":"connected","source":config.source}),
    )?;
    if config.source == "alpaca" {
        send(
            &mut socket,
            json!({"action":"auth", "key":std::env::var("ALPACA_API_KEY")?, "secret":std::env::var("ALPACA_API_SECRET")?}),
        )?;
    } else {
        send(
            &mut socket,
            json!({"action":"subscribe","quotes":[config.symbol]}),
        )?;
    }
    let mut authenticated = config.source == "mock";
    let started = Instant::now();
    let mut was_above = false;
    while !stop.load(Ordering::Relaxed) {
        if !authenticated && started.elapsed() > Duration::from_secs(5) {
            bail!("authentication timed out");
        }
        let message = match socket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => return Err(e.into()),
        };
        match message {
            Message::Text(text) => {
                let batch: Vec<Value> = serde_json::from_str(&text)?;
                for item in batch {
                    match item["T"].as_str() {
                        Some("error") => bail!("provider error code {}", item["code"]),
                        Some("success") if item["msg"] == "authenticated" => {
                            authenticated = true;
                            send(
                                &mut socket,
                                json!({"action":"subscribe","quotes":[config.symbol]}),
                            )?;
                        }
                        Some("q") if authenticated && item["S"] == config.symbol => {
                            let bid = item["bp"].as_f64().context("quote missing bid")?;
                            let ask = item["ap"].as_f64().context("quote missing ask")?;
                            if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask < bid {
                                bail!("invalid quote prices");
                            }
                            let mid = bid / 2.0 + ask / 2.0;
                            let data = json!({"source":config.source,"symbol":config.symbol,"bid":bid,"ask":ask,"mid":mid,"timestamp":item["t"]});
                            emit("quote", data.clone())?;
                            if let Some(threshold) = config.above {
                                let above = mid >= threshold;
                                if above && !was_above {
                                    emit(
                                        "threshold_crossed",
                                        json!({"quote":data,"threshold":threshold}),
                                    )?;
                                }
                                was_above = above;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Close(_) => bail!("provider closed connection"),
            Message::Ping(_) => {
                socket.flush()?;
            }
            _ => {}
        }
    }
    let _ = socket.close(None);
    Ok(())
}
fn main() -> Result<()> {
    let config = Config::load()?;
    if config.source == "alpaca" {
        std::env::var("ALPACA_API_KEY").context("missing ALPACA_API_KEY")?;
        std::env::var("ALPACA_API_SECRET").context("missing ALPACA_API_SECRET")?;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let control = stop.clone();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line
                .ok()
                .and_then(|line| serde_json::from_str::<Value>(&line).ok())
            {
                Some(value) if value["version"] == 1 && value["command"] == "stop" => break,
                _ => continue,
            }
        }
        control.store(true, Ordering::Relaxed);
    });
    emit(
        "status",
        json!({"state":"starting","source":config.source,"symbol":config.symbol}),
    )?;
    let mut attempt = 0_u32;
    while !stop.load(Ordering::Relaxed) {
        if let Err(error) = session(&config, &stop) {
            eprintln!("market session: {error}");
            if stop.load(Ordering::Relaxed) {
                break;
            }
            attempt = attempt.saturating_add(1);
            // Authentication payloads are never logged.
            emit("status", json!({"state":"reconnecting","attempt":attempt}))?;
            let until = Instant::now() + Duration::from_millis(250 * (1_u64 << attempt.min(4)));
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    emit("status", json!({"state":"stopped"}))?;
    Ok(())
}
