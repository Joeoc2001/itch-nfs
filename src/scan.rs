//! The scanner: polls itch.io and archives every upload it has not seen yet.
//!
//! It is the only part that holds the itch.io token, and the only writer to
//! the archive directory. What it has already archived is read back from the
//! archive itself, so it keeps no durable state of its own.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use tracing::{info, warn};

use crate::archive::{self, Kind};
use crate::ingest::{self, Limits, Outcome};
use crate::itch::{ItchClient, OwnedGame, Upload};
use crate::state;
use crate::store::{
    AlreadyCommitted, CONTENT_FILE, MANIFEST_FILE, MAX_PACKS, NumberTaken, PackMeta, Store,
    UPLOAD_DIR, VersionMeta, safe_file_name,
};

/// How many archives-within-archives are unpacked.
pub const MAX_ARCHIVE_DEPTH: u32 = 8;

pub struct ScanConfig {
    /// The durable archive. Only the scanner writes to it.
    pub archive_dir: PathBuf,
    /// Local scratch space, and a local copy of the archive's metadata.
    pub cache_dir: PathBuf,
    /// Cap on the decompressed size of a single upload. Uploads over it are
    /// archived, but served as the upload itself rather than unpacked.
    pub max_unpacked_bytes: u64,
    /// Pack directory names to restrict the scan to. Empty: every pack.
    pub only: Vec<String>,
    /// itch.io classifications that are in scope. Empty: all of them. A pack
    /// itch.io gives no classification for is always in scope.
    pub classifications: Vec<String>,
    /// How many packs are scanned (and downloaded) at once.
    pub concurrency: usize,
}

/// An upload that is not in the archive yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub pack: String,
    pub version: String,
    pub filename: String,
    pub size: Option<u64>,
}

/// What one pass did (or, for a dry run, would do).
#[derive(Debug, Default)]
pub struct Report {
    /// Packs in scope.
    pub packs: usize,
    /// `<pack>/<version>` of everything committed in this pass.
    pub archived: Vec<String>,
    /// One message per thing that went wrong. They are retried next pass.
    pub failed: Vec<String>,
    /// Dry runs only: what a real pass would archive.
    pub pending: Vec<Pending>,
}

/// What makes two listings of an upload id the same content. itch.io lets a
/// creator replace the file behind an upload id.
pub fn fingerprint(u: &Upload) -> String {
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A version about to be archived: everything in `version.json` that is known
/// before the download.
#[derive(Debug, Clone)]
pub struct VersionPlan {
    pub no: u16,
    pub name: String,
    pub upload_id: u64,
    pub build_id: Option<u64>,
    pub fingerprint: String,
    pub filename: String,
    pub size: Option<u64>,
    pub timestamp: i64,
}

impl VersionPlan {
    /// `name` is left empty: it depends on the names already taken.
    fn of(u: &Upload, no: u16) -> Self {
        Self {
            no,
            name: String::new(),
            upload_id: u.id,
            build_id: u.build_id,
            fingerprint: fingerprint(u),
            filename: u.filename.clone(),
            size: u.size,
            timestamp: state::parse_ts(u.updated_at.as_deref())
                .or(state::parse_ts(u.created_at.as_deref()))
                .unwrap_or_else(now_secs),
        }
    }
}

/// The version directory name for an upload: today's naming, made unique
/// among `taken`.
pub fn unique_version_name(plan: &VersionPlan, taken: &[String]) -> String {
    let base = state::version_dir_name(
        plan.timestamp,
        &plan.filename,
        plan.upload_id,
        plan.build_id,
    );
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

/// A version built in local scratch space, ready to be copied into the
/// archive. The scratch directory is deleted when this is dropped.
pub struct Prepared {
    meta: VersionMeta,
    files: Vec<(String, PathBuf)>,
    scratch: PathBuf,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

impl Prepared {
    /// Copy the files into the archive and write the commit marker.
    pub async fn commit(self, store: &Store, pack: &str) -> Result<VersionMeta> {
        let (store, pack) = (store.clone(), pack.to_string());
        tokio::task::spawn_blocking(move || {
            store.commit_version(&pack, self.meta.clone(), &self.files)
        })
        .await?
    }
}

/// Turn an upload sitting in `scratch/upload` into the files of a version:
/// unpack it, list it, and say how it is to be served.
fn build(scratch: &Path, plan: &VersionPlan, limits: Limits) -> Result<Prepared> {
    let upload = scratch.join("upload");
    let content = scratch.join(CONTENT_FILE);
    let tmp = scratch.join("tmp");
    std::fs::create_dir_all(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut note = None;
    let outcome = match ingest::ingest(&upload, &plan.filename, &content, &tmp, limits) {
        Ok(o) => o,
        Err(e) if ingest::is_too_big(&e) => {
            let _ = std::fs::remove_file(&content);
            warn!(
                "{}: decompresses to more than the limit, archiving it as a single file",
                plan.name
            );
            note = Some(format!(
                "The upload decompresses to more than the limit of {} bytes that was in force when it was archived, so it is served as the upload itself rather than unpacked.",
                limits.max_unpacked_bytes
            ));
            Outcome::Raw
        }
        Err(e) => return Err(e),
    };
    let upload_rel = format!("{UPLOAD_DIR}/{}", safe_file_name(&plan.filename));
    let (kind, serve_local, serve) = match outcome {
        Outcome::ZipAsIs => (Kind::Zip, &upload, upload_rel.clone()),
        Outcome::Flattened => (Kind::Zip, &content, CONTENT_FILE.to_string()),
        Outcome::Raw => (Kind::Raw, &upload, upload_rel.clone()),
    };
    let manifest = archive::scan(serve_local, &plan.filename, kind == Kind::Zip)?;
    let manifest_path = scratch.join(MANIFEST_FILE);
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest)?)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    let mut files = vec![(upload_rel.clone(), upload.clone())];
    if outcome == Outcome::Flattened {
        files.push((CONTENT_FILE.to_string(), content));
    }
    files.push((MANIFEST_FILE.to_string(), manifest_path));
    Ok(Prepared {
        meta: VersionMeta {
            no: plan.no,
            name: plan.name.clone(),
            upload_id: plan.upload_id,
            build_id: plan.build_id,
            fingerprint: plan.fingerprint.clone(),
            filename: plan.filename.clone(),
            size: plan.size,
            timestamp: plan.timestamp,
            archived_at: 0, // set when committed
            kind,
            serve,
            upload: Some(upload_rel),
            files: Vec::new(), // filled in when committed
            note,
        },
        files,
        scratch: scratch.to_path_buf(),
    })
}

/// Download an upload into scratch space under `scratch_root` and build its
/// version there. Nothing is written to the archive.
pub async fn prepare(
    client: &ItchClient,
    scratch_root: &Path,
    limits: Limits,
    game_id: u64,
    key_id: u64,
    plan: &VersionPlan,
) -> Result<Prepared> {
    let scratch = scratch_root.join(format!("{game_id}-{}", plan.upload_id));
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    tokio::fs::create_dir_all(&scratch)
        .await
        .with_context(|| format!("create {}", scratch.display()))?;
    let result = async {
        info!(
            "downloading {} ({} bytes)",
            plan.name,
            plan.size.map_or("?".into(), |s| s.to_string())
        );
        let n = client
            .download(plan.upload_id, key_id, &scratch.join("upload"))
            .await?;
        if let Some(size) = plan.size
            && plan.build_id.is_none()
            && size != n
        {
            bail!("{}: downloaded {n} bytes, expected {size}", plan.name);
        }
        // The file behind an upload id can be replaced at any moment, and what
        // is archived can never be corrected. So make sure that what was just
        // downloaded is still what itch.io describes.
        let still_current = client
            .uploads(game_id, key_id)
            .await?
            .iter()
            .any(|u| u.id == plan.upload_id && plan.matches(u));
        if !still_current {
            bail!("{}: the upload changed while it was downloaded", plan.name);
        }
        let (dir, plan) = (scratch.clone(), plan.clone());
        tokio::task::spawn_blocking(move || build(&dir, &plan, limits)).await?
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_dir_all(&scratch).await;
    }
    result
}

impl VersionPlan {
    fn matches(&self, u: &Upload) -> bool {
        self.upload_id == u.id && self.build_id == u.build_id && self.fingerprint == fingerprint(u)
    }
}

/// A pack in scope for this pass.
struct PackPlan {
    game: OwnedGame,
    name: String,
    /// Its `pack.json`, if the archive has a readable one.
    meta: Option<PackMeta>,
    /// Every version the archive holds for it, or why that is not known.
    versions: Result<Vec<VersionMeta>, String>,
}

/// Where pack numbers come from during a pass.
struct PackNumbers {
    next: u32,
    /// Why no number may be given out in this pass: some `pack.json` could
    /// not be read, and it may hold any number.
    blocked: Option<String>,
}

pub struct Scanner {
    cfg: ScanConfig,
    client: ItchClient,
    store: Store,
    scratch: PathBuf,
}

impl Scanner {
    pub fn new(cfg: ScanConfig, client: ItchClient) -> Result<Self> {
        // Nothing is created or deleted here: a dry run must leave the
        // scratch directory of a scanner at work alone.
        let scratch = cfg.cache_dir.join("scan-tmp");
        let store = Store::new(&cfg.archive_dir, Some(cfg.cache_dir.join("meta")));
        Ok(Self {
            cfg,
            client,
            store,
            scratch,
        })
    }

    fn limits(&self) -> Limits {
        Limits {
            max_unpacked_bytes: self.cfg.max_unpacked_bytes,
            max_depth: MAX_ARCHIVE_DEPTH,
        }
    }

    fn in_scope(&self, g: &OwnedGame) -> bool {
        match &g.classification {
            Some(c) if !self.cfg.classifications.is_empty() => self
                .cfg
                .classifications
                .iter()
                .any(|x| x.eq_ignore_ascii_case(c)),
            _ => true,
        }
    }

    /// One pass: archive every upload of every pack in scope that is not in
    /// the archive yet. Fails only if the library cannot be listed or the
    /// archive cannot be read; anything narrower is reported in
    /// [`Report::failed`] and does not stop the other packs.
    pub async fn pass(&self) -> Result<Report> {
        self.run(false).await
    }

    /// A dry run: what [`Scanner::pass`] would archive. Downloads nothing and
    /// writes nothing.
    pub async fn list(&self) -> Result<Report> {
        self.run(true).await
    }

    async fn run(&self, dry: bool) -> Result<Report> {
        if !dry {
            // Whatever is in the scratch directory is from a pass that died:
            // there is one scanner, and its passes do not overlap.
            let _ = tokio::fs::remove_dir_all(&self.scratch).await;
            tokio::fs::create_dir_all(&self.scratch)
                .await
                .with_context(|| format!("create {}", self.scratch.display()))?;
        }
        // Numbers are given out from this view of the archive, so it has to
        // be complete: what could not be read is not "not archived".
        let store = self.store.clone();
        let surveyed = tokio::task::spawn_blocking(move || store.survey())
            .await?
            .context("read the archive")?;
        let mut numbers = PackNumbers {
            next: 1,
            blocked: None,
        };
        // The packs with a readable pack.json, as `Store::load` picks them.
        let mut known: Vec<&PackMeta> = Vec::new();
        for dir in &surveyed {
            match &dir.meta {
                Ok(Some(meta)) => {
                    numbers.next = numbers.next.max(meta.no + 1);
                    match known
                        .iter()
                        .find(|k| k.no == meta.no || k.game_id == meta.game_id)
                    {
                        Some(other) => warn!(
                            "ignoring pack {}: it has the number or game id of {}",
                            dir.name, other.name
                        ),
                        None => known.push(meta),
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("pack {}: {e:#}", dir.name);
                    numbers.blocked.get_or_insert_with(|| {
                        format!(
                            "{} of {} could not be read, so no pack can be numbered in this pass",
                            crate::store::PACK_FILE,
                            dir.name
                        )
                    });
                }
            }
        }
        let owned = self
            .client
            .owned_games()
            .await
            .context("list owned packs")?;

        // Names are settled here, one pack after the other, so that they do
        // not depend on the order the packs happen to be scanned in.
        let mut names: HashSet<String> = known.iter().map(|p| p.name.clone()).collect();
        let mut seen = HashSet::new();
        let mut plans = Vec::new();
        let mut skipped = 0;
        for game in owned {
            if !seen.insert(game.game_id) {
                continue; // a second key for the same pack
            }
            if !self.in_scope(&game) {
                skipped += 1;
                continue;
            }
            let meta = known.iter().find(|p| p.game_id == game.game_id).copied();
            let name = match meta {
                Some(p) => p.name.clone(),
                None => {
                    let mut name = state::game_dir_name(&game.url, game.game_id);
                    if names.contains(&name) {
                        name = format!("{name}-{}", game.game_id);
                    }
                    names.insert(name.clone());
                    name
                }
            };
            // Without a readable pack.json the directory of that name is
            // still this pack's: its versions are not downloaded again.
            let versions = match surveyed.iter().find(|d| d.name == name) {
                Some(dir) => match &dir.versions {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(format!("{e:#}")),
                },
                None => Ok(Vec::new()),
            };
            plans.push(PackPlan {
                game,
                name,
                meta: meta.cloned(),
                versions,
            });
        }
        if skipped > 0 {
            info!("{skipped} owned project(s) are out of scope by classification");
        }
        let mut report = Report::default();
        if !self.cfg.only.is_empty() {
            for wanted in &self.cfg.only {
                if !plans.iter().any(|p| p.name == *wanted) {
                    report
                        .failed
                        .push(format!("{wanted}: no such pack in the library"));
                }
            }
            plans.retain(|p| self.cfg.only.contains(&p.name));
        }
        report.packs = plans.len();

        let next_pack_no = Mutex::new(numbers);
        let report = Mutex::new(report);
        futures_util::stream::iter(plans)
            .for_each_concurrent(self.cfg.concurrency.max(1), |plan| {
                let (report, next_pack_no) = (&report, &next_pack_no);
                async move {
                    let mut local = Report::default();
                    self.scan_pack(plan, dry, next_pack_no, &mut local).await;
                    let mut r = report.lock().unwrap();
                    r.archived.append(&mut local.archived);
                    r.failed.append(&mut local.failed);
                    r.pending.append(&mut local.pending);
                }
            })
            .await;
        let mut report = report.into_inner().unwrap();
        report.archived.sort();
        report.pending.sort_by(|a, b| {
            (&a.pack, &a.version, &a.filename).cmp(&(&b.pack, &b.version, &b.filename))
        });
        for f in &report.failed {
            warn!("{f}");
        }
        Ok(report)
    }

    async fn scan_pack(
        &self,
        plan: PackPlan,
        dry: bool,
        next_pack_no: &Mutex<PackNumbers>,
        report: &mut Report,
    ) {
        let PackPlan {
            game,
            name,
            mut meta,
            versions,
        } = plan;
        // Checked before anything else: with an incomplete list of versions,
        // committed ones would be downloaded again and their numbers reused.
        let versions = match versions {
            Ok(v) => v,
            Err(e) => {
                report
                    .failed
                    .push(format!("{name}: could not read the archive: {e}"));
                return;
            }
        };
        let uploads = match self.client.uploads(game.game_id, game.key_id).await {
            Ok(u) => u,
            Err(e) => {
                report
                    .failed
                    .push(format!("{name}: could not list uploads: {e:#}"));
                return;
            }
        };
        let mut taken: Vec<String> = versions.iter().map(|v| v.name.clone()).collect();
        let mut next_no = versions.last().map_or(0, |v| v.no);
        for pair in versions.windows(2) {
            if pair[0].no == pair[1].no {
                warn!(
                    "{name}: {} and {} are both version {}, only the first is served",
                    pair[0].name, pair[1].name, pair[0].no
                );
            }
        }
        let mut new: Vec<VersionPlan> = uploads
            .iter()
            .filter(|u| {
                !versions
                    .iter()
                    .any(|v| v.is_upload(u.id, u.build_id, &fingerprint(u)))
            })
            .map(|u| VersionPlan::of(u, 0))
            .collect();
        // Oldest first: later versions are de-duplicated against earlier ones.
        new.sort_by_key(|v| (v.timestamp, v.upload_id));

        if meta.is_none() && (!versions.is_empty() || !new.is_empty()) {
            // The pack needs a number. Find out whether it can have one
            // before downloading anything.
            let blocked = next_pack_no.lock().unwrap().blocked.clone();
            if let Some(why) = blocked {
                report.failed.push(format!("{name}: {why}"));
                return;
            }
            // Committed versions without a readable pack.json (it was cut
            // short by a crash) are not downloaded again, and pack.json is
            // written again right away so that they are served.
            if !versions.is_empty() && !dry {
                match self.new_pack(&game, &name, next_pack_no).await {
                    Ok(m) => meta = Some(m),
                    Err(e) => {
                        report.failed.push(format!("{name}: {e:#}"));
                        return;
                    }
                }
            }
        }

        for mut v in new {
            let Some(no) = next_no.checked_add(1) else {
                report
                    .failed
                    .push(format!("{name}: too many versions, ignoring new uploads"));
                break;
            };
            v.no = no;
            v.name = unique_version_name(&v, &taken);
            if dry {
                report.pending.push(Pending {
                    pack: name.clone(),
                    version: v.name.clone(),
                    filename: v.filename.clone(),
                    size: v.size,
                });
                taken.push(v.name);
                next_no = no;
                continue;
            }
            let archived = async {
                let prepared = prepare(
                    &self.client,
                    &self.scratch,
                    self.limits(),
                    game.game_id,
                    game.key_id,
                    &v,
                )
                .await?;
                // pack.json is only written once there is a version to put
                // next to it, so pack numbers go to packs that have content.
                if meta.is_none() {
                    meta = Some(self.new_pack(&game, &name, next_pack_no).await?);
                }
                prepared.commit(&self.store, &name).await
            }
            .await;
            match archived {
                Ok(done) => {
                    info!("archived {name}/{}", done.name);
                    report.archived.push(format!("{name}/{}", done.name));
                    taken.push(done.name);
                    next_no = no;
                }
                Err(e) => {
                    let stale = e
                        .chain()
                        .any(|c| c.is::<AlreadyCommitted>() || c.is::<NumberTaken>());
                    report.failed.push(format!("{name}/{}: {e:#}", v.name));
                    if stale {
                        // The archive holds a version this pass did not know
                        // of, so which numbers are free is not known either.
                        report.failed.push(format!(
                            "{name}: the archive is not as it was read, leaving the pack for the next pass"
                        ));
                        break;
                    }
                    // Otherwise the number and the name stay free: nothing
                    // was committed under them.
                }
            }
        }
    }

    /// Give a pack its number and write its `pack.json`.
    async fn new_pack(
        &self,
        game: &OwnedGame,
        name: &str,
        next_pack_no: &Mutex<PackNumbers>,
    ) -> Result<PackMeta> {
        let no = {
            let mut numbers = next_pack_no.lock().unwrap();
            if let Some(why) = &numbers.blocked {
                bail!("{why}");
            }
            if numbers.next > MAX_PACKS {
                bail!("too many packs");
            }
            numbers.next += 1;
            numbers.next - 1
        };
        let meta = PackMeta {
            no,
            game_id: game.game_id,
            name: name.to_string(),
            title: game.title.clone(),
            url: game.url.clone(),
        };
        info!("new pack: {name}");
        let (store, m) = (self.store.clone(), meta.clone());
        tokio::task::spawn_blocking(move || store.write_pack(&m)).await??;
        Ok(meta)
    }
}
