//! The archive directory: a durable, self-describing, write-once record of
//! every upload the scanner has seen.
//!
//! ```text
//! <archive>/packs/<pack name>/
//!   pack.json                      written once
//!   <version name>/
//!     upload/<original file name>  the bytes itch.io served, untouched
//!     content.zip                  flattened zip, only if the upload is not a plain zip
//!     manifest.json                the file listing
//!     version.json                 written LAST: the commit marker
//! ```
//!
//! The directory is meant to be an object store mount (S3 and the like), so
//! every file in it is written once, front to back, and never touched again:
//! no rename, no seek, no append. A version directory without `version.json`
//! is incomplete; readers ignore it and the scanner redoes it. The same goes
//! for a `version.json` or `pack.json` that a crash left empty or cut short.
//!
//! The scanner is the only writer, and there must be only one scanner.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::archive::{Kind, Manifest};

pub const MAX_VERSIONS: u16 = 0xFFFF;
pub const MAX_PACKS: u32 = 0xF_FFFF;

pub const PACKS_DIR: &str = "packs";
pub const PACK_FILE: &str = "pack.json";
pub const VERSION_FILE: &str = "version.json";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const CONTENT_FILE: &str = "content.zip";
pub const UPLOAD_DIR: &str = "upload";

/// `pack.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackMeta {
    /// Our own stable pack number, from 1. The top 20 bits of NFS file ids.
    pub no: u32,
    /// The itch.io game id.
    pub game_id: u64,
    /// Directory name, under `packs/` and under the export root.
    pub name: String,
    #[serde(default)]
    pub title: String,
    /// The pack's itch.io page. Empty for packs imported by `migrate`.
    #[serde(default)]
    pub url: String,
}

/// One file of a version, relative to the version directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub path: String,
    pub size: u64,
    /// Lower-case hex.
    pub sha256: String,
}

/// `version.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionMeta {
    /// Version number within the pack, from 1. Bits 28..44 of NFS file ids.
    pub no: u16,
    /// Directory name, under the pack directory in the archive and the export.
    pub name: String,
    pub upload_id: u64,
    pub build_id: Option<u64>,
    /// Distinguishes re-uploads that keep the same upload id.
    pub fingerprint: String,
    /// The upload's file name on itch.io.
    pub filename: String,
    /// The size itch.io reported, if it did.
    pub size: Option<u64>,
    /// When the upload was published (unix seconds).
    pub timestamp: i64,
    /// When the scanner committed this version (unix seconds).
    pub archived_at: i64,
    /// How `serve` is exposed: as the contents of a zip, or as one file.
    pub kind: Kind,
    /// The file the frontend serves from: `content.zip`, or the upload itself.
    pub serve: String,
    /// The original upload. `None` only for versions imported by `migrate`
    /// that had already been flattened, when the original was not kept.
    pub upload: Option<String>,
    /// Every file of the version except this one, with checksums.
    pub files: Vec<FileMeta>,
    /// Anything worth knowing about how this version came to look as it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl VersionMeta {
    pub fn file(&self, path: &str) -> Option<&FileMeta> {
        self.files.iter().find(|f| f.path == path)
    }

    /// Whether this version is exactly what itch.io describes.
    pub fn is_upload(&self, upload_id: u64, build_id: Option<u64>, fingerprint: &str) -> bool {
        self.upload_id == upload_id && self.build_id == build_id && self.fingerprint == fingerprint
    }

    fn validate(&self, dir_name: &str) -> Result<()> {
        if self.name != dir_name {
            bail!("is for a version called {:?}", self.name);
        }
        if self.no == 0 {
            bail!("has version number 0");
        }
        for f in &self.files {
            if !is_safe_relative(&f.path) {
                bail!("lists an unsafe path {:?}", f.path);
            }
        }
        for path in [Some(&self.serve), self.upload.as_ref()]
            .into_iter()
            .flatten()
        {
            if self.file(path).is_none() {
                bail!("names {path:?}, which it does not list");
            }
        }
        if self.file(MANIFEST_FILE).is_none() {
            bail!("lists no {MANIFEST_FILE}");
        }
        Ok(())
    }
}

/// A pack and its committed versions, in version number order.
#[derive(Debug, Clone)]
pub struct Pack {
    pub meta: PackMeta,
    pub versions: Vec<VersionMeta>,
}

impl Pack {
    pub fn next_version_no(&self) -> Option<u16> {
        match self.versions.last() {
            None => Some(1),
            Some(v) if v.no < MAX_VERSIONS => Some(v.no + 1),
            Some(_) => None,
        }
    }
}

/// A name that is safe to use as one path component.
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// A `/`-separated relative path that stays inside its directory.
fn is_safe_relative(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(is_safe_name)
}

/// The name an upload is stored (and, if it is not an archive, served) under.
pub fn safe_file_name(filename: &str) -> String {
    let name = filename.replace(['/', '\\', '\0'], "_");
    if name.is_empty() || name == "." || name == ".." {
        "upload.bin".to_string()
    } else {
        name
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Copy `src` to a new file `dest`, front to back, returning the size and
/// SHA-256 of what was written.
pub fn copy_hashed(src: &Path, dest: &Path) -> Result<(u64, String)> {
    let mut from = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut to = File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = from
            .read(&mut buf)
            .with_context(|| format!("read {}", src.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        to.write_all(&buf[..n])
            .with_context(|| format!("write {}", dest.display()))?;
        size += n as u64;
    }
    // On an object store mount this is what completes the upload.
    to.sync_all()
        .with_context(|| format!("sync {}", dest.display()))?;
    Ok((size, hex(&hasher.finalize())))
}

/// Write a new file in one go.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .with_context(|| format!("write {}", path.display()))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// What reading `pack.json` or `version.json` found. The files are written in
/// place (the archive may be an object store mount, so there is no rename),
/// which means that a crash can leave one empty or cut short. Such a file
/// counts as not written, for readers and for the scanner alike.
enum Meta<T> {
    Missing,
    /// It is there, but it does not parse or does not describe its directory.
    Invalid(anyhow::Error),
    Valid(T),
}

impl<T> Meta<T> {
    fn into_result(self) -> Result<Option<T>> {
        match self {
            Meta::Missing => Ok(None),
            Meta::Invalid(e) => Err(e),
            Meta::Valid(meta) => Ok(Some(meta)),
        }
    }
}

/// Access to an archive directory, with an optional local copy of its (small,
/// immutable) metadata files so that each is read from the archive only once.
#[derive(Debug, Clone)]
pub struct Store {
    archive: PathBuf,
    meta_cache: Option<PathBuf>,
}

impl Store {
    pub fn new(archive: impl Into<PathBuf>, meta_cache: Option<PathBuf>) -> Self {
        Self {
            archive: archive.into(),
            meta_cache,
        }
    }

    pub fn archive_dir(&self) -> &Path {
        &self.archive
    }

    pub fn packs_dir(&self) -> PathBuf {
        self.archive.join(PACKS_DIR)
    }

    pub fn pack_dir(&self, pack: &str) -> PathBuf {
        self.packs_dir().join(pack)
    }

    pub fn version_dir(&self, pack: &str, version: &str) -> PathBuf {
        self.pack_dir(pack).join(version)
    }

    /// Where a file listed in `version.json` lives.
    pub fn file_path(&self, pack: &str, version: &str, file: &FileMeta) -> PathBuf {
        let mut path = self.version_dir(pack, version);
        path.extend(file.path.split('/'));
        path
    }

    // ---- reading ----

    /// A metadata file, by path relative to `packs/`: from the local copy if
    /// there is one, else from the archive (and then copied locally). `None`
    /// if it does not exist.
    fn read_meta(&self, rel: &Path) -> Result<Option<Vec<u8>>> {
        let cached = self.meta_cache.as_ref().map(|c| c.join(rel));
        if let Some(cached) = &cached
            && let Ok(bytes) = std::fs::read(cached)
        {
            return Ok(Some(bytes));
        }
        let path = self.packs_dir().join(rel);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        if let Some(cached) = &cached {
            self.cache_meta(cached, &bytes);
        }
        Ok(Some(bytes))
    }

    /// Best effort: the local copy only saves reads.
    fn cache_meta(&self, cached: &Path, bytes: &[u8]) {
        let write = || -> Result<()> {
            let dir = cached.parent().context("no parent")?;
            std::fs::create_dir_all(dir)?;
            let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
            tmp.write_all(bytes)?;
            tmp.persist(cached)?;
            Ok(())
        };
        if let Err(e) = write() {
            warn!("could not cache {}: {e:#}", cached.display());
        }
    }

    fn forget_meta(&self, rel: &Path) {
        if let Some(c) = &self.meta_cache {
            let _ = std::fs::remove_file(c.join(rel));
        }
    }

    /// `pack.json` of a pack directory, as far as it can be made out.
    fn read_pack(&self, pack: &str) -> Result<Meta<PackMeta>> {
        let rel = Path::new(pack).join(PACK_FILE);
        let Some(bytes) = self.read_meta(&rel)? else {
            return Ok(Meta::Missing);
        };
        let checked = || -> Result<PackMeta> {
            let meta: PackMeta = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", rel.display()))?;
            if meta.name != pack {
                bail!("{} is for a pack called {:?}", rel.display(), meta.name);
            }
            if meta.no == 0 || meta.no > MAX_PACKS {
                bail!("{} has pack number {}", rel.display(), meta.no);
            }
            Ok(meta)
        };
        Ok(match checked() {
            Ok(meta) => Meta::Valid(meta),
            Err(e) => {
                self.forget_meta(&rel);
                Meta::Invalid(e)
            }
        })
    }

    /// `version.json` of a version directory, as far as it can be made out.
    fn read_version(&self, pack: &str, version: &str) -> Result<Meta<VersionMeta>> {
        let rel = Path::new(pack).join(version).join(VERSION_FILE);
        let Some(bytes) = self.read_meta(&rel)? else {
            return Ok(Meta::Missing);
        };
        let checked = || -> Result<VersionMeta> {
            let meta: VersionMeta = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", rel.display()))?;
            meta.validate(version)
                .map_err(|e| anyhow!("{} {e}", rel.display()))?;
            Ok(meta)
        };
        Ok(match checked() {
            Ok(meta) => Meta::Valid(meta),
            Err(e) => {
                self.forget_meta(&rel);
                Meta::Invalid(e)
            }
        })
    }

    /// `pack.json` of a pack directory, if it has one.
    pub fn pack(&self, pack: &str) -> Result<Option<PackMeta>> {
        self.read_pack(pack)?.into_result()
    }

    /// `version.json` of a version directory, or `None` if the version was
    /// never committed.
    pub fn version(&self, pack: &str, version: &str) -> Result<Option<VersionMeta>> {
        self.read_version(pack, version)?.into_result()
    }

    /// The file listing of a committed version, checked against its checksum.
    pub fn manifest(&self, pack: &str, version: &VersionMeta) -> Result<Manifest> {
        let rel = Path::new(pack).join(&version.name).join(MANIFEST_FILE);
        let expected = version
            .file(MANIFEST_FILE)
            .with_context(|| format!("{pack}/{} lists no manifest", version.name))?;
        let bytes = self
            .read_meta(&rel)?
            .with_context(|| format!("{} is missing", rel.display()))?;
        if bytes.len() as u64 != expected.size || sha256_hex(&bytes) != expected.sha256 {
            self.forget_meta(&rel);
            bail!("{} does not match its checksum", rel.display());
        }
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", rel.display()))
    }

    /// Names of the directories in `dir`, sorted. Empty if it does not exist.
    fn subdirs(dir: &Path) -> Result<Vec<String>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("list {}", dir.display())),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("list {}", dir.display()))?;
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            match entry.file_name().into_string() {
                Ok(name) if is_safe_name(&name) => names.push(name),
                Ok(_) => {}
                Err(name) => warn!("ignoring {name:?} in {}", dir.display()),
            }
        }
        names.sort();
        Ok(names)
    }

    /// Every pack with a `pack.json`, and its committed versions. Costs one
    /// directory listing per pack, plus one read for each metadata file that
    /// is not in the local copy yet. Anything malformed is skipped with a
    /// warning rather than failing the whole archive.
    pub fn load(&self) -> Result<Vec<Pack>> {
        let mut packs: Vec<Pack> = Vec::new();
        for pack_name in Self::subdirs(&self.packs_dir())? {
            let meta = match self.pack(&pack_name) {
                Ok(Some(m)) => m,
                Ok(None) => continue,
                Err(e) => {
                    warn!("ignoring pack {pack_name}: {e:#}");
                    continue;
                }
            };
            if let Some(other) = packs
                .iter()
                .find(|p| p.meta.no == meta.no || p.meta.game_id == meta.game_id)
            {
                warn!(
                    "ignoring pack {pack_name}: it has the number or game id of {}",
                    other.meta.name
                );
                continue;
            }
            let versions = self.versions(&pack_name)?;
            packs.push(Pack { meta, versions });
        }
        packs.sort_by_key(|p| p.meta.no);
        Ok(packs)
    }

    /// The committed versions in a pack directory, in version number order,
    /// whether or not the pack has a readable `pack.json`. Anything malformed
    /// is skipped with a warning.
    pub fn versions(&self, pack_name: &str) -> Result<Vec<VersionMeta>> {
        let mut versions: Vec<VersionMeta> = Vec::new();
        for version_name in Self::subdirs(&self.pack_dir(pack_name))? {
            match self.version(pack_name, &version_name) {
                Ok(Some(v)) => match versions.iter().find(|x| x.no == v.no) {
                    Some(other) => warn!(
                        "ignoring version {pack_name}/{version_name}: it has the number of {}",
                        other.name
                    ),
                    None => versions.push(v),
                },
                Ok(None) => {}
                Err(e) => warn!("ignoring version {pack_name}/{version_name}: {e:#}"),
            }
        }
        versions.sort_by_key(|v| v.no);
        Ok(versions)
    }

    // ---- writing (scanner and migrate only) ----

    /// Write `pack.json`, unless the pack already has this one. A `pack.json`
    /// that cannot be read back (see [`Meta`]) is replaced; a readable one
    /// for another pack is an error, since nothing written under it would
    /// ever be found again.
    pub fn write_pack(&self, meta: &PackMeta) -> Result<()> {
        if !is_safe_name(&meta.name) {
            bail!("unsafe pack name {:?}", meta.name);
        }
        let dir = self.pack_dir(&meta.name);
        let path = dir.join(PACK_FILE);
        match self.read_pack(&meta.name)? {
            Meta::Missing => {}
            Meta::Valid(have) if have.no == meta.no && have.game_id == meta.game_id => {
                return Ok(());
            }
            Meta::Valid(have) => bail!(
                "{} already belongs to pack number {} (game {}), not number {} (game {})",
                path.display(),
                have.no,
                have.game_id,
                meta.no,
                meta.game_id
            ),
            Meta::Invalid(e) => warn!("replacing {}: {e:#}", path.display()),
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        write_new(&path, &serde_json::to_vec_pretty(meta)?)?;
        // Whatever the local copy held, it is not this.
        self.forget_meta(&Path::new(&meta.name).join(PACK_FILE));
        Ok(())
    }

    /// Commit a version: copy each `(path in the version directory, local
    /// file)` into the archive, then write `version.json`. `meta.files` and
    /// `meta.archived_at` are filled in here. Whatever an interrupted earlier
    /// attempt left behind is removed first, a `version.json` that cannot be
    /// read back (see [`Meta`]) included.
    pub fn commit_version(
        &self,
        pack: &str,
        mut meta: VersionMeta,
        files: &[(String, PathBuf)],
    ) -> Result<VersionMeta> {
        if !is_safe_name(&meta.name) {
            bail!("unsafe version name {:?}", meta.name);
        }
        let dir = self.version_dir(pack, &meta.name);
        let marker = dir.join(VERSION_FILE);
        match self.read_version(pack, &meta.name)? {
            Meta::Missing => {}
            Meta::Valid(_) => bail!("{pack}/{} is already committed", meta.name),
            // Readers do not count this as committed, so neither may we: it
            // would never be served and never be redone.
            Meta::Invalid(e) => {
                warn!("redoing {pack}/{}: {e:#}", meta.name);
                std::fs::remove_file(&marker)
                    .with_context(|| format!("remove {}", marker.display()))?;
            }
        }
        remove_files(&dir)?;
        meta.files.clear();
        for (rel, src) in files {
            if !is_safe_relative(rel) || rel == VERSION_FILE {
                bail!("unsafe file path {rel:?}");
            }
            let mut dest = dir.clone();
            dest.extend(rel.split('/'));
            let parent = dest.parent().context("no parent")?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            let (size, sha256) = copy_hashed(src, &dest)?;
            meta.files.push(FileMeta {
                path: rel.clone(),
                size,
                sha256,
            });
        }
        meta.archived_at = now_secs();
        meta.validate(&meta.name)
            .map_err(|e| anyhow!("{pack}/{}: version.json {e}", meta.name))?;
        let bytes = serde_json::to_vec_pretty(&meta)?;
        write_new(&marker, &bytes)?;
        if let Some(cache) = &self.meta_cache {
            self.cache_meta(
                &cache.join(pack).join(&meta.name).join(VERSION_FILE),
                &bytes,
            );
        }
        Ok(meta)
    }
}

/// Delete every file under `dir` (which need not exist), leaving directories.
fn remove_files(dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("list {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("list {}", dir.display()))?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            remove_files(&path)?;
        } else {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Entry;

    fn pack_meta(no: u32, name: &str) -> PackMeta {
        PackMeta {
            no,
            game_id: 500 + no as u64,
            name: name.into(),
            title: "Tiny Pack".into(),
            url: "https://creator.itch.io/tiny-pack".into(),
        }
    }

    fn version_meta(no: u16, name: &str) -> VersionMeta {
        VersionMeta {
            no,
            name: name.into(),
            upload_id: no as u64,
            build_id: None,
            fingerprint: format!("md5-{no}"),
            filename: "Tiny Pack.zip".into(),
            size: Some(5),
            timestamp: 1_700_000_000,
            archived_at: 0,
            kind: Kind::Zip,
            serve: "upload/Tiny Pack.zip".into(),
            upload: Some("upload/Tiny Pack.zip".into()),
            files: Vec::new(),
            note: None,
        }
    }

    /// Scratch files for one version: an "upload" and its manifest.
    fn sources(scratch: &Path, body: &[u8]) -> Vec<(String, PathBuf)> {
        let upload = scratch.join("upload");
        let manifest = scratch.join("manifest.json");
        std::fs::write(&upload, body).unwrap();
        let m = Manifest {
            kind: Kind::Zip,
            entries: vec![Entry {
                path: "a.png".into(),
                size: 4,
                crc: 7,
                mtime: 0,
                index: 0,
            }],
        };
        std::fs::write(&manifest, serde_json::to_vec(&m).unwrap()).unwrap();
        vec![
            ("upload/Tiny Pack.zip".into(), upload),
            (MANIFEST_FILE.into(), manifest),
        ]
    }

    #[test]
    fn round_trip() {
        let (archive, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Store::new(archive.path(), None);
        assert!(
            store.load().unwrap().is_empty(),
            "an empty archive is not an error"
        );

        store
            .write_pack(&pack_meta(1, "creator.tiny-pack"))
            .unwrap();
        let files = sources(scratch.path(), b"hello");
        let v1 = store
            .commit_version("creator.tiny-pack", version_meta(1, "v1"), &files)
            .unwrap();
        let v2 = store
            .commit_version("creator.tiny-pack", version_meta(2, "v2"), &files)
            .unwrap();
        assert!(v1.archived_at > 0);
        let upload = v1.file("upload/Tiny Pack.zip").unwrap();
        assert_eq!(upload.size, 5);
        assert_eq!(
            upload.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            std::fs::read(store.file_path("creator.tiny-pack", "v1", upload)).unwrap(),
            b"hello"
        );

        let packs = store.load().unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].meta, pack_meta(1, "creator.tiny-pack"));
        assert_eq!(packs[0].versions, [v1.clone(), v2]);
        assert_eq!(packs[0].next_version_no(), Some(3));
        let manifest = store.manifest("creator.tiny-pack", &v1).unwrap();
        assert_eq!(manifest.entries[0].path, "a.png");

        // On disk exactly the documented layout, and nothing else.
        let mut on_disk = Vec::new();
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, root, out);
                } else {
                    out.push(p.strip_prefix(root).unwrap().to_str().unwrap().to_string());
                }
            }
        }
        walk(archive.path(), archive.path(), &mut on_disk);
        on_disk.sort();
        assert_eq!(
            on_disk,
            [
                "packs/creator.tiny-pack/pack.json",
                "packs/creator.tiny-pack/v1/manifest.json",
                "packs/creator.tiny-pack/v1/upload/Tiny Pack.zip",
                "packs/creator.tiny-pack/v1/version.json",
                "packs/creator.tiny-pack/v2/manifest.json",
                "packs/creator.tiny-pack/v2/upload/Tiny Pack.zip",
                "packs/creator.tiny-pack/v2/version.json",
            ]
        );
    }

    #[test]
    fn a_version_without_its_commit_marker_is_ignored_and_can_be_redone() {
        let (archive, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Store::new(archive.path(), None);
        store.write_pack(&pack_meta(1, "p")).unwrap();
        let files = sources(scratch.path(), b"hello");
        store
            .commit_version("p", version_meta(1, "v1"), &files)
            .unwrap();

        // An interrupted version: files, but no version.json.
        let dir = store.version_dir("p", "v2");
        std::fs::create_dir_all(dir.join("upload")).unwrap();
        std::fs::write(dir.join("upload/Tiny Pack.zip"), b"hel").unwrap();
        std::fs::write(dir.join("content.zip"), b"stale").unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), b"{").unwrap();

        let packs = store.load().unwrap();
        assert_eq!(packs[0].versions.len(), 1, "v2 is not committed");
        assert!(store.version("p", "v2").unwrap().is_none());
        assert_eq!(packs[0].next_version_no(), Some(2));

        // Redoing it replaces the leftovers.
        let v2 = store
            .commit_version("p", version_meta(2, "v2"), &files)
            .unwrap();
        assert!(!dir.join("content.zip").exists(), "leftovers are removed");
        assert_eq!(
            std::fs::read(dir.join("upload/Tiny Pack.zip")).unwrap(),
            b"hello"
        );
        assert_eq!(store.load().unwrap()[0].versions[1], v2);
    }

    #[test]
    fn committed_versions_are_never_rewritten() {
        let (archive, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Store::new(archive.path(), None);
        store.write_pack(&pack_meta(1, "p")).unwrap();
        let files = sources(scratch.path(), b"hello");
        store
            .commit_version("p", version_meta(1, "v1"), &files)
            .unwrap();
        let files = sources(scratch.path(), b"other");
        let err = store
            .commit_version("p", version_meta(1, "v1"), &files)
            .unwrap_err();
        assert!(err.to_string().contains("already committed"), "{err:#}");
        assert_eq!(
            std::fs::read(store.version_dir("p", "v1").join("upload/Tiny Pack.zip")).unwrap(),
            b"hello"
        );
        // pack.json is written once too.
        let mut renamed = pack_meta(1, "p");
        renamed.title = "Other".into();
        store.write_pack(&renamed).unwrap();
        assert_eq!(store.pack("p").unwrap().unwrap().title, "Tiny Pack");
    }

    /// The files are written in place, so a crash can leave one empty or
    /// cut short. Readers skip such a file; the writer must not take it for
    /// the real thing, or the version is neither served nor ever redone.
    #[test]
    fn a_commit_marker_that_cannot_be_read_is_replaced() {
        for junk in [&b""[..], b"{\"no\": 1, \"na"] {
            let (archive, scratch, cache) = (
                tempfile::tempdir().unwrap(),
                tempfile::tempdir().unwrap(),
                tempfile::tempdir().unwrap(),
            );
            let store = Store::new(archive.path(), Some(cache.path().to_path_buf()));
            store.write_pack(&pack_meta(1, "p")).unwrap();
            let dir = store.version_dir("p", "v1");
            std::fs::create_dir_all(dir.join("upload")).unwrap();
            std::fs::write(dir.join("upload/Tiny Pack.zip"), b"hal").unwrap();
            std::fs::write(dir.join(VERSION_FILE), junk).unwrap();
            assert!(store.load().unwrap()[0].versions.is_empty());

            let files = sources(scratch.path(), b"hello");
            let v1 = store
                .commit_version("p", version_meta(1, "v1"), &files)
                .unwrap();
            assert_eq!(store.load().unwrap()[0].versions, std::slice::from_ref(&v1));
            // Another reader, with no local copies, agrees.
            let fresh = Store::new(archive.path(), None);
            assert_eq!(fresh.load().unwrap()[0].versions, std::slice::from_ref(&v1));
            assert_eq!(
                std::fs::read(fresh.file_path("p", "v1", &v1.files[0])).unwrap(),
                b"hello"
            );
            // And now that it can be read, it is committed for good.
            assert!(
                store
                    .commit_version("p", version_meta(1, "v1"), &files)
                    .is_err()
            );
        }
    }

    #[test]
    fn a_pack_json_that_cannot_be_read_is_replaced() {
        for junk in [&b""[..], b"{\"no\": 1, \"na"] {
            let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let store = Store::new(archive.path(), Some(cache.path().to_path_buf()));
            std::fs::create_dir_all(store.pack_dir("p")).unwrap();
            std::fs::write(store.pack_dir("p").join(PACK_FILE), junk).unwrap();
            assert!(store.load().unwrap().is_empty());

            store.write_pack(&pack_meta(1, "p")).unwrap();
            assert_eq!(store.load().unwrap()[0].meta, pack_meta(1, "p"));
            let fresh = Store::new(archive.path(), None);
            assert_eq!(fresh.load().unwrap()[0].meta, pack_meta(1, "p"));
        }
    }

    /// Writing under a `pack.json` that is someone else's would put versions
    /// where no reader looks for them.
    #[test]
    fn a_pack_json_of_another_pack_is_not_taken_for_ours() {
        let archive = tempfile::tempdir().unwrap();
        let store = Store::new(archive.path(), None);
        store.write_pack(&pack_meta(1, "p")).unwrap();
        store.write_pack(&pack_meta(1, "p")).unwrap();
        let err = store.write_pack(&pack_meta(2, "p")).unwrap_err();
        assert!(format!("{err:#}").contains("already belongs to"), "{err:#}");
        assert_eq!(store.load().unwrap()[0].meta, pack_meta(1, "p"));
    }

    #[test]
    fn metadata_is_read_from_the_archive_once() {
        let (archive, scratch, cache) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        // Written by a scanner with no local copy of the metadata.
        let writer = Store::new(archive.path(), None);
        writer.write_pack(&pack_meta(1, "p")).unwrap();
        let v1 = writer
            .commit_version(
                "p",
                version_meta(1, "v1"),
                &sources(scratch.path(), b"hello"),
            )
            .unwrap();

        let reader = Store::new(archive.path(), Some(cache.path().to_path_buf()));
        let first = reader.load().unwrap();
        reader.manifest("p", &v1).unwrap();
        // The metadata files can now vanish from the archive (the directories
        // must still list): everything is answered from the local copy.
        for f in ["pack.json", "v1/version.json", "v1/manifest.json"] {
            std::fs::remove_file(archive.path().join("packs/p").join(f)).unwrap();
        }
        let second = reader.load().unwrap();
        assert_eq!(second[0].meta, first[0].meta);
        assert_eq!(second[0].versions, first[0].versions);
        reader.manifest("p", &v1).unwrap();
        // Without the local copy the pack is gone.
        assert!(writer.load().unwrap().is_empty());
    }

    #[test]
    fn malformed_metadata_is_skipped() {
        let (archive, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Store::new(archive.path(), None);
        let files = sources(scratch.path(), b"hello");
        store.write_pack(&pack_meta(1, "good")).unwrap();
        store
            .commit_version("good", version_meta(1, "v1"), &files)
            .unwrap();
        // Same version number twice: the first by name wins.
        store
            .commit_version("good", version_meta(1, "v1-again"), &files)
            .unwrap();
        // A version.json pointing outside its directory.
        let mut evil = version_meta(2, "evil");
        evil.files = vec![
            FileMeta {
                path: "../../x".into(),
                size: 0,
                sha256: String::new(),
            },
            FileMeta {
                path: MANIFEST_FILE.into(),
                size: 0,
                sha256: String::new(),
            },
        ];
        evil.serve = "../../x".into();
        evil.upload = None;
        let dir = store.version_dir("good", "evil");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(VERSION_FILE), serde_json::to_vec(&evil).unwrap()).unwrap();
        // A version.json copied into a directory of another name.
        let dir = store.version_dir("good", "v9");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(
            store.version_dir("good", "v1").join(VERSION_FILE),
            dir.join(VERSION_FILE),
        )
        .unwrap();
        // A second pack with the same number, one with garbage, one without pack.json.
        store.write_pack(&pack_meta(1, "twin")).unwrap();
        std::fs::create_dir_all(store.pack_dir("garbage")).unwrap();
        std::fs::write(store.pack_dir("garbage").join(PACK_FILE), b"not json").unwrap();
        std::fs::create_dir_all(store.pack_dir("empty")).unwrap();

        let packs = store.load().unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].meta.name, "good");
        let names: Vec<_> = packs[0].versions.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["v1"]);
    }

    #[test]
    fn a_corrupt_manifest_is_refused() {
        let (archive, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Store::new(archive.path(), None);
        store.write_pack(&pack_meta(1, "p")).unwrap();
        let v1 = store
            .commit_version(
                "p",
                version_meta(1, "v1"),
                &sources(scratch.path(), b"hello"),
            )
            .unwrap();
        std::fs::write(store.version_dir("p", "v1").join(MANIFEST_FILE), b"{}").unwrap();
        let err = store.manifest("p", &v1).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err:#}");
    }

    #[test]
    fn upload_names_are_made_safe() {
        assert_eq!(safe_file_name("Tiny Pack.zip"), "Tiny Pack.zip");
        assert_eq!(safe_file_name("../a/b\\c"), ".._a_b_c");
        assert_eq!(safe_file_name(".."), "upload.bin");
        assert_eq!(safe_file_name(""), "upload.bin");
    }
}
