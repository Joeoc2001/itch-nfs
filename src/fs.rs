//! The virtual filesystem: `/<pack>/<version>/<archive contents>`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use nfsserve::nfs::*;
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tracing::{info, warn};

use crate::archive::{self, Archives, Manifest};
use crate::cache::{KeyedLocks, MemCache};
use crate::ingest::{self, Limits, Outcome};
use crate::itch::{ItchClient, Upload};
use crate::state::{self, GameState, State, VersionState};
use crate::tree::{NodeKind, Seen, Source, Tree};

pub struct Config {
    /// Durable: the version history and file listings.
    pub state_dir: PathBuf,
    /// Disposable: downloaded archives.
    pub archive_dir: PathBuf,
    pub cache_max_bytes: u64,
    pub cache_ttl: Duration,
    pub refresh_interval: Duration,
    /// Cap on the decompressed size of a single upload.
    pub max_unpacked_bytes: u64,
}

/// How many archives-within-archives are unpacked.
const MAX_ARCHIVE_DEPTH: u32 = 8;

/// Largest READ served at once, matching the `rtmax` nfsserve advertises.
/// Clients may ask for more, and the export is unauthenticated, so without a
/// cap one request could make the server allocate up to 4 GiB.
const MAX_READ: u32 = 1024 * 1024;

const ERROR_BACKOFF: Duration = Duration::from_secs(30);

/// File ids: 20 bits pack number, 16 bits version number, 28 bits tree node.
/// They are derived from persisted numbering only, so they (and the NFS file
/// handles made from them) stay valid across server restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Id {
    Root,
    Game(u32),
    Version(u32, u16),
    Node(u32, u16, u32),
}

const ROOT_ID: u64 = 1;

impl Id {
    fn encode(self) -> u64 {
        match self {
            Id::Root => ROOT_ID,
            Id::Game(g) => (g as u64) << 44,
            Id::Version(g, v) => (g as u64) << 44 | (v as u64) << 28,
            Id::Node(g, v, n) => (g as u64) << 44 | (v as u64) << 28 | n as u64,
        }
    }

    fn decode(id: u64) -> Result<Id, nfsstat3> {
        if id == ROOT_ID {
            return Ok(Id::Root);
        }
        let (g, v, n) = (
            (id >> 44) as u32,
            (id >> 28) as u16,
            (id & 0xFFF_FFFF) as u32,
        );
        match (g, v, n) {
            (0, _, _) | (_, 0, 1..) => Err(nfsstat3::NFS3ERR_STALE),
            (g, 0, 0) => Ok(Id::Game(g)),
            (g, v, 0) => Ok(Id::Version(g, v)),
            (g, v, n) => Ok(Id::Node(g, v, n)),
        }
    }
}

/// Runtime (non-persisted) data for one pack.
#[derive(Default)]
struct GameRt {
    refresh: tokio::sync::Mutex<Refresh>,
    /// Trees of versions 1..=len, loaded in order because each one is
    /// de-duplicated against all of its predecessors.
    trees: RwLock<Vec<Arc<Tree>>>,
    load: tokio::sync::Mutex<Seen>,
}

#[derive(Default)]
struct Refresh {
    next: Option<Instant>,
    /// What itch currently serves for each upload id: (build id, fingerprint).
    /// `None` until the uploads have been listed successfully.
    current: Option<HashMap<u64, (Option<u64>, String)>>,
}

struct Inner {
    cfg: Config,
    client: ItchClient,
    state: Mutex<State>,
    library_next: tokio::sync::Mutex<Option<Instant>>,
    games: Mutex<HashMap<u32, Arc<GameRt>>>,
    mem: MemCache<u64>,
    locks: KeyedLocks<u64>,
    archives: Arc<Archives>,
}

#[derive(Clone)]
pub struct ItchFs(Arc<Inner>);

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn io_err(e: anyhow::Error) -> nfsstat3 {
    warn!("{e:#}");
    nfsstat3::NFS3ERR_IO
}

fn time(secs: i64) -> nfstime3 {
    nfstime3 {
        seconds: secs.clamp(0, u32::MAX as i64) as u32,
        nseconds: 0,
    }
}

fn attr(id: u64, dir: bool, size: u64, mtime: i64) -> fattr3 {
    fattr3 {
        ftype: if dir { ftype3::NF3DIR } else { ftype3::NF3REG },
        mode: if dir { 0o555 } else { 0o444 },
        nlink: if dir { 2 } else { 1 },
        uid: 0,
        gid: 0,
        size: if dir { 4096 } else { size },
        used: if dir { 4096 } else { size },
        rdev: specdata3::default(),
        fsid: 0x69746368,
        fileid: id,
        atime: time(mtime),
        mtime: time(mtime),
        ctime: time(mtime),
    }
}

fn fingerprint(u: &Upload) -> String {
    if u.build_id.is_some() {
        return String::new(); // the build id says it all
    }
    match &u.md5_hash {
        Some(h) if !h.is_empty() => h.clone(),
        _ => format!(
            "{}|{}",
            u.updated_at.as_deref().unwrap_or(""),
            u.size.unwrap_or(0)
        ),
    }
}

impl ItchFs {
    pub fn new(cfg: Config, client: ItchClient) -> Result<Self> {
        std::fs::create_dir_all(cfg.state_dir.join("manifests"))
            .with_context(|| format!("create {}", cfg.state_dir.display()))?;
        // Extracted oversized files are only valid for this process.
        for scratch in ["spill", "tmp"] {
            let dir = cfg.archive_dir.join(scratch);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let state = State::load(&cfg.state_dir)?;
        let mem = MemCache::new(cfg.cache_max_bytes, cfg.cache_ttl);
        Ok(ItchFs(Arc::new(Inner {
            cfg,
            client,
            state: Mutex::new(state),
            library_next: Default::default(),
            games: Default::default(),
            mem,
            locks: Default::default(),
            archives: Default::default(),
        })))
    }

    /// Background upkeep: expire cached assets, and poll itch.io for new packs
    /// and new versions. Polling matters beyond freshness: NFS clients only
    /// re-read a directory when its mtime changes, so discovery cannot rely on
    /// client requests alone.
    pub async fn run_maintenance(&self) {
        let sweep =
            (self.0.cfg.cache_ttl / 2).clamp(Duration::from_secs(1), Duration::from_secs(60));
        let mut sweep = tokio::time::interval(sweep);
        let mut refresh =
            tokio::time::interval(self.0.cfg.refresh_interval.max(Duration::from_secs(1)));
        loop {
            tokio::select! {
                _ = sweep.tick() => self.0.mem.purge_expired(),
                _ = refresh.tick() => self.refresh_all().await,
            }
        }
    }

    /// Refresh the library, and every pack that has been browsed before.
    pub async fn refresh_all(&self) {
        self.ensure_library(true).await;
        let tracked: Vec<u32> = {
            let st = self.0.state.lock().unwrap();
            st.games
                .iter()
                .filter(|(_, g)| !g.versions.is_empty() && g.key_id.is_some())
                .map(|(no, _)| *no)
                .collect()
        };
        for no in tracked {
            self.ensure_game(no, true).await;
        }
    }

    fn rt(&self, no: u32) -> Arc<GameRt> {
        self.0.games.lock().unwrap().entry(no).or_default().clone()
    }

    fn save_state(&self, st: &State) {
        if let Err(e) = st.save(&self.0.cfg.state_dir) {
            warn!("failed to persist state: {e:#}");
        }
    }

    /// Sync the list of owned packs, unless done recently.
    async fn ensure_library(&self, force: bool) {
        let mut next = self.0.library_next.lock().await;
        if !force && next.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let owned = match self.0.client.owned_games().await {
            Ok(o) => o,
            Err(e) => {
                warn!("could not list owned packs, serving known ones: {e:#}");
                *next = Some(Instant::now() + ERROR_BACKOFF.min(self.0.cfg.refresh_interval));
                return;
            }
        };
        *next = Some(Instant::now() + self.0.cfg.refresh_interval);

        let mut st = self.0.state.lock().unwrap();
        let keys: HashMap<u64, u64> = owned.iter().map(|g| (g.game_id, g.key_id)).collect();
        let mut changed = false;
        for g in st.games.values_mut() {
            let key = keys.get(&g.game_id).copied();
            changed |= g.key_id != key;
            g.key_id = key;
        }
        let mut listing_changed = false;
        for g in owned {
            if st.game_no(g.game_id).is_some() {
                continue;
            }
            let no = st.games.keys().next_back().map_or(1, |n| n + 1);
            if no > state::MAX_GAMES {
                warn!("too many packs, ignoring {}", g.url);
                continue;
            }
            let mut name = state::game_dir_name(&g.url, g.game_id);
            if st.games.values().any(|x| x.name == name) {
                name = format!("{name}-{}", g.game_id);
            }
            info!("new pack: {name}");
            st.games.insert(
                no,
                GameState {
                    game_id: g.game_id,
                    name,
                    title: g.title,
                    key_id: Some(g.key_id),
                    versions: Vec::new(),
                    changed_at: now_secs(),
                },
            );
            listing_changed = true;
        }
        if listing_changed {
            st.changed_at = now_secs().max(st.changed_at + 1);
        }
        if changed || listing_changed {
            self.save_state(&st);
        }
    }

    /// Sync the versions of one pack with the uploads itch currently lists.
    async fn ensure_game(&self, no: u32, force: bool) {
        let rt = self.rt(no);
        let mut r = rt.refresh.lock().await;
        if !force && r.next.is_some_and(|t| Instant::now() < t) {
            return;
        }
        r.next = Some(Instant::now() + self.0.cfg.refresh_interval);
        let ids = self
            .0
            .state
            .lock()
            .unwrap()
            .games
            .get(&no)
            .map(|g| (g.game_id, g.key_id, g.name.clone()));
        let Some((game_id, Some(key_id), game_name)) = ids else {
            return;
        };
        let uploads = match self.0.client.uploads(game_id, key_id).await {
            Ok(u) => u,
            Err(e) => {
                warn!("could not list uploads of {game_name}, serving known versions: {e:#}");
                r.next = Some(Instant::now() + ERROR_BACKOFF.min(self.0.cfg.refresh_interval));
                return;
            }
        };
        r.current = Some(
            uploads
                .iter()
                .map(|u| (u.id, (u.build_id, fingerprint(u))))
                .collect(),
        );

        let mut st = self.0.state.lock().unwrap();
        let Some(g) = st.games.get_mut(&no) else {
            return;
        };
        let mut new: Vec<VersionState> = uploads
            .iter()
            .filter(|u| {
                !g.versions.iter().any(|v| {
                    v.upload_id == u.id
                        && v.build_id == u.build_id
                        && v.fingerprint == fingerprint(u)
                })
            })
            .map(|u| VersionState {
                name: String::new(),
                upload_id: u.id,
                build_id: u.build_id,
                fingerprint: fingerprint(u),
                filename: u.filename.clone(),
                size: u.size,
                timestamp: state::parse_ts(u.updated_at.as_deref())
                    .or(state::parse_ts(u.created_at.as_deref()))
                    .unwrap_or_else(now_secs),
            })
            .collect();
        if new.is_empty() {
            return;
        }
        // Oldest first: later versions are de-duplicated against earlier ones.
        new.sort_by_key(|v| (v.timestamp, v.upload_id));
        for mut v in new {
            if g.versions.len() >= state::MAX_VERSIONS {
                warn!("{game_name} has too many versions, ignoring new uploads");
                break;
            }
            let base = state::version_dir_name(v.timestamp, &v.filename, v.upload_id, v.build_id);
            v.name = base.clone();
            let mut n = 2;
            while g.versions.iter().any(|x| x.name == v.name) {
                v.name = format!("{base}-{n}");
                n += 1;
            }
            info!("new version: {game_name}/{}", v.name);
            g.versions.push(v);
        }
        g.changed_at = now_secs().max(g.changed_at + 1);
        self.save_state(&st);
    }

    fn game(&self, no: u32) -> Result<GameState, nfsstat3> {
        self.0
            .state
            .lock()
            .unwrap()
            .games
            .get(&no)
            .cloned()
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn version(&self, no: u32, ver: u16) -> Result<(u64, VersionState), nfsstat3> {
        let st = self.0.state.lock().unwrap();
        let g = st.games.get(&no).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let v = g
            .versions
            .get(ver as usize - 1)
            .ok_or(nfsstat3::NFS3ERR_STALE)?;
        Ok((g.game_id, v.clone()))
    }

    /// Where a version's archive lives in the disk cache: `.zip` if it is
    /// served as a zip (as uploaded, or flattened), `.raw` if served whole.
    fn archive_path(&self, game_id: u64, ver: u16, zip: bool) -> PathBuf {
        self.0.cfg.archive_dir.join(format!(
            "{game_id}-{ver}.{}",
            if zip { "zip" } else { "raw" }
        ))
    }

    /// Make sure the version's archive is in the disk cache, downloading and
    /// unpacking it if needed. Returns its path and whether it is a zip.
    async fn ensure_archive(&self, no: u32, ver: u16) -> Result<(PathBuf, bool)> {
        let (game_id, v) = self
            .version(no, ver)
            .map_err(|_| anyhow!("unknown version"))?;
        let key = Id::Version(no, ver).encode();
        let _guard = self.0.locks.lock(key).await;
        for zip in [true, false] {
            let path = self.archive_path(game_id, ver, zip);
            if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                return Ok((path, zip));
            }
        }
        // An upload id can be re-used for new content, so only download if itch
        // still serves exactly this version.
        self.ensure_game(no, false).await;
        let rt = self.rt(no);
        let current = rt
            .refresh
            .lock()
            .await
            .current
            .as_ref()
            .map(|c| c.get(&v.upload_id).cloned());
        match current {
            None => bail!("{}: cannot download, itch.io is unreachable", v.name),
            Some(c) if c != Some((v.build_id, v.fingerprint.clone())) => {
                bail!(
                    "{}: this version is no longer downloadable from itch.io and its archive is not in the disk cache",
                    v.name
                )
            }
            Some(_) => {}
        }
        let key_id = self
            .0
            .state
            .lock()
            .unwrap()
            .games
            .get(&no)
            .and_then(|g| g.key_id);
        let key_id = key_id
            .ok_or_else(|| anyhow!("{}: pack is no longer in the itch.io library", v.name))?;

        info!(
            "downloading {} ({} bytes)",
            v.name,
            v.size.map_or("?".into(), |s| s.to_string())
        );
        let part = self.0.cfg.archive_dir.join(format!("{game_id}-{ver}.part"));
        let flat = self.0.cfg.archive_dir.join(format!("{game_id}-{ver}.flat"));
        let result = async {
            let n = self.0.client.download(v.upload_id, key_id, &part).await?;
            if let Some(size) = v.size
                && v.build_id.is_none()
                && size != n
            {
                bail!("{}: downloaded {n} bytes, expected {size}", v.name);
            }
            let limits = Limits {
                max_unpacked_bytes: self.0.cfg.max_unpacked_bytes,
                max_depth: MAX_ARCHIVE_DEPTH,
            };
            let (src, out, tmp, filename) = (
                part.clone(),
                flat.clone(),
                self.0.cfg.archive_dir.join("tmp"),
                v.filename.clone(),
            );
            let outcome = tokio::task::spawn_blocking(move || {
                ingest::ingest(&src, &filename, &out, &tmp, limits)
            })
            .await??;
            self.0.archives.forget(key);
            let (from, zip) = match outcome {
                Outcome::ZipAsIs => (&part, true),
                Outcome::Flattened => (&flat, true),
                Outcome::Raw => (&part, false),
            };
            let path = self.archive_path(game_id, ver, zip);
            tokio::fs::rename(from, &path).await?;
            Ok((path, zip))
        }
        .await;
        // Whichever of the two was not promoted is scrap.
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&flat).await;
        result
    }

    async fn manifest(&self, no: u32, ver: u16) -> Result<Manifest> {
        let (game_id, v) = self
            .version(no, ver)
            .map_err(|_| anyhow!("unknown version"))?;
        let path = self
            .0
            .cfg
            .state_dir
            .join("manifests")
            .join(format!("{game_id}-{ver}.json"));
        if let Ok(bytes) = tokio::fs::read(&path).await {
            match serde_json::from_slice(&bytes) {
                Ok(m) => return Ok(m),
                Err(e) => warn!("ignoring corrupt {}: {e}", path.display()),
            }
        }
        let (archive, zip) = self.ensure_archive(no, ver).await?;
        let manifest =
            tokio::task::spawn_blocking(move || archive::scan(&archive, &v.filename, zip))
                .await??;
        state::write_atomic(&path, &serde_json::to_vec(&manifest)?)?;
        Ok(manifest)
    }

    /// The (de-duplicated) tree of a version. Loads all earlier versions first.
    async fn tree(&self, no: u32, ver: u16) -> Result<Arc<Tree>, nfsstat3> {
        let rt = self.rt(no);
        if let Some(t) = rt.trees.read().unwrap().get(ver as usize - 1) {
            return Ok(t.clone());
        }
        let mut seen = rt.load.lock().await;
        loop {
            let have = rt.trees.read().unwrap().len();
            if have >= ver as usize {
                return Ok(rt.trees.read().unwrap()[ver as usize - 1].clone());
            }
            let next = have as u16 + 1;
            let (_, v) = self.version(no, next)?;
            let manifest = self.manifest(no, next).await.map_err(io_err)?;
            let tree = Tree::build(&manifest, &v.name, &mut seen).map_err(io_err)?;
            rt.trees.write().unwrap().push(Arc::new(tree));
        }
    }

    async fn zip_archive(&self, no: u32, ver: u16) -> Result<PathBuf> {
        match self.ensure_archive(no, ver).await? {
            (path, true) => Ok(path),
            (path, false) => bail!(
                "{} could not be unpacked the way it was when it was first listed",
                path.display()
            ),
        }
    }

    /// Decompressed contents of a zip entry, via the in-memory cache.
    async fn file_bytes(
        &self,
        id: u64,
        no: u32,
        ver: u16,
        index: usize,
        size: u64,
        crc: u32,
    ) -> Result<Bytes> {
        if let Some(b) = self.0.mem.get(&id) {
            return Ok(b);
        }
        let _guard = self.0.locks.lock(id).await;
        if let Some(b) = self.0.mem.get(&id) {
            return Ok(b);
        }
        let path = self.zip_archive(no, ver).await?;
        let archives = self.0.archives.clone();
        let key = Id::Version(no, ver).encode();
        let data = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::with_capacity(size as usize);
            archives
                .extract(key, &path, index, size, crc, &mut buf)
                .map(|_| buf)
        })
        .await??;
        let data = Bytes::from(data);
        self.0.mem.insert(id, data.clone());
        Ok(data)
    }

    /// Files too big for the memory cache are extracted to disk instead.
    async fn spill(
        &self,
        id: u64,
        no: u32,
        ver: u16,
        index: usize,
        size: u64,
        crc: u32,
    ) -> Result<PathBuf> {
        let dest = self
            .0
            .cfg
            .archive_dir
            .join("spill")
            .join(format!("{id:016x}"));
        let _guard = self.0.locks.lock(id).await;
        if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            return Ok(dest);
        }
        let path = self.zip_archive(no, ver).await?;
        let archives = self.0.archives.clone();
        let key = Id::Version(no, ver).encode();
        let out = dest.clone();
        tokio::task::spawn_blocking(move || {
            let part = out.with_extension("part");
            let mut file = std::io::BufWriter::new(std::fs::File::create(&part)?);
            archives.extract(key, &path, index, size, crc, &mut file)?;
            file.into_inner().map_err(|e| e.into_error())?;
            std::fs::rename(&part, &out)?;
            anyhow::Ok(())
        })
        .await??;
        Ok(dest)
    }

    /// (name, id, attributes) of everything in a directory, in a stable order.
    async fn list(&self, dir: Id) -> Result<Vec<(String, fattr3)>, nfsstat3> {
        match dir {
            Id::Root => {
                self.ensure_library(false).await;
                let st = self.0.state.lock().unwrap();
                Ok(st
                    .games
                    .iter()
                    .filter(|(_, g)| g.key_id.is_some() || !g.versions.is_empty())
                    .map(|(no, g)| {
                        (
                            g.name.clone(),
                            attr(Id::Game(*no).encode(), true, 0, g.changed_at),
                        )
                    })
                    .collect())
            }
            Id::Game(no) => {
                self.game(no)?;
                self.ensure_game(no, false).await;
                let g = self.game(no)?;
                Ok(g.versions
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        (
                            v.name.clone(),
                            attr(Id::Version(no, i as u16 + 1).encode(), true, 0, v.timestamp),
                        )
                    })
                    .collect())
            }
            Id::Version(no, ver) => self.list_node(no, ver, 0).await,
            Id::Node(no, ver, n) => self.list_node(no, ver, n).await,
        }
    }

    async fn list_node(
        &self,
        no: u32,
        ver: u16,
        n: u32,
    ) -> Result<Vec<(String, fattr3)>, nfsstat3> {
        let (_, v) = self.version(no, ver)?;
        let tree = self.tree(no, ver).await?;
        let node = tree.get(n).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let NodeKind::Dir { children } = &node.kind else {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        };
        Ok(children
            .iter()
            .filter_map(|&c| {
                tree.get(c).map(|child| {
                    (
                        child.name.clone(),
                        node_attr(no, ver, c, &child.kind, v.timestamp),
                    )
                })
            })
            .collect())
    }
}

fn node_attr(no: u32, ver: u16, n: u32, kind: &NodeKind, version_ts: i64) -> fattr3 {
    let id = Id::Node(no, ver, n).encode();
    match kind {
        NodeKind::Dir { .. } => attr(id, true, 0, version_ts),
        NodeKind::File { size, mtime, .. } => attr(
            id,
            false,
            *size,
            if *mtime > 0 { *mtime } else { version_ts },
        ),
    }
}

#[async_trait]
impl NFSFileSystem for ItchFs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadOnly
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let dir = Id::decode(dirid)?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            return Ok(match dir {
                Id::Root | Id::Game(_) => Id::Root,
                Id::Version(g, _) => Id::Game(g),
                Id::Node(g, v, n) => {
                    let tree = self.tree(g, v).await?;
                    match tree.get(n).ok_or(nfsstat3::NFS3ERR_STALE)?.parent {
                        0 => Id::Version(g, v),
                        p => Id::Node(g, v, p),
                    }
                }
            }
            .encode());
        }
        let found = match dir {
            Id::Root => {
                self.ensure_library(false).await;
                let st = self.0.state.lock().unwrap();
                st.games
                    .iter()
                    .find(|(_, g)| g.name == name && (g.key_id.is_some() || !g.versions.is_empty()))
                    .map(|(no, _)| Id::Game(*no))
            }
            Id::Game(no) => {
                self.game(no)?;
                self.ensure_game(no, false).await;
                self.game(no)?
                    .versions
                    .iter()
                    .position(|v| v.name == name)
                    .map(|i| Id::Version(no, i as u16 + 1))
            }
            Id::Version(g, v) | Id::Node(g, v, _) => {
                let n = if let Id::Node(_, _, n) = dir { n } else { 0 };
                self.version(g, v)?;
                let tree = self.tree(g, v).await?;
                match tree.get(n).ok_or(nfsstat3::NFS3ERR_STALE)?.kind {
                    NodeKind::Dir { .. } => tree.child(n, name).map(|c| Id::Node(g, v, c)),
                    NodeKind::File { .. } => return Err(nfsstat3::NFS3ERR_NOTDIR),
                }
            }
        };
        found.map(Id::encode).ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        match Id::decode(id)? {
            Id::Root => Ok(attr(id, true, 0, self.0.state.lock().unwrap().changed_at)),
            Id::Game(no) => Ok(attr(id, true, 0, self.game(no)?.changed_at)),
            Id::Version(no, ver) => Ok(attr(id, true, 0, self.version(no, ver)?.1.timestamp)),
            Id::Node(no, ver, n) => {
                let (_, v) = self.version(no, ver)?;
                let tree = self.tree(no, ver).await?;
                let node = tree.get(n).ok_or(nfsstat3::NFS3ERR_STALE)?;
                Ok(node_attr(no, ver, n, &node.kind, v.timestamp))
            }
        }
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let Id::Node(no, ver, n) = Id::decode(id)? else {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        };
        self.version(no, ver)?;
        let tree = self.tree(no, ver).await?;
        let node = tree.get(n).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let NodeKind::File {
            size, crc, source, ..
        } = &node.kind
        else {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        };
        let (size, crc) = (*size, *crc);
        let count = count.min(MAX_READ);
        let slice = |b: &Bytes| {
            let start = offset.min(b.len() as u64) as usize;
            let end = (offset.saturating_add(count as u64)).min(b.len() as u64) as usize;
            b[start..end].to_vec()
        };
        let data = match source {
            Source::Inline(b) => slice(b),
            Source::Zip(index) if size <= self.0.mem.max_bytes() => slice(
                &self
                    .file_bytes(id, no, ver, *index, size, crc)
                    .await
                    .map_err(io_err)?,
            ),
            source => {
                let path = match source {
                    Source::Zip(index) => self.spill(id, no, ver, *index, size, crc).await,
                    _ => self.ensure_archive(no, ver).await.map(|(path, _)| path),
                }
                .map_err(io_err)?;
                tokio::task::spawn_blocking(move || archive::read_range(&path, offset, count))
                    .await
                    .map_err(|e| io_err(e.into()))?
                    .map_err(|e| io_err(e.into()))?
            }
        };
        let eof = offset.saturating_add(data.len() as u64) >= size;
        Ok((data, eof))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let all = self.list(Id::decode(dirid)?).await?;
        let start = match start_after {
            0 => 0,
            after => {
                all.iter()
                    .position(|(_, a)| a.fileid == after)
                    .ok_or(nfsstat3::NFS3ERR_BAD_COOKIE)?
                    + 1
            }
        };
        let end = all.len().min(start.saturating_add(max_entries));
        let entries = all[start..end]
            .iter()
            .map(|(name, a)| DirEntry {
                fileid: a.fileid,
                name: name.as_bytes().into(),
                attr: *a,
            })
            .collect();
        Ok(ReadDirResult {
            entries,
            end: end == all.len(),
        })
    }

    /// Handles are just the file id, so they outlive server restarts.
    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        nfs_fh3 {
            data: id.to_be_bytes().to_vec(),
        }
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        let bytes: [u8; 8] = fh
            .data
            .as_slice()
            .try_into()
            .map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?;
        Ok(u64::from_be_bytes(bytes))
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_INVAL)
    }

    async fn setattr(&self, _id: fileid3, _setattr: sattr3) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn write(&self, _id: fileid3, _offset: u64, _data: &[u8]) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create_exclusive(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn mkdir(
        &self,
        _dirid: fileid3,
        _dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn remove(&self, _dirid: fileid3, _filename: &filename3) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn rename(
        &self,
        _from_dirid: fileid3,
        _from_filename: &filename3,
        _to_dirid: fileid3,
        _to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }
}
