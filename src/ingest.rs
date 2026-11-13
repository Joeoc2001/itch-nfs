//! Turning a downloaded upload into something we can serve.
//!
//! Whatever the upload is (zip, 7z, rar, tar.*), and however many archives are
//! nested inside it, the result is one flat zip on disk. Zip is the only format
//! here with cheap random access, which is what lets individual assets be
//! decompressed on demand later. A nested archive becomes a directory named
//! after the archive file.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use tracing::warn;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::archive::{is_junk, normalise};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Cap on the total decompressed size of one upload (archive bombs).
    pub max_unpacked_bytes: u64,
    /// How many archives deep to look.
    pub max_depth: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The upload is a plain zip with nothing nested: serve it as it is.
    ZipAsIs,
    /// The upload was flattened into a zip at the requested output path.
    Flattened,
    /// Not an archive we can open: serve the upload as a single file.
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Zip,
    SevenZ,
    Rar,
    Tar(Compression),
}

const ARCHIVE_SUFFIXES: &[&str] = &[
    ".zip", ".7z", ".rar", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tbz", ".tar.xz",
    ".txz", ".tar.zst", ".tzst",
];

/// Only files named like archives are opened. Plenty of asset formats are
/// archives in disguise (.unitypackage, .jar, .love, .docx) and must stay whole.
pub fn is_archive_name(name: &str) -> bool {
    archive_suffix(name).is_some()
}

/// The (longest) archive extension `name` ends with, if any.
pub fn archive_suffix(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    ARCHIVE_SUFFIXES
        .iter()
        .filter(|s| name.ends_with(*s))
        .max_by_key(|s| s.len())
        .copied()
}

/// Identify the format from the file's magic bytes.
fn detect(path: &Path, name: &str) -> Option<Format> {
    if !is_archive_name(name) {
        return None;
    }
    let mut head = [0u8; 512];
    let n = read_up_to(&mut File::open(path).ok()?, &mut head).ok()?;
    let head = &head[..n];
    let compression = if head.starts_with(&[0x1f, 0x8b]) {
        Compression::Gzip
    } else if head.starts_with(b"BZh") {
        Compression::Bzip2
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Compression::Xz
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Compression::Zstd
    } else {
        Compression::None
    };
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Some(Format::Zip)
    } else if head.starts_with(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]) {
        Some(Format::SevenZ)
    } else if head.starts_with(b"Rar!\x1a\x07") {
        Some(Format::Rar)
    } else if compression != Compression::None || head.get(257..262) == Some(b"ustar") {
        Some(Format::Tar(compression))
    } else {
        None
    }
}

fn read_up_to(r: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

struct Ctx<'a> {
    limits: Limits,
    /// Bytes of decompressed output still allowed.
    budget: u64,
    tmp: &'a Path,
    /// Set when the failure is ours (disk full, budget) rather than the
    /// archive's, so that it is reported instead of papered over.
    fatal: bool,
}

/// The zip being written.
struct Out {
    zip: ZipWriter<BufWriter<File>>,
    names: HashSet<String>,
}

impl Out {
    fn create(path: &Path) -> Result<Out> {
        let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
        Ok(Out {
            zip: ZipWriter::new(BufWriter::new(file)),
            names: HashSet::new(),
        })
    }

    /// Compress a stream into the zip as `path`. Duplicate paths are dropped.
    fn add_stream(
        &mut self,
        ctx: &mut Ctx,
        path: &str,
        mtime: Option<i64>,
        r: &mut dyn Read,
    ) -> Result<()> {
        if !self.names.insert(path.to_string()) {
            return Ok(());
        }
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Zstd)
            .compression_level(Some(3))
            .large_file(true)
            .last_modified_time(mtime.and_then(zip_time).unwrap_or_default());
        if let Err(e) = self.zip.start_file(path, options) {
            ctx.fatal = true;
            return Err(e.into());
        }
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = r.read(&mut buf).with_context(|| format!("read {path}"))?;
            if n == 0 {
                return Ok(());
            }
            ctx.spend(n as u64)?;
            if let Err(e) = self.zip.write_all(&buf[..n]) {
                ctx.fatal = true;
                return Err(e.into());
            }
        }
    }

    fn finish(self) -> Result<()> {
        self.zip
            .finish()?
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        Ok(())
    }
}

impl Ctx<'_> {
    fn spend(&mut self, n: u64) -> Result<()> {
        if n > self.budget {
            self.fatal = true;
            bail!("upload decompresses to more than the configured limit");
        }
        self.budget -= n;
        Ok(())
    }

    fn temp_file(&mut self) -> Result<tempfile::NamedTempFile> {
        tempfile::NamedTempFile::new_in(self.tmp).map_err(|e| {
            self.fatal = true;
            anyhow!(e).context("create temporary file")
        })
    }
}

fn zip_time(secs: i64) -> Option<zip::DateTime> {
    use chrono::{Datelike, Timelike};
    let t = chrono::DateTime::from_timestamp(secs, 0)?;
    zip::DateTime::from_date_and_time(
        t.year().try_into().ok()?,
        t.month() as u8,
        t.day() as u8,
        t.hour() as u8,
        t.minute() as u8,
        t.second() as u8,
    )
    .ok()
}

/// Prepare a downloaded upload for serving. May write `out_path`; see [`Outcome`].
pub fn ingest(
    src: &Path,
    filename: &str,
    out_path: &Path,
    tmp: &Path,
    limits: Limits,
) -> Result<Outcome> {
    let Some(format) = detect(src, filename) else {
        return Ok(Outcome::Raw);
    };
    if format == Format::Zip {
        match ZipArchive::new(BufReader::new(File::open(src)?)) {
            Ok(zip)
                if !zip
                    .file_names()
                    .any(|n| n.is_ok_and(|n| is_archive_name(&n))) =>
            {
                return Ok(Outcome::ZipAsIs);
            }
            Ok(_) => {}
            Err(e) => {
                warn!("{filename}: not a readable zip, serving it as a plain file: {e}");
                return Ok(Outcome::Raw);
            }
        }
    }
    let mut ctx = Ctx {
        limits,
        budget: limits.max_unpacked_bytes,
        tmp,
        fatal: false,
    };
    let mut out = Out::create(out_path)?;
    let result = expand(&mut ctx, src, format, "", &mut out, 0).and_then(|_| out.finish());
    match result {
        Ok(()) => Ok(Outcome::Flattened),
        Err(e) => {
            let _ = std::fs::remove_file(out_path);
            if ctx.fatal {
                return Err(e.context(format!("unpack {filename}")));
            }
            warn!("{filename}: cannot be unpacked, serving it as a plain file: {e:#}");
            Ok(Outcome::Raw)
        }
    }
}

/// Write one archive member to `out`, descending into it if it is an archive.
fn add_entry(
    ctx: &mut Ctx,
    out: &mut Out,
    path: &str,
    mtime: Option<i64>,
    r: &mut dyn Read,
    depth: u32,
) -> Result<()> {
    if is_junk(path) {
        return Ok(());
    }
    if !is_archive_name(path) || depth >= ctx.limits.max_depth {
        return out.add_stream(ctx, path, mtime, r);
    }
    // Nested archive: spool it to disk, and try to unpack it into a zip of its
    // own so that a failure half-way leaves nothing behind.
    let mut spool = ctx.temp_file()?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = r.read(&mut buf).with_context(|| format!("read {path}"))?;
        if n == 0 {
            break;
        }
        ctx.spend(n as u64)?;
        if let Err(e) = spool.write_all(&buf[..n]) {
            ctx.fatal = true;
            return Err(e.into());
        }
    }
    spool.flush()?;
    if let Some(format) = detect(spool.path(), path) {
        let nested = ctx.temp_file()?;
        let mut sub = Out::create(nested.path())?;
        match expand(
            ctx,
            spool.path(),
            format,
            &format!("{path}/"),
            &mut sub,
            depth + 1,
        )
        .and_then(|_| sub.finish())
        {
            Ok(()) => {
                let mut zip = ZipArchive::new(BufReader::new(File::open(nested.path())?))?;
                for i in 0..zip.len() {
                    let f = zip.by_index_raw(i)?;
                    if out.names.insert(f.name()?.into_owned())
                        && let Err(e) = out.zip.raw_copy_file(f)
                    {
                        ctx.fatal = true;
                        return Err(e.into());
                    }
                }
                return Ok(());
            }
            Err(e) if ctx.fatal => return Err(e),
            Err(e) => {
                warn!("{path}: nested archive cannot be unpacked, keeping it as a file: {e:#}")
            }
        }
    }
    spool.rewind()?;
    out.add_stream(ctx, path, mtime, &mut spool)
}

/// Unpack the archive at `src` into `out`, with every path prefixed by `prefix`.
fn expand(
    ctx: &mut Ctx,
    src: &Path,
    format: Format,
    prefix: &str,
    out: &mut Out,
    depth: u32,
) -> Result<()> {
    match format {
        Format::Zip => {
            let mut zip = ZipArchive::new(BufReader::new(File::open(src)?))?;
            for i in 0..zip.len() {
                let raw = zip.by_index_raw(i)?;
                if raw.is_dir() || raw.is_symlink() {
                    continue;
                }
                let Some(name) = raw.enclosed_name().and_then(|p| normalise(&p)) else {
                    continue;
                };
                let path = format!("{prefix}{name}");
                if is_archive_name(&path) {
                    let mtime = raw.last_modified().and_then(crate::archive::zip_time);
                    drop(raw);
                    add_entry(ctx, out, &path, mtime, &mut zip.by_index(i)?, depth)?;
                } else if !is_junk(&path) && out.names.insert(path.clone()) {
                    // Already compressed: copy the bytes across untouched.
                    ctx.spend(raw.size())?;
                    if let Err(e) = out.zip.raw_copy_file_rename(raw, &path) {
                        ctx.fatal = true;
                        return Err(e.into());
                    }
                }
            }
        }
        Format::Tar(compression) => {
            let file = BufReader::new(File::open(src)?);
            let reader: Box<dyn Read> = match compression {
                Compression::None => Box::new(file),
                Compression::Gzip => Box::new(flate2::bufread::MultiGzDecoder::new(file)),
                Compression::Bzip2 => Box::new(bzip2::bufread::MultiBzDecoder::new(file)),
                Compression::Xz => Box::new(lzma_rust2::XzReader::new(file, true)),
                Compression::Zstd => Box::new(zstd::stream::read::Decoder::with_buffer(file)?),
            };
            let mut tar = tar::Archive::new(reader);
            let mut any = false;
            for entry in tar.entries()? {
                let mut entry = entry?;
                any = true;
                if !entry.header().entry_type().is_file() {
                    continue;
                }
                let Some(name) = normalise(&entry.path()?) else {
                    continue;
                };
                let mtime = entry.header().mtime().ok().map(|t| t as i64);
                add_entry(
                    ctx,
                    out,
                    &format!("{prefix}{name}"),
                    mtime,
                    &mut entry,
                    depth,
                )?;
            }
            if !any {
                bail!("not a tar archive");
            }
        }
        Format::SevenZ => {
            let mut reader =
                sevenz_rust2::ArchiveReader::open(src, sevenz_rust2::Password::empty())?;
            // The callback cannot carry our error type out, so park it here.
            let mut failure = None;
            let walked = reader.for_each_entries(|entry, r| {
                if entry.is_directory() {
                    return Ok(true);
                }
                let Some(name) = normalise(Path::new(&entry.name().replace('\\', "/"))) else {
                    // The stream still has to be consumed to reach the next entry.
                    std::io::copy(r, &mut std::io::sink())?;
                    return Ok(true);
                };
                let mtime = std::time::SystemTime::from(entry.last_modified_date())
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs() as i64);
                match add_entry(ctx, out, &format!("{prefix}{name}"), mtime, r, depth) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        failure = Some(e);
                        Ok(false)
                    }
                }
            });
            if let Some(e) = failure {
                return Err(e);
            }
            walked?;
        }
        Format::Rar => {
            // unrar can only extract to a path, so each member goes via a temp file.
            let mut archive = unrar::Archive::new(src).open_for_processing()?;
            while let Some(header) = archive.read_header()? {
                let entry = header.entry();
                let name = normalise(&entry.filename);
                archive = match name {
                    Some(name) if entry.is_file() => {
                        ctx.spend(0)?;
                        if entry.unpacked_size > ctx.budget {
                            ctx.fatal = true;
                            bail!("upload decompresses to more than the configured limit");
                        }
                        let tmp = ctx.temp_file()?;
                        let next = header.extract_to(tmp.path())?;
                        add_entry(
                            ctx,
                            out,
                            &format!("{prefix}{name}"),
                            None,
                            &mut File::open(tmp.path())?,
                            depth,
                        )?;
                        next
                    }
                    _ => header.skip()?,
                };
            }
        }
    }
    Ok(())
}
