use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use itch_nfs::fs::{Config, ItchFs};
use itch_nfs::itch::{DEFAULT_API_URL, ItchClient};
use itch_nfs::scan::{ScanConfig, Scanner};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};

/// Archives the itch.io asset packs you own, and serves the archive as a
/// read-only NFSv3 export.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The scanner: archive every upload that is not in the archive yet.
    /// The only part that needs the itch.io token, and the only writer to the
    /// archive. Run exactly one per archive.
    Scan(ScanArgs),
    /// The frontend: serve the archive over NFS. Needs no token, never talks
    /// to itch.io, and only reads the archive.
    Serve(ServeArgs),
}

#[derive(Args)]
struct Dirs {
    /// The durable archive of everything downloaded. May be an object store
    /// mount. The scanner writes it; the frontend only reads it.
    #[arg(
        long,
        env = "ITCH_NFS_ARCHIVE_DIR",
        default_value = "./itch-nfs-archive"
    )]
    archive_dir: PathBuf,

    /// Local, disposable directory for caches and scratch space
    /// [default: <system temp dir>/itch-nfs].
    #[arg(long, env = "ITCH_NFS_CACHE_DIR")]
    cache_dir: Option<PathBuf>,
}

impl Dirs {
    fn cache_dir(&self) -> Result<PathBuf> {
        match &self.cache_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create {}", dir.display()))?;
                Ok(dir.clone())
            }
            None => private_temp_dir(),
        }
    }
}

#[derive(Args)]
struct Itch {
    /// itch.io OAuth access token or API key. Prefer the environment variable or --token-file.
    #[arg(long, env = "ITCH_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// File containing the itch.io token.
    #[arg(long, env = "ITCH_TOKEN_FILE", conflicts_with = "token")]
    token_file: Option<PathBuf>,

    /// itch.io API base URL.
    #[arg(long, env = "ITCH_NFS_API_URL", default_value = DEFAULT_API_URL)]
    api_url: String,

    /// Minimum milliseconds between two requests to itch.io.
    #[arg(long, env = "ITCH_NFS_API_DELAY_MS", default_value_t = 250)]
    api_delay_ms: u64,
}

impl Itch {
    fn token(&self) -> Result<String> {
        let token = match (&self.token, &self.token_file) {
            (Some(t), _) => t.clone(),
            (None, Some(f)) => {
                std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?
            }
            (None, None) => bail!("no itch.io token: set ITCH_TOKEN or pass --token-file"),
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            bail!("the itch.io token is empty");
        }
        Ok(token)
    }

    fn client(&self) -> Result<ItchClient> {
        Ok(ItchClient::new(&self.api_url, self.token()?)?
            .with_delay(Duration::from_millis(self.api_delay_ms)))
    }
}

#[derive(Args)]
struct ScanArgs {
    #[command(flatten)]
    dirs: Dirs,

    #[command(flatten)]
    itch: Itch,

    /// Archive uploads that decompress to more than this many GiB as a single
    /// file rather than unpacking them (archive bombs).
    #[arg(long, env = "ITCH_NFS_MAX_UNPACKED_GB", default_value_t = 100)]
    max_unpacked_gb: u64,

    /// Keep running, scanning again this many seconds after each pass ends.
    /// Without it, scan once and exit (for cron or a Kubernetes CronJob).
    #[arg(long, env = "ITCH_NFS_SCAN_INTERVAL")]
    interval: Option<u64>,

    /// Only scan this pack, by directory name (repeatable).
    #[arg(long, env = "ITCH_NFS_ONLY", value_delimiter = ',')]
    only: Vec<String>,

    /// Print what would be archived, as tab-separated
    /// `pack, version, file name, size in bytes`, and download nothing.
    #[arg(long, conflicts_with = "interval")]
    list: bool,

    /// How many packs are scanned (and downloaded) at once.
    #[arg(long, env = "ITCH_NFS_CONCURRENCY", default_value_t = 2)]
    concurrency: usize,

    /// itch.io project classifications to archive (repeatable); `any` for
    /// everything the account owns, games included.
    #[arg(
        long,
        env = "ITCH_NFS_CLASSIFICATION",
        value_delimiter = ',',
        default_value = "assets"
    )]
    classification: Vec<String>,
}

#[derive(Args)]
struct ServeArgs {
    #[command(flatten)]
    dirs: Dirs,

    /// Address to serve NFS (and the MOUNT protocol) on.
    #[arg(long, env = "ITCH_NFS_LISTEN", default_value = "0.0.0.0:11111")]
    listen: String,

    /// Maximum total size of decompressed assets held in memory, in MiB.
    #[arg(long, env = "ITCH_NFS_CACHE_MAX_MB", default_value_t = 1024)]
    cache_max_mb: u64,

    /// Seconds an unused decompressed asset stays in memory.
    #[arg(long, env = "ITCH_NFS_CACHE_TTL", default_value_t = 600)]
    cache_ttl: u64,

    /// Maximum total size of the local copies of the archive's zips, in GiB.
    #[arg(long, env = "ITCH_NFS_DISK_CACHE_MAX_GB", default_value_t = 20)]
    disk_cache_max_gb: u64,

    /// Seconds between listings of the archive for newly committed versions.
    #[arg(long, env = "ITCH_NFS_RESCAN_INTERVAL", default_value_t = 900)]
    rescan_interval: u64,
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("start the async runtime")?;
    let result = runtime.block_on(run());
    // Do not wait for blocking work (unpacking, copying into the archive)
    // that a shutdown signal cut loose: it may take hours, and whatever it
    // leaves half done is redone by the next scan.
    runtime.shutdown_background();
    result
}

async fn run() -> Result<()> {
    // Logs go to stderr: stdout is for `scan --list`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nfsserve=warn".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Scan(args) => run_scan(args).await,
        Command::Serve(args) => run_serve(args).await,
    }
}

async fn run_scan(args: ScanArgs) -> Result<()> {
    let client = args.itch.client()?;
    let cfg = ScanConfig {
        cache_dir: args.dirs.cache_dir()?,
        archive_dir: args.dirs.archive_dir,
        max_unpacked_bytes: args.max_unpacked_gb.saturating_mul(1 << 30),
        only: args.only,
        classifications: if args.classification.iter().any(|c| c == "any") {
            Vec::new()
        } else {
            args.classification
        },
        concurrency: args.concurrency,
    };
    // Before the first pass, not after it: a first scan can run for hours,
    // and as PID 1 it would not react to SIGTERM at all until then.
    let mut shutdown = Shutdown::new()?;
    let scanner = Scanner::new(cfg, client)?;
    if args.list {
        let report = tokio::select! {
            report = scanner.list() => report?,
            _ = shutdown.recv() => bail!("interrupted"),
        };
        for p in &report.pending {
            println!(
                "{}\t{}\t{}\t{}",
                p.pack,
                p.version,
                p.filename.replace(['\t', '\n'], " "),
                p.size.map_or("-".to_string(), |s| s.to_string())
            );
        }
        tracing::info!(
            "{} upload(s) of {} pack(s) in scope are not archived yet",
            report.pending.len(),
            report.packs
        );
        if !report.failed.is_empty() {
            bail!("{} problem(s), see above", report.failed.len());
        }
        return Ok(());
    }
    loop {
        // Dropping a pass half way is safe: a version without its commit
        // marker is redone by the next pass.
        let outcome = tokio::select! {
            outcome = scanner.pass() => outcome,
            _ = shutdown.recv() => {
                if args.interval.is_none() {
                    // One pass: let cron (or CI) see that it did not finish.
                    bail!("interrupted before the scan finished; the next scan carries on");
                }
                tracing::info!("shutting down, the next scan carries on from here");
                return Ok(());
            }
        };
        match &outcome {
            Ok(r) => tracing::info!(
                "scanned {} pack(s): {} upload(s) archived, {} problem(s)",
                r.packs,
                r.archived.len(),
                r.failed.len()
            ),
            Err(e) => tracing::warn!("scan failed: {e:#}"),
        }
        let Some(interval) = args.interval else {
            // One pass: let cron (or CI) see whether it worked.
            let report = outcome?;
            if !report.failed.is_empty() {
                bail!(
                    "{} problem(s), see above; they are retried on the next scan",
                    report.failed.len()
                );
            }
            return Ok(());
        };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval.max(1))) => {}
            _ = shutdown.recv() => {
                tracing::info!("shutting down");
                return Ok(());
            }
        }
    }
}

async fn run_serve(args: ServeArgs) -> Result<()> {
    let cfg = Config {
        cache_dir: args.dirs.cache_dir()?,
        archive_dir: args.dirs.archive_dir,
        cache_max_bytes: args.cache_max_mb.saturating_mul(1024 * 1024),
        cache_ttl: Duration::from_secs(args.cache_ttl),
        rescan_interval: Duration::from_secs(args.rescan_interval),
        disk_cache_max_bytes: args.disk_cache_max_gb.saturating_mul(1 << 30),
    };
    let mut shutdown = Shutdown::new()?;
    let fs = ItchFs::new(cfg)?;
    let listener = NFSTcpListener::bind(&args.listen, fs.clone())
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    tracing::info!("serving NFSv3 on {}", args.listen);
    tokio::spawn(async move { fs.run_maintenance().await });
    tokio::select! {
        served = listener.handle_forever() => served?,
        _ = shutdown.recv() => tracing::info!("shutting down"),
    }
    Ok(())
}

/// The default `--cache-dir`, `<temp dir>/itch-nfs`. The temp dir is shared
/// with other users, who could otherwise create this directory first (or make
/// it a symlink) and so plant cached archives that get served, or have the server
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
            "{} is not a directory owned by this user; pass --cache-dir",
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
///
/// The handlers are installed when this is created and stay installed for the
/// life of the process, so create it once, before the work starts, and wait
/// on the same one every time: a signal that arrives while nothing is waiting
/// is kept for the next `recv`, whereas a listener created later would never
/// see it.
struct Shutdown {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl Shutdown {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("install the SIGINT handler")?,
            terminate: signal(SignalKind::terminate()).context("install the SIGTERM handler")?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}
