//! Reading asset pack archives: listing them, and extracting from them.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// An archive (flattened to a zip if need be), exposed as its contents.
    // The aliases are how manifests written before the archive directory
    // existed spell it; `migrate` reads those.
    #[serde(alias = "Zip")]
    Zip,
    /// Anything else, exposed as the single downloaded file.
    #[serde(alias = "Raw")]
    Raw,
}

/// The file listing of one pack version. Stored next to the version in the
/// archive directory, so that directory listings and de-duplication never
/// need to open the upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub kind: Kind,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// `/`-separated, relative, normalised.
    pub path: String,
    pub size: u64,
    pub crc: u32,
    /// Unix seconds, 0 if unknown.
    pub mtime: i64,
    /// Index into the zip's central directory.
    pub index: usize,
}

/// Build the manifest of an ingested upload: `zip` says whether the cached
/// file is a (possibly flattened) zip, or the upload served as it is.
pub fn scan(path: &Path, filename: &str, zip: bool) -> Result<Manifest> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    if zip {
        let mut zip =
            ZipArchive::new(file).with_context(|| format!("read zip {}", path.display()))?;
        let mut entries = Vec::with_capacity(zip.len());
        for index in 0..zip.len() {
            let f = zip.by_index_raw(index)?;
            if f.is_dir() || f.is_symlink() {
                continue;
            }
            let Some(path) = f.enclosed_name().and_then(|p| normalise(&p)) else {
                tracing::warn!("skipping zip entry with unsafe name {:?}", f.name());
                continue;
            };
            if is_junk(&path) {
                continue;
            }
            let mtime = f.last_modified().and_then(zip_time).unwrap_or(0);
            entries.push(Entry {
                path,
                size: f.size(),
                crc: f.crc32(),
                mtime,
                index,
            });
        }
        return Ok(Manifest {
            kind: Kind::Zip,
            entries,
        });
    }
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok(Manifest {
        kind: Kind::Raw,
        entries: vec![Entry {
            path: crate::store::safe_file_name(filename),
            size,
            crc: hasher.finalize(),
            mtime: 0,
            index: 0,
        }],
    })
}

pub(crate) fn normalise(p: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for c in p.components() {
        match c {
            std::path::Component::Normal(s) => parts.push(s.to_str()?),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// macOS Finder litter that is never a real asset.
pub(crate) fn is_junk(path: &str) -> bool {
    path.split('/').any(|c| c == "__MACOSX" || c == ".DS_Store")
}

pub(crate) fn zip_time(t: zip::DateTime) -> Option<i64> {
    if t == zip::DateTime::default() {
        return None; // 1980-01-01: the writer did not know
    }
    let d = chrono::NaiveDate::from_ymd_opt(t.year() as i32, t.month() as u32, t.day() as u32)?;
    Some(
        d.and_hms_opt(t.hour() as u32, t.minute() as u32, t.second() as u32)?
            .and_utc()
            .timestamp(),
    )
}

/// Read up to `count` bytes at `offset`.
pub fn read_range(path: &Path, offset: u64, count: u32) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.take(count as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

const MAX_OPEN: usize = 16;

/// A small pool of opened zip archives, so that reading many files out of one
/// pack does not re-parse its central directory every time.
#[derive(Default)]
pub struct Archives {
    open: Mutex<HashMap<u64, Open>>,
}

struct Open {
    path: PathBuf,
    zip: Arc<Mutex<ZipArchive<File>>>,
}

impl Archives {
    fn handle(&self, key: u64, path: &Path) -> Result<Arc<Mutex<ZipArchive<File>>>> {
        if let Some(o) = self.open.lock().unwrap().get(&key) {
            return Ok(o.zip.clone());
        }
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let h = Arc::new(Mutex::new(
            ZipArchive::new(file).with_context(|| format!("read zip {}", path.display()))?,
        ));
        let mut open = self.open.lock().unwrap();
        while open.len() >= MAX_OPEN {
            let victim = *open.keys().nth(rand::random_range(0..open.len())).unwrap();
            open.remove(&victim);
        }
        open.insert(
            key,
            Open {
                path: path.to_path_buf(),
                zip: h.clone(),
            },
        );
        Ok(h)
    }

    /// Forget every opened archive that was opened from `path`, because the
    /// file was deleted: an open handle would keep its space allocated.
    pub fn forget_path(&self, path: &Path) {
        self.open.lock().unwrap().retain(|_, o| o.path != path);
    }

    /// Forget an opened archive (e.g. because its file was replaced).
    pub fn forget(&self, key: u64) {
        self.open.lock().unwrap().remove(&key);
    }

    /// Decompress one entry into `out`. The zip crate verifies the CRC.
    pub fn extract(
        &self,
        key: u64,
        path: &Path,
        index: usize,
        size: u64,
        crc: u32,
        out: &mut dyn std::io::Write,
    ) -> Result<()> {
        let h = self.handle(key, path)?;
        let mut zip = h.lock().unwrap();
        let mut f = zip
            .by_index(index)
            .with_context(|| format!("zip entry {index} of {}", path.display()))?;
        if f.crc32() != crc {
            bail!(
                "zip entry {index} of {} is not the file that was listed",
                path.display()
            );
        }
        let n = std::io::copy(&mut f, out)
            .with_context(|| format!("decompress entry {index} of {}", path.display()))?;
        if n != size {
            bail!(
                "zip entry {index} of {} is {n} bytes, expected {size}",
                path.display()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deleted_file_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let zip_at = |name: &str| {
            let path = dir.path().join(name);
            let mut w = zip::ZipWriter::new(File::create(&path).unwrap());
            w.start_file("a", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut w, b"hello").unwrap();
            w.finish().unwrap();
            path
        };
        let (a, b) = (zip_at("a.zip"), zip_at("b.zip"));
        let archives = Archives::default();
        // Two versions served from the same file, and one from another.
        for (key, path) in [(1, &a), (2, &a), (3, &b)] {
            let mut out = Vec::new();
            archives
                .extract(key, path, 0, 5, crc32fast::hash(b"hello"), &mut out)
                .unwrap();
            assert_eq!(out, b"hello");
        }
        archives.forget_path(&a);
        let open = archives.open.lock().unwrap();
        assert_eq!(open.keys().copied().collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn kind_is_written_lower_case_and_read_either_way() {
        let m = Manifest {
            kind: Kind::Zip,
            entries: Vec::new(),
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"kind":"zip","entries":[]}"#
        );
        // As written before the archive directory existed.
        let old: Manifest = serde_json::from_str(r#"{"kind":"Raw","entries":[]}"#).unwrap();
        assert_eq!(old.kind, Kind::Raw);
        let old: Manifest = serde_json::from_str(r#"{"kind":"Zip","entries":[]}"#).unwrap();
        assert_eq!(old.kind, Kind::Zip);
    }
}
