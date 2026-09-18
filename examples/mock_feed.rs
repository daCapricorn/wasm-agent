#[path = "../tests/support/mod.rs"]
mod support;
fn main() {
    let feed = support::Feed::start("127.0.0.1:18765", false, false);
    println!("Synthetic quotes at ws://{}; Ctrl-C to exit", feed.address);
    loop {
        std::thread::park();
    }
}
