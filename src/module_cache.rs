//! Private, locally generated executable artifacts. Raw WASM remains the source of truth.
use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use wasmer::{Engine, Module};

const HEADER: &[u8] = b"wasm-agent-cache-v1\n";
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum CacheStatus {
    Hit,
    Compiled,
}

pub struct ModuleCache {
    root: PathBuf,
}

impl ModuleCache {
    pub fn new(root: PathBuf) -> Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root)?;
        let metadata = fs::symlink_metadata(&root)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("cache root must be a real directory");
        }
        check_private(&metadata)?;
        Ok(Self { root })
    }

    fn key(&self, engine: &Engine, wasm: &[u8]) -> Result<String> {
        let mut hash = blake3::Hasher::new();
        hash.update(HEADER);
        hash.update(env!("WASM_AGENT_BUILD_ID").as_bytes());
        hash.update(engine.deterministic_id().as_bytes());
        hash.update(format!("{:?}", engine.as_sys().target()).as_bytes());
        hash.update(wasm);
        Ok(hash.finalize().to_hex().to_string())
    }

    pub fn load(&self, engine: &Engine, path: &Path) -> Result<(Module, CacheStatus)> {
        let wasm = fs::read(path).with_context(|| format!("read module {}", path.display()))?;
        let key = self.key(engine, &wasm)?;
        let artifact_path = self.root.join(format!("{key}.artifact"));
        match self.read_artifact(&artifact_path) {
            Ok(Some(bytes)) => {
                // SAFETY: private owner-only cache; artifacts are produced by Module::serialize
                // with this build fingerprint for this target/engine and source hash. Integrity is
                // checked BEFORE deserialize (which is not an untrusted input validator).
                // Other processes running as this OS user are inside the trust boundary.
                match unsafe { Module::deserialize(engine, bytes) } {
                    Ok(module) => return Ok((module, CacheStatus::Hit)),
                    Err(_) => eprintln!(
                        "wasm cache: incompatible artifact, rebuilding {}",
                        path.display()
                    ),
                }
            }
            Ok(None) => {}
            Err(error) => return Err(error),
        }
        let module = Module::new(engine, wasm).context("compile WASM module")?;
        let bytes = module.serialize()?;
        if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
            bail!("compiled artifact exceeds cache size limit");
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(HEADER)?;
        file.write_all(blake3::hash(&bytes).as_bytes())?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        // Atomic replacement: concurrent writers can only publish complete equivalent artifacts.
        file.persist(&artifact_path)?;
        Ok((module, CacheStatus::Compiled))
    }

    fn read_artifact(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("open trusted artifact"),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("cached artifact is not a regular file");
        }
        check_private(&metadata)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_ARTIFACT_BYTES + HEADER.len() as u64 + 33)
            .read_to_end(&mut bytes)?;
        let prefix = HEADER.len() + 32;
        if bytes.len() < prefix
            || bytes.len() as u64 > MAX_ARTIFACT_BYTES + prefix as u64
            || !bytes.starts_with(HEADER)
            || blake3::hash(&bytes[prefix..]).as_bytes() != &bytes[HEADER.len()..prefix]
        {
            eprintln!("wasm cache: damaged artifact, rebuilding");
            return Ok(None);
        }
        Ok(Some(bytes.split_off(prefix)))
    }
}

fn check_private(metadata: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            bail!("executable cache must be owned by the current user and not writable by others");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        bail!("executable cache ownership checks are currently supported on Unix only");
    }
    Ok(())
}

pub fn load(engine: &Engine, path: &Path) -> Result<Module> {
    let root = match std::env::var_os("WASM_AGENT_CACHE_DIR") {
        Some(path) => PathBuf::from(path),
        None => {
            let home = PathBuf::from(
                std::env::var_os("HOME").context("HOME or WASM_AGENT_CACHE_DIR required")?,
            );
            if cfg!(target_os = "macos") {
                home.join("Library/Caches/wasm-agent/compiled-v1")
            } else {
                std::env::var_os("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".cache"))
                    .join("wasm-agent/compiled-v1")
            }
        }
    };
    let (module, status) = ModuleCache::new(root)?.load(engine, path)?;
    eprintln!(
        "wasm cache: {} {}",
        if status == CacheStatus::Hit {
            "hit"
        } else {
            "compiled"
        },
        path.display()
    );
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmer::{imports, Instance, Store};

    #[test]
    fn cache_hit_source_change_and_corruption_recovery() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = ModuleCache::new(temp.path().join("cache"))?;
        let source = temp.path().join("module.wasm");
        let engine = Engine::default();
        fs::write(
            &source,
            b"(module (func (export \"value\") (result i32) i32.const 42))",
        )?;
        let (_, status) = cache.load(&engine, &source)?;
        assert_eq!(status, CacheStatus::Compiled);
        let (module, status) = cache.load(&engine, &source)?;
        assert_eq!(status, CacheStatus::Hit);
        let mut store = Store::new(engine.clone());
        let instance = Instance::new(&mut store, &module, &imports! {})?;
        assert_eq!(
            instance
                .exports
                .get_typed_function::<(), i32>(&store, "value")?
                .call(&mut store)?,
            42
        );
        let path = cache.root.join(format!(
            "{}.artifact",
            cache.key(&engine, &fs::read(&source)?)?
        ));
        fs::write(path, b"corrupted")?;
        assert_eq!(cache.load(&engine, &source)?.1, CacheStatus::Compiled);
        fs::write(
            &source,
            b"(module (func (export \"value\") (result i32) i32.const 43))",
        )?;
        assert_eq!(cache.load(&engine, &source)?.1, CacheStatus::Compiled);
        assert_eq!(cache.load(&engine, &source)?.1, CacheStatus::Hit);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn rejects_writable_directory_and_symlink_artifact() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("cache");
        let cache = ModuleCache::new(root.clone())?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777))?;
        assert!(ModuleCache::new(root.clone()).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let source = temp.path().join("module.wasm");
        fs::write(&source, b"(module)")?;
        let engine = Engine::default();
        let artifact = root.join(format!(
            "{}.artifact",
            cache.key(&engine, &fs::read(&source)?)?
        ));
        symlink(&source, artifact)?;
        assert!(cache.load(&engine, &source).is_err());
        Ok(())
    }
}
