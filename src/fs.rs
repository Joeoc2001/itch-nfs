//! The frontend's virtual filesystem: `/<pack>/<version>/<archive contents>`,
//! served from the archive directory.
//!
//! Nothing here talks to itch.io, and nothing here writes to the archive.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use nfsserve::nfs::*;
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tracing::{info, warn};

use crate::archive::{self, Archives, Kind};
use crate::cache::{KeyedLocks, MemCache};
use crate::diskcache::{Cached, DiskCache};
use crate::store::{Pack, PackMeta, Store, VersionMeta};
use crate::tree::{NodeKind, Seen, Source, Tree};

pub struct Config {
    /// The durable archive the scanner writes. Only ever read here.
    pub archive_dir: PathBuf,
    /// Local and disposable: copies of the archive's metadata and zips, and
    /// extracted files too large for memory.
    pub cache_dir: PathBuf,
    /// Budget for decompressed assets held in memory.
    pub cache_max_bytes: u64,
    pub cache_ttl: Duration,
    /// How often the archive is listed for newly committed versions.
    pub rescan_interval: Duration,
    /// Budget for the local copies of the archive's zips.
    pub disk_cache_max_bytes: u64,
}

/// Largest READ served at once, matching the `rtmax` nfsserve advertises.
/// Clients may ask for more, and the export is unauthenticated, so without a
/// cap one request could make the server allocate up to 4 GiB.
const MAX_READ: u32 = 1024 * 1024;

/// File ids: 20 bits pack number, 16 bits version number, 28 bits tree node.
/// The numbers are recorded in the archive (`pack.json`, `version.json`), so
/// the ids (and the NFS file handles made from them) stay valid across
/// restarts, and across the loss of everything but the archive.
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

/// A pack with at least one committed version.
struct PackEntry {
    meta: PackMeta,
    versions: BTreeMap<u16, Arc<VersionMeta>>,
    /// The directory's mtime, which is what makes NFS clients re-read it:
    /// when the newest version was archived, and never going backwards.
    changed_at: i64,
}

/// What the archive holds, by pack number.
#[derive(Default)]
struct Tables {
    packs: BTreeMap<u32, PackEntry>,
    /// The root's mtime; changes whenever a pack appears.
    changed_at: i64,
}

/// Lazily built data for one pack.
#[derive(Default)]
struct GameRt {
    /// Trees of the pack's first versions, in version number order. Always a
    /// prefix, because each is de-duplicated against all of its predecessors.
    trees: RwLock<BTreeMap<u16, Arc<Tree>>>,
    /// Everything in the versions that `trees` covers.
    load: tokio::sync::Mutex<Seen>,
}

struct Inner {
    cfg: Config,
    store: Store,
    tables: RwLock<Tables>,
    rescan: tokio::sync::Mutex<()>,
    games: Mutex<HashMap<u32, Arc<GameRt>>>,
    mem: MemCache<u64>,
    locks: KeyedLocks<u64>,
    archives: Arc<Archives>,
    disk: DiskCache,
}

#[derive(Clone)]
pub struct ItchFs(Arc<Inner>);

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

impl ItchFs {
    /// Open the archive and load what it holds. An archive that does not
    /// exist yet (the scanner has not run) is an empty export, not an error.
    pub fn new(cfg: Config) -> Result<Self> {
        // Extracted oversized files are only valid for this process.
        let spill = cfg.cache_dir.join("spill");
        let _ = std::fs::remove_dir_all(&spill);
        std::fs::create_dir_all(&spill).with_context(|| format!("create {}", spill.display()))?;
        let disk = DiskCache::new(cfg.cache_dir.join("zips"), cfg.disk_cache_max_bytes)?;
        let store = Store::new(&cfg.archive_dir, Some(cfg.cache_dir.join("meta")));
        let mem = MemCache::new(cfg.cache_max_bytes, cfg.cache_ttl);
        // A zip that is kept open would hold on to the space of its copy
        // after the copy is evicted.
        let archives: Arc<Archives> = Default::default();
        let pool = archives.clone();
        disk.on_evict(move |path| pool.forget_path(path));
        let fs = ItchFs(Arc::new(Inner {
            cfg,
            store,
            tables: Default::default(),
            rescan: Default::default(),
            games: Default::default(),
            mem,
            locks: Default::default(),
            archives,
            disk,
        }));
        let packs = fs.0.store.load().context("read the archive")?;
        fs.merge(packs, false);
        let (packs, versions) = {
            let t = fs.0.tables.read().unwrap();
            (
                t.packs.len(),
                t.packs.values().map(|p| p.versions.len()).sum::<usize>(),
            )
        };
        info!(
            "{} holds {versions} version(s) of {packs} pack(s)",
            fs.0.cfg.archive_dir.display()
        );
        Ok(fs)
    }

    /// Background upkeep: expire cached assets, and list the archive for
    /// newly committed versions.
    pub async fn run_maintenance(&self) {
        let sweep =
            (self.0.cfg.cache_ttl / 2).clamp(Duration::from_secs(1), Duration::from_secs(60));
        let mut sweep = tokio::time::interval(sweep);
        let every = self.0.cfg.rescan_interval.max(Duration::from_secs(1));
        let mut rescan = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        loop {
            tokio::select! {
                _ = sweep.tick() => self.0.mem.purge_expired(),
                _ = rescan.tick() => {
                    if let Err(e) = self.rescan().await {
                        warn!("could not rescan the archive, serving what is known: {e:#}");
                    }
                }
            }
        }
    }

    /// Pick up packs and versions committed since the last look. Costs one
    /// directory listing per pack, and one read per new metadata file.
    pub async fn rescan(&self) -> Result<()> {
        let _guard = self.0.rescan.lock().await;
        let store = self.0.store.clone();
        let packs = tokio::task::spawn_blocking(move || store.load()).await??;
        self.merge(packs, true);
        Ok(())
    }

    /// Add what is new in `packs` to the tables, logging it if `announce`.
    /// Nothing is ever removed or changed: the archive is write-once, and
    /// handles must stay valid.
    fn merge(&self, packs: Vec<Pack>, announce: bool) {
        let mut t = self.0.tables.write().unwrap();
        let mut new_packs: Option<i64> = None;
        for p in packs {
            // A pack appears once it has a committed version.
            if p.versions.is_empty() {
                continue;
            }
            let no = p.meta.no;
            let fresh = !t.packs.contains_key(&no);
            let entry = t.packs.entry(no).or_insert_with(|| PackEntry {
                meta: p.meta.clone(),
                versions: BTreeMap::new(),
                changed_at: 0,
            });
            if entry.meta != p.meta {
                warn!(
                    "ignoring pack {}: its number is {}'s",
                    p.meta.name, entry.meta.name
                );
                continue;
            }
            let mut added: Option<i64> = None;
            for v in p.versions {
                if entry.versions.contains_key(&v.no) {
                    continue;
                }
                if entry
                    .versions
                    .keys()
                    .next_back()
                    .is_some_and(|&last| v.no < last)
                {
                    // Should not happen (versions are numbered in the order
                    // they are committed), but later versions are
                    // de-duplicated against this one: rebuild their trees.
                    warn!(
                        "{}/{} appeared with a lower number than existing versions",
                        entry.meta.name, v.name
                    );
                    self.0.games.lock().unwrap().remove(&no);
                }
                if announce {
                    info!("new version: {}/{}", entry.meta.name, v.name);
                }
                added = Some(added.unwrap_or(i64::MIN).max(v.archived_at));
                entry.versions.insert(v.no, Arc::new(v));
            }
            if let Some(at) = added {
                entry.changed_at = at.max(entry.changed_at + 1);
                if fresh {
                    new_packs = Some(new_packs.unwrap_or(i64::MIN).max(at));
                }
            }
        }
        if let Some(at) = new_packs {
            t.changed_at = at.max(t.changed_at + 1);
        }
    }

    /// How many times a file has been copied out of the archive directory.
    pub fn archive_reads(&self) -> u64 {
        self.0.disk.copies()
    }

    fn rt(&self, no: u32) -> Arc<GameRt> {
        self.0.games.lock().unwrap().entry(no).or_default().clone()
    }

    /// (pack directory name, version) for a pack and version number.
    fn version(&self, no: u32, ver: u16) -> Result<(String, Arc<VersionMeta>), nfsstat3> {
        let t = self.0.tables.read().unwrap();
        let p = t.packs.get(&no).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let v = p.versions.get(&ver).ok_or(nfsstat3::NFS3ERR_STALE)?;
        Ok((p.meta.name.clone(), v.clone()))
    }

    /// The (de-duplicated) tree of a version. Loads all earlier versions first.
    async fn tree(&self, no: u32, ver: u16) -> Result<Arc<Tree>, nfsstat3> {
        let rt = self.rt(no);
        if let Some(t) = rt.trees.read().unwrap().get(&ver) {
            return Ok(t.clone());
        }
        let mut seen = rt.load.lock().await;
        // Every version up to this one that has no tree yet, oldest first.
        let (pack, todo): (String, Vec<Arc<VersionMeta>>) = {
            let t = self.0.tables.read().unwrap();
            let p = t.packs.get(&no).ok_or(nfsstat3::NFS3ERR_STALE)?;
            let have = rt.trees.read().unwrap();
            (
                p.meta.name.clone(),
                p.versions
                    .range(..=ver)
                    .filter(|(n, _)| !have.contains_key(n))
                    .map(|(_, v)| v.clone())
                    .collect(),
            )
        };
        for v in todo {
            let (store, name, meta) = (self.0.store.clone(), pack.clone(), v.clone());
            let manifest = tokio::task::spawn_blocking(move || store.manifest(&name, &meta))
                .await
                .map_err(|e| io_err(e.into()))?
                .map_err(io_err)?;
            let tree = Tree::build(&manifest, &v.name, &mut seen).map_err(io_err)?;
            rt.trees.write().unwrap().insert(v.no, Arc::new(tree));
        }
        rt.trees
            .read()
            .unwrap()
            .get(&ver)
            .cloned()
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    /// The local copy of the file a version is served from, fetched from the
    /// archive (once) if need be.
    async fn served(&self, no: u32, ver: u16) -> Result<(Cached, Kind)> {
        let (pack, v) = self
            .version(no, ver)
            .map_err(|_| anyhow!("unknown version"))?;
        let file = v
            .file(&v.serve)
            .ok_or_else(|| anyhow!("{pack}/{} does not list {}", v.name, v.serve))?;
        let src = self.0.store.file_path(&pack, &v.name, file);
        let cached = self.0.disk.get(&src, file.size, &file.sha256).await?;
        Ok((cached, v.kind))
    }

    async fn zip_archive(&self, no: u32, ver: u16) -> Result<Cached> {
        match self.served(no, ver).await? {
            (cached, Kind::Zip) => Ok(cached),
            (cached, Kind::Raw) => bail!(
                "{} is listed as a zip but archived as a plain file",
                cached.path().display()
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
        let zip = self.zip_archive(no, ver).await?;
        let archives = self.0.archives.clone();
        let key = Id::Version(no, ver).encode();
        let data = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::with_capacity(size as usize);
            archives
                .extract(key, zip.path(), index, size, crc, &mut buf)
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
            .cache_dir
            .join("spill")
            .join(format!("{id:016x}"));
        let _guard = self.0.locks.lock(id).await;
        if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            return Ok(dest);
        }
        let zip = self.zip_archive(no, ver).await?;
        let archives = self.0.archives.clone();
        let key = Id::Version(no, ver).encode();
        let out = dest.clone();
        tokio::task::spawn_blocking(move || {
            let part = out.with_extension("part");
            let mut file = std::io::BufWriter::new(std::fs::File::create(&part)?);
            archives.extract(key, zip.path(), index, size, crc, &mut file)?;
            file.into_inner().map_err(|e| e.into_error())?;
            std::fs::rename(&part, &out)?;
            anyhow::Ok(())
        })
        .await??;
        Ok(dest)
    }

    /// (name, attributes) of everything in a directory, in a stable order.
    async fn list(&self, dir: Id) -> Result<Vec<(String, fattr3)>, nfsstat3> {
        match dir {
            Id::Root => {
                let t = self.0.tables.read().unwrap();
                Ok(t.packs
                    .iter()
                    .map(|(no, p)| {
                        (
                            p.meta.name.clone(),
                            attr(Id::Game(*no).encode(), true, 0, p.changed_at),
                        )
                    })
                    .collect())
            }
            Id::Game(no) => {
                let t = self.0.tables.read().unwrap();
                let p = t.packs.get(&no).ok_or(nfsstat3::NFS3ERR_STALE)?;
                Ok(p.versions
                    .iter()
                    .map(|(ver, v)| {
                        (
                            v.name.clone(),
                            attr(Id::Version(no, *ver).encode(), true, 0, v.timestamp),
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
                let t = self.0.tables.read().unwrap();
                t.packs
                    .iter()
                    .find(|(_, p)| p.meta.name == name)
                    .map(|(no, _)| Id::Game(*no))
            }
            Id::Game(no) => {
                let t = self.0.tables.read().unwrap();
                let p = t.packs.get(&no).ok_or(nfsstat3::NFS3ERR_STALE)?;
                p.versions
                    .iter()
                    .find(|(_, v)| v.name == name)
                    .map(|(ver, _)| Id::Version(no, *ver))
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
            Id::Root => Ok(attr(id, true, 0, self.0.tables.read().unwrap().changed_at)),
            Id::Game(no) => {
                let t = self.0.tables.read().unwrap();
                let p = t.packs.get(&no).ok_or(nfsstat3::NFS3ERR_STALE)?;
                Ok(attr(id, true, 0, p.changed_at))
            }
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
        let ranged = |r: Result<std::io::Result<Vec<u8>>, tokio::task::JoinError>| {
            r.map_err(|e| io_err(e.into()))?
                .map_err(|e| io_err(e.into()))
        };
        let data = match source {
            Source::Inline(b) => slice(b),
            Source::Zip(index) if size <= self.0.mem.max_bytes() => slice(
                &self
                    .file_bytes(id, no, ver, *index, size, crc)
                    .await
                    .map_err(io_err)?,
            ),
            Source::Zip(index) => {
                let path = self
                    .spill(id, no, ver, *index, size, crc)
                    .await
                    .map_err(io_err)?;
                ranged(
                    tokio::task::spawn_blocking(move || archive::read_range(&path, offset, count))
                        .await,
                )?
            }
            Source::Raw => {
                // The upload itself, read from its local copy.
                let (file, _) = self.served(no, ver).await.map_err(io_err)?;
                ranged(
                    tokio::task::spawn_blocking(move || {
                        archive::read_range(file.path(), offset, count)
                    })
                    .await,
                )?
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
