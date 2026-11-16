//! Shared by the integration tests and `examples/mock_itch.rs`: archive
//! fixtures in every supported format, and a mock of the itch.io API.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use serde_json::json;

pub type Files<'a> = &'a [(&'a str, &'a [u8])];

pub fn zip_of(files: Files) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    // A fixed timestamp, as in `tar_of`: by default an entry is stamped with
    // the current time, so two builds of a fixture could differ in content
    // and, once compressed again, in size.
    let options =
        zip::write::SimpleFileOptions::default().last_modified_time(zip::DateTime::default());
    for (name, data) in files {
        w.start_file(*name, options).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

pub fn tar_of(files: Files) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        h.set_cksum();
        b.append_data(&mut h, name, *data).unwrap();
    }
    b.into_inner().unwrap()
}

pub fn tar_gz_of(files: Files) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(&tar_of(files)).unwrap();
    e.finish().unwrap()
}

pub fn tar_bz2_of(files: Files) -> Vec<u8> {
    let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    e.write_all(&tar_of(files)).unwrap();
    e.finish().unwrap()
}

pub fn tar_xz_of(files: Files) -> Vec<u8> {
    let mut e =
        lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6)).unwrap();
    e.write_all(&tar_of(files)).unwrap();
    e.finish().unwrap()
}

pub fn tar_zst_of(files: Files) -> Vec<u8> {
    zstd::encode_all(&tar_of(files)[..], 3).unwrap()
}

pub fn sevenz_of(files: Files) -> Vec<u8> {
    let mut w = sevenz_rust2::ArchiveWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    for (name, data) in files {
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(name), Some(*data))
            .unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// A RAR5 archive with stored (uncompressed) members, written by hand because
/// nothing open source can create RAR files.
pub fn rar_of(files: Files) -> Vec<u8> {
    fn vint(mut n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }
    // A header block: CRC32, then size, then the body (which starts at the type).
    fn block(body: Vec<u8>) -> Vec<u8> {
        let mut sized = vint(body.len() as u64);
        sized.extend(body);
        let mut out = crc32fast::hash(&sized).to_le_bytes().to_vec();
        out.extend(sized);
        out
    }
    let mut out = b"Rar!\x1a\x07\x01\x00".to_vec();
    out.extend(block([vint(1), vint(0), vint(0)].concat())); // main header
    for (name, data) in files {
        let body = [
            vint(2),                 // file header
            vint(2),                 // header flags: data area follows
            vint(data.len() as u64), // packed size
            vint(4),                 // file flags: CRC32 present
            vint(data.len() as u64), // unpacked size
            vint(0o644),             // attributes
            crc32fast::hash(data).to_le_bytes().to_vec(),
            vint(0), // compression: stored
            vint(1), // host OS: unix
            vint(name.len() as u64),
            name.as_bytes().to_vec(),
        ]
        .concat();
        out.extend(block(body));
        out.extend_from_slice(data);
    }
    out.extend(block([vint(5), vint(0), vint(0)].concat())); // end of archive
    out
}

pub const TOKEN: &str = "test-token";
pub const GAME: u64 = 500;
pub const KEY: u64 = 77;

pub struct MockGame {
    pub id: u64,
    pub key: u64,
    pub title: String,
    pub url: String,
    /// `None`: the API does not say.
    pub classification: Option<String>,
}

impl MockGame {
    pub fn new(id: u64, title: &str, url: &str, classification: Option<&str>) -> Self {
        Self {
            id,
            key: id + 1000,
            title: title.into(),
            url: url.into(),
            classification: classification.map(str::to_string),
        }
    }
}

pub struct MockUpload {
    pub game: u64,
    pub filename: String,
    pub updated_at: String,
    pub data: Vec<u8>,
}

impl MockUpload {
    pub fn new(filename: &str, date: &str, data: Vec<u8>) -> Self {
        Self {
            game: GAME,
            filename: filename.into(),
            updated_at: format!("{date}T10:00:00.000000000Z"),
            data,
        }
    }

    pub fn of_game(mut self, game: u64) -> Self {
        self.game = game;
        self
    }
}

/// What the mock reports as an upload's `md5_hash` (not a real MD5).
pub fn mock_md5(data: &[u8]) -> String {
    format!("md5-{}-{}", data.len(), crc32fast::hash(data))
}

/// An account that, to begin with, owns one pack ("Tiny Pack" by "creator").
pub struct Mock {
    pub games: Mutex<Vec<MockGame>>,
    /// By upload id.
    pub uploads: Mutex<BTreeMap<u64, MockUpload>>,
    /// Downloads served (or attempted).
    pub downloads: AtomicUsize,
    /// Every request, of any kind.
    pub requests: AtomicUsize,
    /// Upload ids whose download fails with a server error.
    pub broken: Mutex<BTreeSet<u64>>,
    /// Replace an upload just as it is being downloaded: the download gets
    /// the old bytes, and every listing after it describes the new upload.
    pub swap_on_download: Mutex<BTreeMap<u64, MockUpload>>,
}

impl Default for Mock {
    fn default() -> Self {
        Self {
            games: Mutex::new(vec![MockGame {
                id: GAME,
                key: KEY,
                title: "Tiny Pack".into(),
                url: "https://creator.itch.io/tiny-pack".into(),
                classification: None,
            }]),
            uploads: Default::default(),
            downloads: Default::default(),
            requests: Default::default(),
            broken: Default::default(),
            swap_on_download: Default::default(),
        }
    }
}

type Params = Query<BTreeMap<String, String>>;
type Json = (StatusCode, axum::Json<serde_json::Value>);

/// The token must be right, and a download key (if given) must be `game`'s
/// (or, with no `game`, anyone's).
fn authorised(
    mock: &Mock,
    headers: &HeaderMap,
    params: &BTreeMap<String, String>,
    game: Option<u64>,
) -> bool {
    mock.requests.fetch_add(1, Ordering::SeqCst);
    headers.get("authorization").is_some_and(|v| v == TOKEN)
        && params.get("download_key_id").is_none_or(|k| {
            mock.games
                .lock()
                .unwrap()
                .iter()
                .any(|g| *k == g.key.to_string() && game.is_none_or(|id| id == g.id))
        })
}

async fn owned_keys(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Query(params): Params,
) -> Json {
    if !authorised(&mock, &headers, &params, None) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"errors": ["invalid key"]})),
        );
    }
    // Like the real API, an empty list is serialised as `{}`.
    let keys = match params.get("page").map(String::as_str) {
        Some("1") => mock
            .games
            .lock()
            .unwrap()
            .iter()
            .map(|g| {
                let mut game = json!({"id": g.id, "title": g.title, "url": g.url});
                if let Some(c) = &g.classification {
                    game["classification"] = json!(c);
                }
                json!({"id": g.key, "game_id": g.id, "game": game})
            })
            .collect(),
        _ => json!({}),
    };
    (
        StatusCode::OK,
        axum::Json(json!({"owned_keys": keys, "page": 1, "per_page": 50})),
    )
}

async fn uploads(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    UrlPath(game): UrlPath<u64>,
    Query(params): Params,
) -> Json {
    if !authorised(&mock, &headers, &params, Some(game)) || !params.contains_key("download_key_id")
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"errors": ["invalid game"]})),
        );
    }
    let list: Vec<_> = mock
        .uploads
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, u)| u.game == game)
        .map(|(id, u)| {
            json!({"id": id, "filename": u.filename, "size": u.data.len(), "md5_hash": mock_md5(&u.data), "storage": "hosted", "created_at": u.updated_at, "updated_at": u.updated_at})
        })
        .collect();
    (StatusCode::OK, axum::Json(json!({"uploads": list})))
}

async fn download(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<u64>,
    Query(params): Params,
) -> Result<Vec<u8>, StatusCode> {
    let game = mock.uploads.lock().unwrap().get(&id).map(|u| u.game);
    if !authorised(&mock, &headers, &params, game) || !params.contains_key("download_key_id") {
        return Err(StatusCode::FORBIDDEN);
    }
    mock.downloads.fetch_add(1, Ordering::SeqCst);
    if mock.broken.lock().unwrap().contains(&id) {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let mut uploads = mock.uploads.lock().unwrap();
    let data = uploads
        .get(&id)
        .map(|u| u.data.clone())
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(replacement) = mock.swap_on_download.lock().unwrap().remove(&id) {
        uploads.insert(id, replacement);
    }
    Ok(data)
}

pub fn router(mock: Arc<Mock>) -> Router {
    Router::new()
        .route("/profile/owned-keys", get(owned_keys))
        .route("/games/{id}/uploads", get(uploads))
        .route("/uploads/{id}/download", get(download))
        .with_state(mock)
}

/// Serve the mock on an ephemeral local port.
pub async fn spawn(mock: Arc<Mock>) -> std::net::SocketAddr {
    spawn_stoppable(mock).await.0
}

/// As [`spawn`], also returning the server task: abort it to take the API down.
pub async fn spawn_stoppable(
    mock: Arc<Mock>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = http.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(http, router(mock)).await;
    });
    (addr, task)
}

/// A pack history that exercises every format, nesting, de-duplication, and
/// versions that put other content at a path an earlier version has taken.
/// `ci/mock-expected.txt` is the tree the server must produce from it.
pub fn fixture_uploads() -> BTreeMap<u64, MockUpload> {
    let deep = zip_of(&[("deep.txt", b"deep\n")]);
    let extras = tar_gz_of(&[("x.txt", b"x\n"), ("more/deeper.zip", &deep)]);
    let unitypackage = tar_gz_of(&[("asset/pathname", b"Assets/a.png\n")]);
    let v1 = zip_of(&[
        ("tiny/a.png", b"aaaa"),
        ("tiny/b.png", b"bbbb"),
        ("tiny/extras.tar.gz", &extras),
        ("tiny/tool.unitypackage", &unitypackage),
        ("tiny/broken.zip", b"PK\x03\x04 not really a zip"),
    ]);
    let sounds = tar_xz_of(&[("s.ogg", b"ogg1")]);
    let v2 = sevenz_of(&[
        ("tiny-v2/a.png", b"aaaa"),
        ("tiny-v2/b.png", b"BBBB"),
        ("tiny-v2/extras.tar.gz", &extras),
        ("tiny-v2/sounds.tar.xz", &sounds),
    ]);
    let v3 = rar_of(&[
        ("docs/readme.txt", b"read me\n"),
        ("docs/a.png", b"aaaa"),
        // Version 1 has other content at this path.
        ("tiny/b.png", b"bbb3"),
    ]);
    let v4 = tar_bz2_of(&[
        ("b.png", b"BBBB"),
        ("fonts.tar.zst", &tar_zst_of(&[("f.ttf", b"font")])),
        ("plain.tar", &tar_of(&[("p.txt", b"plain\n")])),
        // In version 1 this could not be opened, so there it is a file.
        ("tiny/broken.zip", &zip_of(&[("fixed.txt", b"fixed\n")])),
        // Shares version 1's directory: x.txt changed, y.txt is new.
        (
            "tiny/extras.tar.gz",
            &tar_gz_of(&[("x.txt", b"x4\n"), ("y.txt", b"y\n")]),
        ),
    ]);
    BTreeMap::from([
        (1, MockUpload::new("Tiny Pack v1.zip", "2024-01-05", v1)),
        (2, MockUpload::new("Tiny Pack v2.7z", "2024-02-05", v2)),
        (3, MockUpload::new("Tiny Docs.rar", "2024-03-05", v3)),
        (4, MockUpload::new("Tiny Extras.tar.bz2", "2024-04-05", v4)),
        (
            5,
            MockUpload::new(
                "Tiny Manual.pdf",
                "2024-05-05",
                b"%PDF-1.4 not really".to_vec(),
            ),
        ),
    ])
}

/// A scanner configuration for tests: everything in scope, no pacing.
pub fn scan_config(
    archive: &std::path::Path,
    cache: &std::path::Path,
) -> itch_nfs::scan::ScanConfig {
    itch_nfs::scan::ScanConfig {
        archive_dir: archive.to_path_buf(),
        cache_dir: cache.to_path_buf(),
        max_unpacked_bytes: 1 << 30,
        only: Vec::new(),
        classifications: Vec::new(),
        concurrency: 2,
    }
}

pub fn client(mock_addr: std::net::SocketAddr) -> itch_nfs::itch::ItchClient {
    itch_nfs::itch::ItchClient::new(&format!("http://{mock_addr}"), TOKEN.into()).unwrap()
}

pub fn scanner(
    mock_addr: std::net::SocketAddr,
    cfg: itch_nfs::scan::ScanConfig,
) -> itch_nfs::scan::Scanner {
    itch_nfs::scan::Scanner::new(cfg, client(mock_addr)).unwrap()
}

/// What identifies a file on disk, and when it was last written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub inode: u64,
    pub size: u64,
    pub mtime: (i64, i64),
}

/// Every file under `dir`, by `/`-separated relative path.
pub fn snapshot(dir: &std::path::Path) -> BTreeMap<String, Stamp> {
    use std::os::unix::fs::MetadataExt;
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut BTreeMap<String, Stamp>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries {
            let path = e.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                walk(&path, root, out);
            } else {
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                    Stamp {
                        inode: meta.ino(),
                        size: meta.size(),
                        mtime: (meta.mtime(), meta.mtime_nsec()),
                    },
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}
