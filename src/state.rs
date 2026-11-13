//! Persistent state: which packs and versions exist, and what they are called.
//!
//! Names and numbering are assigned once and never change, so that paths pinned
//! by a build stay valid (and NFS file handles survive a server restart).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const MAX_VERSIONS: usize = 0xFFFF;
pub const MAX_GAMES: u32 = 0xF_FFFF;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Keyed by our own stable game number (not the itch id), starting at 1.
    #[serde(default)]
    pub games: BTreeMap<u32, GameState>,
    /// When the list of packs last changed (unix seconds); the root's mtime.
    #[serde(default)]
    pub changed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub game_id: u64,
    /// Directory name under the export root.
    pub name: String,
    pub title: String,
    /// Download key; `None` once the pack is no longer in the library.
    pub key_id: Option<u64>,
    /// In discovery order; a version's number is its index + 1.
    #[serde(default)]
    pub versions: Vec<VersionState>,
    /// When the list of versions last changed (unix seconds); the directory's
    /// mtime, which is what makes NFS clients re-read it.
    #[serde(default)]
    pub changed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionState {
    /// Directory name under the pack directory.
    pub name: String,
    pub upload_id: u64,
    pub build_id: Option<u64>,
    /// Distinguishes re-uploads that keep the same upload id.
    pub fingerprint: String,
    pub filename: String,
    pub size: Option<u64>,
    /// Unix seconds.
    pub timestamp: i64,
}

impl State {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path(dir);
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        write_atomic(&Self::path(dir), &serde_json::to_vec_pretty(self)?)
    }

    fn path(dir: &Path) -> PathBuf {
        dir.join("state.json")
    }

    pub fn game_no(&self, game_id: u64) -> Option<u32> {
        self.games
            .iter()
            .find(|(_, g)| g.game_id == game_id)
            .map(|(no, _)| *no)
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Reduce arbitrary text to a filesystem- and shell-friendly name.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches(|c| c == '-' || c == '.').to_string()
}

/// `https://creator.itch.io/pack-name` -> `creator.pack-name`.
pub fn game_dir_name(url: &str, game_id: u64) -> String {
    let rest = url.split("://").nth(1).unwrap_or("");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let creator = slug(host.strip_suffix(".itch.io").unwrap_or(""));
    let pack = slug(path.split(['/', '?', '#']).next().unwrap_or(""));
    if creator.is_empty() || pack.is_empty() {
        format!("game-{game_id}")
    } else {
        format!("{creator}.{pack}")
    }
}

/// Parse itch's RFC 3339 timestamps into unix seconds.
pub fn parse_ts(s: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|d| d.timestamp())
}

pub fn version_dir_name(ts: i64, filename: &str, upload_id: u64, build_id: Option<u64>) -> String {
    let date = chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    // Drop the extension, including compound ones like `.tar.gz`.
    let stem = match crate::ingest::archive_suffix(filename) {
        Some(suffix) => &filename[..filename.len() - suffix.len()],
        None => filename.rsplit_once('.').map_or(filename, |(s, _)| s),
    };
    let stem = if stem.is_empty() { filename } else { stem };
    let mut name = format!("{date}-{}-u{upload_id}", slug(stem));
    if let Some(b) = build_id {
        name.push_str(&format!("-b{b}"));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            game_dir_name("https://kenney.itch.io/tiny-town", 5),
            "kenney.tiny-town"
        );
        assert_eq!(game_dir_name("", 5), "game-5");
        let ts = parse_ts(Some("2024-01-05T10:15:00.000000000Z")).unwrap();
        assert_eq!(
            version_dir_name(ts, "Tiny Town (v1.2).zip", 77, None),
            "2024-01-05-tiny-town-v1.2-u77"
        );
        assert_eq!(
            version_dir_name(ts, "a.zip", 77, Some(9)),
            "2024-01-05-a-u77-b9"
        );
    }
}
