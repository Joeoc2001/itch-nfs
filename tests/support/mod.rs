//! Shared by the integration tests and `examples/mock_itch.rs`: archive
//! fixtures in every supported format, and a mock of the itch.io API.
#![allow(dead_code)]

use std::collections::BTreeMap;
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
    for (name, data) in files {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
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

pub struct MockUpload {
    pub filename: String,
    pub updated_at: String,
    pub data: Vec<u8>,
}

impl MockUpload {
    pub fn new(filename: &str, date: &str, data: Vec<u8>) -> Self {
        Self {
            filename: filename.into(),
            updated_at: format!("{date}T10:00:00.000000000Z"),
            data,
        }
    }
}

/// One account owning one pack ("Tiny Pack" by "creator").
#[derive(Default)]
pub struct Mock {
    pub uploads: Mutex<BTreeMap<u64, MockUpload>>,
    pub downloads: AtomicUsize,
}

type Params = Query<BTreeMap<String, String>>;
type Json = (StatusCode, axum::Json<serde_json::Value>);

fn authorised(headers: &HeaderMap, params: &BTreeMap<String, String>) -> bool {
    headers.get("authorization").is_some_and(|v| v == TOKEN)
        && params
            .get("download_key_id")
            .is_none_or(|k| *k == KEY.to_string())
}

async fn owned_keys(headers: HeaderMap, Query(params): Params) -> Json {
    if !authorised(&headers, &params) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"errors": ["invalid key"]})),
        );
    }
    // Like the real API, an empty list is serialised as `{}`.
    let keys = match params.get("page").map(String::as_str) {
        Some("1") => {
            json!([{"id": KEY, "game_id": GAME, "game": {"id": GAME, "title": "Tiny Pack", "url": "https://creator.itch.io/tiny-pack"}}])
        }
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
    if !authorised(&headers, &params) || game != GAME || !params.contains_key("download_key_id") {
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
        .map(|(id, u)| {
            let md5 = format!("md5-{}-{}", u.data.len(), crc32fast::hash(&u.data));
            json!({"id": id, "filename": u.filename, "size": u.data.len(), "md5_hash": md5, "storage": "hosted", "created_at": u.updated_at, "updated_at": u.updated_at})
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
    if !authorised(&headers, &params) || !params.contains_key("download_key_id") {
        return Err(StatusCode::FORBIDDEN);
    }
    mock.downloads.fetch_add(1, Ordering::SeqCst);
    mock.uploads
        .lock()
        .unwrap()
        .get(&id)
        .map(|u| u.data.clone())
        .ok_or(StatusCode::NOT_FOUND)
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
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = http.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(http, router(mock)).await });
    addr
}

/// A pack history that exercises every format, nesting and de-duplication.
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
    let v3 = rar_of(&[("docs/readme.txt", b"read me\n"), ("docs/a.png", b"aaaa")]);
    let v4 = tar_bz2_of(&[
        ("b.png", b"BBBB"),
        ("fonts.tar.zst", &tar_zst_of(&[("f.ttf", b"font")])),
        ("plain.tar", &tar_of(&[("p.txt", b"plain\n")])),
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
