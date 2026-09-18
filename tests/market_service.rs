mod support;
use std::{path::PathBuf, time::Duration};
use wasm_agent::{Config, Event, Service};

async fn start(address: &str, network: bool) -> Service {
    let module = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("modules/market/target/wasm32-wasmer-wasi/release/market.wasm");
    assert!(
        module.exists(),
        "build the guest first: bash scripts/build-market.sh"
    );
    Service::start(Config {
        module,
        allow_network: network,
        env: vec![
            ("MARKET_URL".into(), format!("ws://{address}")),
            ("MARKET_ABOVE".into(), "100".into()),
        ],
    })
    .await
    .unwrap()
}
async fn next(service: &mut Service) -> Event {
    tokio::time::timeout(Duration::from_secs(15), service.events.recv())
        .await
        .expect("event timeout")
        .expect("event stream ended")
        .expect("invalid event")
}

#[tokio::test(flavor = "multi_thread")]
async fn receives_filters_reconnects_and_stops_real_wasm() {
    tokio::time::timeout(
        Duration::from_secs(30),
        receives_filters_reconnects_and_stops_real_wasm_scenario(),
    )
    .await
    .expect("scenario exceeded 30 seconds");
}

async fn receives_filters_reconnects_and_stops_real_wasm_scenario() {
    let feed = support::Feed::start("127.0.0.1:0", true, false);
    let mut service = start(&feed.address, true).await;
    let mut quotes = 0;
    let mut triggers = 0;
    let mut connections = 0;
    let mut reconnecting = false;
    while quotes < 7 {
        let event = next(&mut service).await;
        match event.kind.as_str() {
            "quote" => {
                assert_eq!(event.data["symbol"], "AAPL");
                assert_eq!(event.data["source"], "mock");
                quotes += 1;
            }
            "threshold_crossed" => {
                triggers += 1;
                assert!(event.data["quote"]["mid"].as_f64().unwrap() >= 100.0);
            }
            "status" if event.data["state"] == "connected" => connections += 1,
            "status" if event.data["state"] == "reconnecting" => reconnecting = true,
            _ => {}
        }
    }
    assert!(reconnecting && connections >= 2);
    assert_eq!(triggers, 2, "one trigger per crossing, not per quote");
    service.stop(Duration::from_secs(3)).await.unwrap();
    assert_eq!(service.wait().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stops_while_websocket_is_idle() {
    tokio::time::timeout(
        Duration::from_secs(30),
        stops_while_websocket_is_idle_scenario(),
    )
    .await
    .expect("scenario exceeded 30 seconds");
}

async fn stops_while_websocket_is_idle_scenario() {
    let feed = support::Feed::start("127.0.0.1:0", false, true);
    let mut service = start(&feed.address, true).await;
    while next(&mut service).await.data["state"] != "connected" {}
    tokio::time::sleep(Duration::from_millis(300)).await;
    service.stop(Duration::from_secs(3)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn network_is_denied_without_explicit_capability() {
    tokio::time::timeout(
        Duration::from_secs(30),
        network_is_denied_without_explicit_capability_scenario(),
    )
    .await
    .expect("scenario exceeded 30 seconds");
}

async fn network_is_denied_without_explicit_capability_scenario() {
    let feed = support::Feed::start("127.0.0.1:0", false, false);
    let mut service = start(&feed.address, false).await;
    loop {
        let event = next(&mut service).await;
        assert_ne!(event.kind, "quote");
        assert_ne!(event.data["state"], "connected");
        if event.data["state"] == "reconnecting" {
            break;
        }
    }
    service.stop(Duration::from_secs(3)).await.unwrap();
}
