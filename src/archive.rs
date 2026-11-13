//! Reading asset pack archives from the on-disk cache.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// A zip archive, exposed as its decompressed contents.
    Zip,
    /// Anything else, exposed as the single downloaded file.
    Raw,
}

/// The file listing of one pack version. Persisted, so that directory listings
/// and de-duplication never need the archive again.
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

/// Build the manifest of a downloaded upload.
pub fn scan(path: &Path, filename: &str) -> Result<Manifest> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    match ZipArchive::new(file) {
        Ok(mut zip) => {
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
            Ok(Manifest {
                kind: Kind::Zip,
                entries,
            })
        }
        Err(_) => {
            let mut file = File::open(path)?;
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
            let name = filename.replace(['/', '\\'], "_");
            let name = if name.is_empty() || name == "." || name == ".." {
                "upload.bin".to_string()
            } else {
                name
            };
            Ok(Manifest {
                kind: Kind::Raw,
                entries: vec![Entry {
                    path: name,
                    size,
                    crc: hasher.finalize(),
                    mtime: 0,
                    index: 0,
                }],
            })
        }
    }
}

fn normalise(p: &Path) -> Option<String> {
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
fn is_junk(path: &str) -> bool {
    path.split('/').any(|c| c == "__MACOSX" || c == ".DS_Store")
}

fn zip_time(t: zip::DateTime) -> Option<i64> {
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
    open: Mutex<HashMap<u64, Arc<Mutex<ZipArchive<File>>>>>,
}

impl Archives {
    fn handle(&self, key: u64, path: &Path) -> Result<Arc<Mutex<ZipArchive<File>>>> {
        if let Some(h) = self.open.lock().unwrap().get(&key) {
            return Ok(h.clone());
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
        open.insert(key, h.clone());
        Ok(h)
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
        out: &mut dyn std::io::Write,
    ) -> Result<()> {
        let h = self.handle(key, path)?;
        let mut zip = h.lock().unwrap();
        let mut f = zip
            .by_index(index)
            .with_context(|| format!("zip entry {index} of {}", path.display()))?;
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
