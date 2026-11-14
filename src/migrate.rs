//! Importing a deployment from before the archive directory existed.
//!
//! Back then one process did everything, and kept `state.json` and
//! `manifests/` in its state directory and `<game id>-<version>.zip|raw` files
//! in what it called its archive directory (a disposable cache). `migrate`
//! writes all of that into the archive layout with the same pack names,
//! version names and numbers, so that exported paths and NFS file handles
//! stay what they were. The old directories are only read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::archive::{self, Kind, Manifest};
use crate::ingest::{self, Limits};
use crate::itch::{ItchClient, Upload};
use crate::scan::{self, MAX_ARCHIVE_DEPTH, VersionPlan};
use crate::state::{GameState, State, VersionState};
use crate::store::{
    CONTENT_FILE, MANIFEST_FILE, MAX_PACKS, PackMeta, Store, UPLOAD_DIR, VersionMeta, is_safe_name,
    safe_file_name,
};

pub struct MigrateConfig {
    /// The old `--state-dir`: `state.json` and `manifests/`.
    pub state_dir: PathBuf,
    /// The old `--archive-dir`: `<game id>-<version>.zip|raw`.
    pub old_archive_dir: PathBuf,
    /// The archive to import into.
    pub archive_dir: PathBuf,
    /// Local scratch space.
    pub cache_dir: PathBuf,
    pub max_unpacked_bytes: u64,
}

#[derive(Debug, Default)]
pub struct MigrateReport {
    pub packs: usize,
    /// `<pack>/<version>` imported from the old directories.
    pub imported: Vec<String>,
    /// Versions the old server had listed but never downloaded, fetched now.
    pub fetched: Vec<String>,
    /// Versions that were in the archive already (from an earlier run).
    pub already: usize,
    /// Versions that could not be imported: listed, never downloaded, and no
    /// longer (or not verifiably) available. Their numbers stay unused.
    pub gaps: Vec<String>,
}

/// If this looks like a deployment from before the archive directory existed
/// that has not been migrated, say what gave it away. `old_state_dir` is
/// where the old `--state-dir` pointed, if that is still configured.
pub fn legacy_deployment(
    archive_dir: &Path,
    cache_dir: &Path,
    old_state_dir: Option<&Path>,
) -> Option<String> {
    if archive_dir.join(crate::store::PACKS_DIR).exists() {
        return None; // migrated, or started afresh
    }
    let mut state_dirs = vec![
        archive_dir.to_path_buf(),
        cache_dir.to_path_buf(),
        PathBuf::from("./itch-nfs-state"), // the old default
    ];
    if let Some(dir) = old_state_dir {
        state_dirs.insert(0, dir.to_path_buf());
    }
    for dir in state_dirs {
        let state = dir.join("state.json");
        if state.is_file() {
            return Some(format!("{} exists", state.display()));
        }
    }
    let old_archive = std::fs::read_dir(archive_dir).ok()?.flatten().find(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| old_archive_name(n).is_some())
    })?;
    Some(format!(
        "{} is a download from the old layout",
        old_archive.path().display()
    ))
}

/// `<game id>-<version>.zip|raw` -> (game id, version, is a zip).
fn old_archive_name(name: &str) -> Option<(u64, u16, bool)> {
    let (stem, ext) = name.rsplit_once('.')?;
    let zip = match ext {
        "zip" => true,
        "raw" => false,
        _ => return None,
    };
    let (game, ver) = stem.split_once('-')?;
    Some((game.parse().ok()?, ver.parse().ok()?, zip))
}

/// Import one version whose download is still in the old archive directory.
fn import_local(
    store: &Store,
    cfg: &MigrateConfig,
    g: &GameState,
    v: &VersionState,
    ver: u16,
    file: &Path,
    zip: bool,
) -> Result<VersionMeta> {
    let kind = if zip { Kind::Zip } else { Kind::Raw };
    let old_manifest = cfg
        .state_dir
        .join("manifests")
        .join(format!("{}-{ver}.json", g.game_id));
    let manifest = std::fs::read(&old_manifest)
        .ok()
        .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
        .filter(|m| m.kind == kind);
    let manifest = match manifest {
        Some(m) => m,
        // Never listed (or the listing is unreadable): list it now.
        None => archive::scan(file, &v.filename, zip)?,
    };
    let scratch = tempfile::tempdir_in(&cfg.cache_dir)
        .with_context(|| format!("create a directory in {}", cfg.cache_dir.display()))?;
    let manifest_path = scratch.path().join(MANIFEST_FILE);
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;

    // The old server kept a plain zip as downloaded, and replaced anything
    // else with the zip it was flattened into. Nothing recorded which, so a
    // zip only counts as the original if it has the upload's name and size.
    let size = std::fs::metadata(file)?.len();
    let original =
        !zip || (ingest::archive_suffix(&v.filename) == Some(".zip") && v.size == Some(size));
    let (rel, upload, note) = if original {
        let rel = format!("{UPLOAD_DIR}/{}", safe_file_name(&v.filename));
        (rel.clone(), Some(rel), None)
    } else {
        (
            CONTENT_FILE.to_string(),
            None,
            Some(
                "Imported by `itch-nfs migrate` from a deployment that did not keep original uploads: only the flattened zip exists."
                    .to_string(),
            ),
        )
    };
    let meta = VersionMeta {
        no: ver,
        name: v.name.clone(),
        upload_id: v.upload_id,
        build_id: v.build_id,
        fingerprint: v.fingerprint.clone(),
        filename: v.filename.clone(),
        size: v.size,
        timestamp: v.timestamp,
        archived_at: 0,
        kind,
        serve: rel.clone(),
        upload,
        files: Vec::new(),
        note,
    };
    store.commit_version(
        &g.name,
        meta,
        &[
            (rel, file.to_path_buf()),
            (MANIFEST_FILE.to_string(), manifest_path),
        ],
    )
}

/// Import everything the old deployment in `cfg` knew. Safe to run again:
/// what is already in the archive is left alone. With a `client`, versions
/// that were listed but never downloaded are fetched, as long as itch.io
/// still serves exactly that upload.
pub async fn migrate(cfg: &MigrateConfig, client: Option<&ItchClient>) -> Result<MigrateReport> {
    let state_path = cfg.state_dir.join("state.json");
    if !state_path.is_file() {
        bail!(
            "{} does not exist: nothing to migrate",
            state_path.display()
        );
    }
    let state = State::load(&cfg.state_dir)?;
    std::fs::create_dir_all(&cfg.cache_dir)
        .with_context(|| format!("create {}", cfg.cache_dir.display()))?;
    let store = Store::new(&cfg.archive_dir, None);
    let existing = store.load().context("read the archive")?;

    // Refuse to mix two histories: numbers and names must agree.
    for p in &existing {
        let clash = state.games.iter().find(|(no, g)| {
            (**no == p.meta.no) != (g.game_id == p.meta.game_id)
                || (g.game_id == p.meta.game_id && g.name != p.meta.name)
        });
        if let Some((no, g)) = clash {
            bail!(
                "the archive already has pack {} (number {}), which conflicts with {} (number {no}) in {}",
                p.meta.name,
                p.meta.no,
                g.name,
                state_path.display()
            );
        }
    }

    let keys: HashMap<u64, u64> = match client {
        Some(c) => match c.owned_games().await {
            Ok(owned) => owned.into_iter().map(|g| (g.game_id, g.key_id)).collect(),
            Err(e) => {
                warn!("could not list owned packs, nothing will be fetched: {e:#}");
                HashMap::new()
            }
        },
        None => HashMap::new(),
    };
    let limits = Limits {
        max_unpacked_bytes: cfg.max_unpacked_bytes,
        max_depth: MAX_ARCHIVE_DEPTH,
    };
    let scratch = cfg.cache_dir.join("scan-tmp");

    let mut report = MigrateReport::default();
    for (&no, g) in &state.games {
        if no == 0 || no > MAX_PACKS || !is_safe_name(&g.name) {
            bail!(
                "{}: pack {no} ({:?}) is not valid",
                state_path.display(),
                g.name
            );
        }
        // Every pack keeps its number, even one with nothing to import yet.
        store.write_pack(&PackMeta {
            no,
            game_id: g.game_id,
            name: g.name.clone(),
            title: g.title.clone(),
            url: String::new(),
        })?;
        report.packs += 1;
        let committed = existing
            .iter()
            .find(|p| p.meta.no == no)
            .map(|p| p.versions.as_slice())
            .unwrap_or_default();
        let mut listed: Option<Vec<Upload>> = None;

        for (i, v) in g.versions.iter().enumerate() {
            let label = format!("{}/{}", g.name, v.name);
            let Some(ver) = u16::try_from(i + 1).ok() else {
                bail!("{label}: too many versions");
            };
            if !is_safe_name(&v.name) {
                bail!("{label}: not a valid version name");
            }
            if let Some(have) = committed.iter().find(|c| c.no == ver) {
                if have.name != v.name {
                    bail!(
                        "{label}: the archive already has version {ver} of this pack as {}",
                        have.name
                    );
                }
                report.already += 1;
                continue;
            }
            let local = [true, false].into_iter().find_map(|zip| {
                let ext = if zip { "zip" } else { "raw" };
                let path = cfg
                    .old_archive_dir
                    .join(format!("{}-{ver}.{ext}", g.game_id));
                path.is_file().then_some((path, zip))
            });
            if let Some((file, zip)) = local {
                let done = tokio::task::block_in_place(|| {
                    import_local(&store, cfg, g, v, ver, &file, zip)
                })
                .with_context(|| format!("import {label}"))?;
                info!("imported {label}");
                debug_assert_eq!(done.no, ver);
                report.imported.push(label);
                continue;
            }

            // Listed but never downloaded. Fetch it if itch.io still serves
            // exactly this upload; otherwise its number stays unused.
            let key_id = keys.get(&g.game_id).copied();
            let fetched = match (client, key_id) {
                (Some(client), Some(key_id)) => {
                    if listed.is_none() {
                        listed = Some(match client.uploads(g.game_id, key_id).await {
                            Ok(u) => u,
                            Err(e) => {
                                warn!("{}: could not list uploads: {e:#}", g.name);
                                Vec::new()
                            }
                        });
                    }
                    let current = listed.as_ref().unwrap().iter().any(|u| {
                        u.id == v.upload_id
                            && u.build_id == v.build_id
                            && scan::fingerprint(u) == v.fingerprint
                    });
                    if current {
                        let plan = VersionPlan {
                            no: ver,
                            name: v.name.clone(),
                            upload_id: v.upload_id,
                            build_id: v.build_id,
                            fingerprint: v.fingerprint.clone(),
                            filename: v.filename.clone(),
                            size: v.size,
                            timestamp: v.timestamp,
                        };
                        let result = async {
                            scan::prepare(client, &scratch, limits, g.game_id, key_id, &plan)
                                .await?
                                .commit(&store, &g.name)
                                .await
                        }
                        .await;
                        match result {
                            Ok(_) => true,
                            Err(e) => {
                                warn!("{label}: could not be fetched: {e:#}");
                                false
                            }
                        }
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if fetched {
                info!("fetched {label}");
                report.fetched.push(label);
            } else {
                warn!("{label}: was never downloaded and cannot be fetched; leaving a gap");
                report.gaps.push(label);
            }
        }
    }
    Ok(report)
}
