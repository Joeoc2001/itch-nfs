use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use itch_nfs::fs::{Config, ItchFs};
use itch_nfs::itch::{DEFAULT_API_URL, ItchClient};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};

/// Read-only NFSv3 server exposing the itch.io asset packs you own.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Address to serve NFS (and the MOUNT protocol) on.
    #[arg(long, env = "ITCH_NFS_LISTEN", default_value = "0.0.0.0:11111")]
    listen: String,

    /// itch.io OAuth access token or API key. Prefer the environment variable or --token-file.
    #[arg(long, env = "ITCH_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// File containing the itch.io token.
    #[arg(long, env = "ITCH_TOKEN_FILE", conflicts_with = "token")]
    token_file: Option<PathBuf>,

    /// Durable directory for the version history. Keep this: it is what makes pinned paths stable.
    #[arg(long, env = "ITCH_NFS_STATE_DIR", default_value = "./itch-nfs-state")]
    state_dir: PathBuf,

    /// Directory for downloaded archives [default: <system temp dir>/itch-nfs].
    #[arg(long, env = "ITCH_NFS_ARCHIVE_DIR")]
    archive_dir: Option<PathBuf>,

    /// Maximum total size of decompressed assets held in memory, in MiB.
    #[arg(long, env = "ITCH_NFS_CACHE_MAX_MB", default_value_t = 1024)]
    cache_max_mb: u64,

    /// Seconds an unused decompressed asset stays in memory.
    #[arg(long, env = "ITCH_NFS_CACHE_TTL", default_value_t = 600)]
    cache_ttl: u64,

    /// Refuse uploads that decompress to more than this many GiB (archive bombs).
    #[arg(long, env = "ITCH_NFS_MAX_UNPACKED_GB", default_value_t = 100)]
    max_unpacked_gb: u64,

    /// Seconds between checks for new packs and new versions.
    #[arg(long, env = "ITCH_NFS_REFRESH_INTERVAL", default_value_t = 900)]
    refresh_interval: u64,

    /// itch.io API base URL.
    #[arg(long, env = "ITCH_NFS_API_URL", default_value = DEFAULT_API_URL)]
    api_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nfsserve=warn".into()),
        )
        .init();
    let args = Args::parse();

    let token = match (args.token, args.token_file) {
        (Some(t), _) => t,
        (None, Some(f)) => {
            std::fs::read_to_string(&f).with_context(|| format!("read {}", f.display()))?
        }
        (None, None) => bail!("no itch.io token: set ITCH_TOKEN or pass --token-file"),
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("the itch.io token is empty");
    }

    let cfg = Config {
        state_dir: args.state_dir,
        archive_dir: match args.archive_dir {
            Some(dir) => dir,
            None => private_temp_dir()?,
        },
        cache_max_bytes: args.cache_max_mb * 1024 * 1024,
        cache_ttl: Duration::from_secs(args.cache_ttl),
        refresh_interval: Duration::from_secs(args.refresh_interval),
        max_unpacked_bytes: args.max_unpacked_gb.saturating_mul(1 << 30),
    };
    let fs = ItchFs::new(cfg, ItchClient::new(&args.api_url, token)?)?;
    let listener = NFSTcpListener::bind(&args.listen, fs.clone())
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    tracing::info!("serving NFSv3 on {}", args.listen);
    tokio::spawn(async move { fs.run_maintenance().await });
    tokio::select! {
        served = listener.handle_forever() => served?,
        _ = shutdown_signal() => tracing::info!("shutting down"),
    }
    Ok(())
}

/// The default `--archive-dir`, `<temp dir>/itch-nfs`. The temp dir is shared
/// with other users, who could otherwise create this directory first (or make
/// it a symlink) and so plant archives that get served, or have the server
/// overwrite files of its choosing. So it must be a real directory, owned by
/// us and closed to everyone else.
fn private_temp_dir() -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let dir = std::env::temp_dir().join("itch-nfs");
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("create {}", dir.display())),
    }
    let meta =
        std::fs::symlink_metadata(&dir).with_context(|| format!("stat {}", dir.display()))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != euid {
        bail!(
            "{} is not a directory owned by this user; pass --archive-dir",
            dir.display()
        );
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    Ok(dir)
}

/// Ctrl-C or SIGTERM. Handled explicitly because, as PID 1 in a container,
/// the process would otherwise ignore SIGTERM and `docker stop` would hang.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
