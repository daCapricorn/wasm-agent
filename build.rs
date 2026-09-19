use std::{env, fs, path::Path, process::Command};

fn hash_sources(path: &Path, hash: &mut blake3::Hasher) {
    if path.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for entry in entries {
            hash_sources(&entry, hash);
        }
    } else {
        hash.update(path.to_string_lossy().as_bytes());
        hash.update(&fs::read(path).unwrap());
    }
}

fn main() {
    let mut hash = blake3::Hasher::new();
    for path in ["Cargo.lock", "Cargo.toml", "build.rs", "src"] {
        println!("cargo:rerun-if-changed={path}");
        hash_sources(Path::new(path), &mut hash);
    }
    for name in [
        "TARGET",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
        hash.update(name.as_bytes());
        hash.update(env::var(name).unwrap_or_default().as_bytes());
    }
    let mut flags: Vec<_> = env::vars()
        .filter(|(key, _)| key.starts_with("CARGO_FEATURE_") || key.starts_with("CARGO_CFG_"))
        .collect();
    flags.sort();
    for (key, value) in flags {
        hash.update(key.as_bytes());
        hash.update(value.as_bytes());
    }
    let rustc = Command::new(env::var_os("RUSTC").unwrap())
        .arg("-vV")
        .output()
        .unwrap();
    assert!(rustc.status.success());
    hash.update(&rustc.stdout);
    println!(
        "cargo:rustc-env=WASM_AGENT_BUILD_ID={}",
        hash.finalize().to_hex()
    );
}
