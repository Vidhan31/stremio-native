use fslock::LockFile;
use stream_server::{ServerConfig, run};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    let lock_path = std::env::temp_dir().join("stream-server.lock");
    let mut lockfile = LockFile::open(&lock_path)?;
    if !lockfile.try_lock()? {
        eprintln!("Exiting, another instance is running.");
        return Ok(());
    }

    let runtime = tokio::runtime::Runtime::new()?;
    let (_shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    runtime.block_on(run(ServerConfig::binary_default(), shutdown_rx, None))?;
    Ok(())
}
