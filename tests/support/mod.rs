use serde_json::{json, Value};
use std::{
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub struct Feed {
    pub address: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Feed {
    pub fn start(address: &str, disconnect_once: bool, silent: bool) -> Self {
        let listener = TcpListener::bind(address).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut disconnected = false;
            while !flag.load(Ordering::Relaxed) {
                let (stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let Ok(mut socket) = tungstenite::accept(stream) else {
                    continue;
                };
                let Ok(message) = socket.read() else {
                    continue;
                };
                let Ok(subscription) =
                    serde_json::from_str::<Value>(message.to_text().unwrap_or(""))
                else {
                    continue;
                };
                assert_eq!(subscription["action"], "subscribe");
                let symbol = subscription["quotes"][0].as_str().unwrap_or("AAPL");
                let mut tick = 0;
                while !flag.load(Ordering::Relaxed) {
                    if !silent {
                        // Cross the threshold once, stay above, then drop below and repeat.
                        let mid = [99.0, 101.0, 102.0, 98.0][tick % 4];
                        let batch = json!([
                            {"T":"q","S":"IGNORED","bp":1.0,"ap":2.0,"t":"fixture"},
                            {"T":"q","S":symbol,"bp":mid-0.1,"ap":mid+0.1,"t":format!("fixture-{tick}")}
                        ]);
                        if socket
                            .send(tungstenite::Message::Text(batch.to_string().into()))
                            .is_err()
                        {
                            break;
                        }
                        tick += 1;
                        if disconnect_once && !disconnected && tick == 3 {
                            disconnected = true;
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self {
            address,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
