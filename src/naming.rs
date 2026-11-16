//! How packs and versions are named.
//!
//! Names and numbering are assigned once and never change, so that paths pinned
//! by a build stay valid (and NFS file handles survive a server restart).

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
