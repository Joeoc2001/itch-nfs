//! End-to-end: a mock itch.io API, the scanner filling an archive, the
//! frontend serving it, and a real NFSv3 client.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use itch_nfs::fs::{Config, ItchFs};
use itch_nfs::tree::DEDUP_FILE;
use nfs3_client::Nfs3ConnectionBuilder;
use nfs3_client::nfs3_types::nfs3::{self, Nfs3Result, filename3, nfsstat3};
use nfs3_client::nfs3_types::xdr_codec::Opaque;
use nfs3_client::tokio::{TokioConnector, TokioIo};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use support::{Mock, MockGame, MockUpload, scan_config, scanner, snapshot, zip_of};

type Conn = nfs3_client::Nfs3Connection<TokioIo<tokio::net::TcpStream>>;

fn config(archive_dir: &Path, cache_dir: &Path) -> Config {
    Config {
        archive_dir: archive_dir.to_path_buf(),
        cache_dir: cache_dir.to_path_buf(),
        cache_max_bytes: 64 * 1024,
        cache_ttl: Duration::from_secs(60),
        rescan_interval: Duration::from_secs(3600),
        disk_cache_max_bytes: 1 << 30,
    }
}

/// Start a frontend. It is given the archive and a cache directory, and
/// nothing else: no token, no API address.
async fn serve_with(cfg: Config) -> (ItchFs, Conn) {
    let fs = ItchFs::new(cfg).unwrap();
    let listener = NFSTcpListener::bind("127.0.0.1:0", fs.clone())
        .await
        .unwrap();
    let port = listener.get_listen_port();
    tokio::spawn(async move { listener.handle_forever().await });
    let conn = Nfs3ConnectionBuilder::new(TokioConnector, "127.0.0.1", "/")
        .portmapper_port(port)
        .connect_from_privileged_port(false)
        .mount()
        .await
        .unwrap();
    (fs, conn)
}

async fn serve(archive_dir: &Path, cache_dir: &Path) -> (ItchFs, Conn) {
    serve_with(config(archive_dir, cache_dir)).await
}

async fn mtime(c: &mut Conn, path: &str) -> u32 {
    let fh = walk(c, path).await.unwrap();
    c.getattr(&nfs3::GETATTR3args { object: fh })
        .await
        .unwrap()
        .unwrap()
        .obj_attributes
        .mtime
        .seconds
}

async fn lookup(c: &mut Conn, dir: &nfs3::nfs_fh3, name: &str) -> Result<nfs3::nfs_fh3, nfsstat3> {
    let args = nfs3::LOOKUP3args {
        what: nfs3::diropargs3 {
            dir: dir.clone(),
            name: filename3(Opaque::borrowed(name.as_bytes())),
        },
    };
    match c.lookup(&args).await.unwrap() {
        Nfs3Result::Ok(r) => Ok(r.object),
        Nfs3Result::Err((code, _)) => Err(code),
    }
}

async fn walk(c: &mut Conn, path: &str) -> Result<nfs3::nfs_fh3, nfsstat3> {
    let mut fh = c.root_nfs_fh3();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        fh = lookup(c, &fh, part).await?;
    }
    Ok(fh)
}

async fn ls(c: &mut Conn, path: &str) -> Vec<String> {
    let dir = walk(c, path).await.unwrap();
    let mut names = Vec::new();
    let (mut cookie, mut cookieverf) = (nfs3::cookie3::default(), nfs3::cookieverf3::default());
    loop {
        // Small replies, to exercise pagination.
        let r = c
            .readdirplus(&nfs3::READDIRPLUS3args {
                dir: dir.clone(),
                cookie,
                cookieverf,
                dircount: 400,
                maxcount: 800,
            })
            .await
            .unwrap()
            .unwrap();
        cookieverf = r.cookieverf;
        for e in r.reply.entries.into_inner() {
            cookie = e.cookie;
            let name = String::from_utf8(e.name.0.to_vec()).unwrap();
            if name != "." && name != ".." {
                names.push(name);
            }
        }
        if r.reply.eof {
            break;
        }
    }
    names.sort();
    names
}

async fn read_fh(c: &mut Conn, fh: &nfs3::nfs_fh3) -> Result<Vec<u8>, nfsstat3> {
    let mut out = Vec::new();
    loop {
        match c
            .read(&nfs3::READ3args {
                file: fh.clone(),
                offset: out.len() as u64,
                count: 50_000,
            })
            .await
            .unwrap()
        {
            Nfs3Result::Ok(r) => {
                out.extend_from_slice(&r.data.0);
                if r.eof {
                    return Ok(out);
                }
            }
            Nfs3Result::Err((code, _)) => return Err(code),
        }
    }
}

async fn cat(c: &mut Conn, path: &str) -> Vec<u8> {
    let fh = walk(c, path)
        .await
        .unwrap_or_else(|e| panic!("{path}: {e:?}"));
    read_fh(c, &fh).await.unwrap()
}

/// Bigger than the 64 KiB memory cache, and incompressible enough to matter.
fn big() -> Vec<u8> {
    (0..300_000u32)
        .flat_map(|i| i.wrapping_mul(2654435761).to_le_bytes())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn versions_are_pinned_and_deduplicated() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let big = big();

    let mock = Arc::new(Mock::default());
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new(
            "Tiny Pack v1.zip",
            "2024-01-05",
            zip_of(&[
                ("tiny/a.png", b"aaaa"),
                ("tiny/b.png", b"bbbb"),
                ("tiny/big.bin", &big),
                ("__MACOSX/tiny/._a.png", b"junk"),
            ]),
        ),
    );
    let (mock_addr, mock_server) = support::spawn_stoppable(mock.clone()).await;

    let archive_dir = tempfile::tempdir().unwrap();
    let scan_cache = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let scan = scanner(
        mock_addr,
        scan_config(archive_dir.path(), scan_cache.path()),
    );

    // Before the scanner has ever run, the export is empty rather than broken.
    let (fs, mut c) = serve(archive_dir.path(), cache_dir.path()).await;
    assert!(ls(&mut c, "/").await.is_empty());
    let root_before = mtime(&mut c, "/").await;

    // The scanner archives the pack; the frontend finds it on its next rescan.
    let report = scan.pass().await.unwrap();
    assert_eq!(report.failed, Vec::<String>::new());
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 1);
    let requests = mock.requests.load(Ordering::SeqCst);
    fs.rescan().await.unwrap();
    assert!(
        mtime(&mut c, "/").await > root_before,
        "a new pack changes the root's mtime, so clients re-read it"
    );

    let v1 = "creator.tiny-pack/2024-01-05-tiny-pack-v1-u1";
    assert_eq!(ls(&mut c, "/").await, ["creator.tiny-pack"]);
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        ["2024-01-05-tiny-pack-v1-u1"]
    );
    assert_eq!(ls(&mut c, v1).await, [DEDUP_FILE, "tiny"]);
    assert_eq!(
        ls(&mut c, &format!("{v1}/tiny")).await,
        ["a.png", "b.png", "big.bin"]
    );
    assert_eq!(
        fs.archive_reads(),
        0,
        "listing needs no zip from the archive"
    );
    assert_eq!(cat(&mut c, &format!("{v1}/tiny/a.png")).await, b"aaaa");
    assert_eq!(cat(&mut c, &format!("{v1}/tiny/big.bin")).await, big);
    assert_eq!(
        fs.archive_reads(),
        1,
        "the version's zip is read from the archive once, however many files are read"
    );
    assert_eq!(
        walk(&mut c, "creator.tiny-pack/latest").await.unwrap_err(),
        nfsstat3::NFS3ERR_NOENT
    );

    // The export is read-only.
    let dir = walk(&mut c, v1).await.unwrap();
    let create = nfs3::CREATE3args {
        where_: nfs3::diropargs3 {
            dir,
            name: filename3(Opaque::borrowed(b"x")),
        },
        how: nfs3::createhow3::UNCHECKED(nfs3::sattr3::default()),
    };
    assert!(matches!(
        c.create(&create).await.unwrap(),
        Nfs3Result::Err((nfsstat3::NFS3ERR_ROFS, _))
    ));

    // The creator publishes v2 as a second upload: a unchanged, b changed, c new.
    mock.uploads.lock().unwrap().insert(
        2,
        MockUpload::new(
            "Tiny Pack v2.zip",
            "2024-06-01",
            zip_of(&[
                ("tiny-v2/a.png", b"aaaa"),
                ("tiny-v2/b.png", b"BBBB"),
                ("tiny-v2/extra/c.png", b"cccc"),
                ("tiny-v2/big.bin", &big),
            ]),
        ),
    );
    let pack_before = mtime(&mut c, "creator.tiny-pack").await;
    scan.pass().await.unwrap();
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        ["2024-01-05-tiny-pack-v1-u1"],
        "not visible until the frontend rescans"
    );
    fs.rescan().await.unwrap();
    assert!(
        mtime(&mut c, "creator.tiny-pack").await > pack_before,
        "a new version changes the pack directory's mtime"
    );
    let v2 = "creator.tiny-pack/2024-06-01-tiny-pack-v2-u2";
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        ["2024-01-05-tiny-pack-v1-u1", "2024-06-01-tiny-pack-v2-u2"]
    );
    assert_eq!(ls(&mut c, v2).await, [DEDUP_FILE, "tiny-v2"]);
    assert_eq!(
        ls(&mut c, &format!("{v2}/tiny-v2")).await,
        ["b.png", "extra"]
    );
    assert_eq!(cat(&mut c, &format!("{v2}/tiny-v2/b.png")).await, b"BBBB");
    assert_eq!(
        cat(&mut c, &format!("{v2}/tiny-v2/extra/c.png")).await,
        b"cccc"
    );
    assert_eq!(
        walk(&mut c, &format!("{v2}/tiny-v2/a.png"))
            .await
            .unwrap_err(),
        nfsstat3::NFS3ERR_NOENT
    );
    let listing = String::from_utf8(cat(&mut c, &format!("{v2}/{DEDUP_FILE}")).await).unwrap();
    assert!(
        listing.contains("tiny-v2/a.png\t2024-01-05-tiny-pack-v1-u1/tiny/a.png\n"),
        "{listing}"
    );
    assert!(
        listing.contains("tiny-v2/big.bin\t2024-01-05-tiny-pack-v1-u1/tiny/big.bin\n"),
        "{listing}"
    );
    // v1 is untouched.
    assert_eq!(
        ls(&mut c, &format!("{v1}/tiny")).await,
        ["a.png", "b.png", "big.bin"]
    );
    assert_eq!(cat(&mut c, &format!("{v1}/tiny/b.png")).await, b"bbbb");
    assert_eq!(fs.archive_reads(), 2, "one read of the archive per version");

    // The creator replaces upload 1 in place and deletes upload 2. That makes
    // a third version; the first two stay exactly as they were.
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new(
            "Tiny Pack v1.zip",
            "2024-09-01",
            zip_of(&[("tiny/a.png", b"aaaa"), ("tiny/d.png", b"dddd")]),
        ),
    );
    mock.uploads.lock().unwrap().remove(&2);
    scan.pass().await.unwrap();
    fs.rescan().await.unwrap();
    let v3 = "creator.tiny-pack/2024-09-01-tiny-pack-v1-u1";
    assert_eq!(ls(&mut c, "creator.tiny-pack").await.len(), 3);
    assert_eq!(ls(&mut c, &format!("{v3}/tiny")).await, ["d.png"]);
    assert_eq!(cat(&mut c, &format!("{v3}/tiny/d.png")).await, b"dddd");
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 3);

    // Handles a client holds, to compare across what follows.
    let handles = [
        format!("{v1}/tiny/b.png"),
        format!("{v1}/tiny"),
        v1.to_string(),
        format!("{v2}/tiny-v2/extra/c.png"),
        v2.to_string(),
        "creator.tiny-pack".to_string(),
    ];
    let mut held = Vec::new();
    for path in &handles {
        held.push(walk(&mut c, path).await.unwrap());
    }
    let b1 = held[0].clone();

    // Now itch.io goes away entirely, and so does every local directory: the
    // frontend's cache and the scanner's. Only the archive is left. The
    // frontend never made a request of its own in the first place.
    assert_eq!(
        mock.requests.load(Ordering::SeqCst) - requests,
        // Per pass: two pages of owned keys and one listing of uploads; per
        // download: the download and the listing that confirms it.
        2 * 3 + 2 * 2,
        "every request to itch.io is the scanner's"
    );
    drop(fs);
    mock_server.abort();
    let _ = mock_server.await;
    assert!(
        tokio::net::TcpStream::connect(mock_addr).await.is_err(),
        "the mock API is down"
    );
    let untouched = snapshot(archive_dir.path());
    drop(cache_dir);
    drop(scan_cache);
    let cache_dir = tempfile::tempdir().unwrap();
    let (fs, mut c) = serve(archive_dir.path(), cache_dir.path()).await;

    // Same handles, same paths, same contents: including the version whose
    // upload was replaced and the one whose upload was deleted.
    for (path, old) in handles.iter().zip(&held) {
        let new = walk(&mut c, path).await.unwrap();
        assert_eq!(new.data.0, old.data.0, "the handle of {path} changed");
    }
    assert_eq!(read_fh(&mut c, &b1).await.unwrap(), b"bbbb");
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        [
            "2024-01-05-tiny-pack-v1-u1",
            "2024-06-01-tiny-pack-v2-u2",
            "2024-09-01-tiny-pack-v1-u1"
        ]
    );
    assert_eq!(
        ls(&mut c, &format!("{v1}/tiny")).await,
        ["a.png", "b.png", "big.bin"]
    );
    assert_eq!(cat(&mut c, &format!("{v1}/tiny/a.png")).await, b"aaaa");
    assert_eq!(cat(&mut c, &format!("{v1}/tiny/big.bin")).await, big);
    assert_eq!(ls(&mut c, v2).await, [DEDUP_FILE, "tiny-v2"]);
    assert_eq!(cat(&mut c, &format!("{v2}/tiny-v2/b.png")).await, b"BBBB");
    assert_eq!(cat(&mut c, &format!("{v3}/tiny/d.png")).await, b"dddd");
    assert_eq!(fs.archive_reads(), 3);

    // A second read of a version does not touch the archive directory. To
    // prove it, take the archive away: a file of a version read above, that
    // has itself not been read since the restart (so is in no cache of
    // decompressed assets), still reads, from the local copy of the zip.
    let hidden = archive_dir.path().with_extension("hidden");
    std::fs::rename(archive_dir.path(), &hidden).unwrap();
    assert_eq!(
        cat(&mut c, &format!("{v2}/tiny-v2/extra/c.png")).await,
        b"cccc"
    );
    assert_eq!(ls(&mut c, &format!("{v3}/tiny")).await, ["d.png"]);
    assert_eq!(fs.archive_reads(), 3);
    std::fs::rename(&hidden, archive_dir.path()).unwrap();

    // The frontend only ever read the archive.
    assert_eq!(snapshot(archive_dir.path()), untouched);
}

/// The frontend keeps to its disk budget by dropping the local copies of the
/// least recently used zips, and fetches them again when they are needed.
#[tokio::test(flavor = "multi_thread")]
async fn the_disk_cache_is_bounded() {
    let mock = Arc::new(Mock::default());
    let big = big();
    {
        let mut u = mock.uploads.lock().unwrap();
        for id in 1..=3u64 {
            // Each upload differs, so nothing is de-duplicated away.
            let mut data = big.clone();
            data[0] = id as u8;
            let name = format!("f{id}.bin");
            u.insert(
                id,
                MockUpload::new(
                    &format!("V{id}.zip"),
                    &format!("2024-0{id}-01"),
                    zip_of(&[(&name, &data), ("small.txt", id.to_string().as_bytes())]),
                ),
            );
        }
    }
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    scanner(addr, scan_config(archive.path(), cache.path()))
        .pass()
        .await
        .unwrap();

    // Room for two of the three zips (each a little over 1.2 MB).
    let mut cfg = config(archive.path(), cache.path());
    cfg.disk_cache_max_bytes = 3_000_000;
    let (fs, mut c) = serve_with(cfg).await;
    let small = |id: u64| format!("creator.tiny-pack/2024-0{id}-01-v{id}-u{id}/small.txt");
    let zips = || snapshot(&cache.path().join("zips"));
    assert_eq!(cat(&mut c, &small(1)).await, b"1");
    assert_eq!(cat(&mut c, &small(2)).await, b"2");
    assert_eq!((fs.archive_reads(), zips().len()), (2, 2));
    assert_eq!(cat(&mut c, &small(3)).await, b"3");
    assert_eq!(
        (fs.archive_reads(), zips().len()),
        (3, 2),
        "one was evicted"
    );
    assert!(zips().values().map(|s| s.size).sum::<u64>() <= 3_000_000);
    // Version 1 was the least recently used: reading more of it fetches its
    // zip again, and the contents are right.
    let mut expected = big.clone();
    expected[0] = 1;
    assert_eq!(
        cat(&mut c, "creator.tiny-pack/2024-01-01-v1-u1/f1.bin").await,
        expected
    );
    assert_eq!((fs.archive_reads(), zips().len()), (4, 2));
    // Version 3 is still local.
    let mut expected = big.clone();
    expected[0] = 3;
    assert_eq!(
        cat(&mut c, "creator.tiny-pack/2024-03-01-v3-u3/f3.bin").await,
        expected
    );
    assert_eq!(fs.archive_reads(), 4);
}

/// A zip that does not match the checksum recorded when it was archived is
/// refused, rather than served.
#[tokio::test(flavor = "multi_thread")]
async fn a_corrupted_archive_fails_loudly() {
    let mock = Arc::new(Mock::default());
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new("Tiny.zip", "2024-01-05", zip_of(&[("a.png", b"aaaa")])),
    );
    mock.uploads.lock().unwrap().insert(
        2,
        MockUpload::new("Doc.pdf", "2024-02-05", b"%PDF not really".to_vec()),
    );
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    scanner(addr, scan_config(archive.path(), cache.path()))
        .pass()
        .await
        .unwrap();
    // Bit rot in the archive: same sizes, different bytes.
    let pack = archive.path().join("packs/creator.tiny-pack");
    let zip = pack.join("2024-01-05-tiny-u1/upload/Tiny.zip");
    let mut bytes = std::fs::read(&zip).unwrap();
    let at = bytes.len() - 3;
    bytes[at] ^= 0xff;
    std::fs::write(&zip, &bytes).unwrap();
    std::fs::write(
        pack.join("2024-02-05-doc-u2/upload/Doc.pdf"),
        b"%PDF not REALLY",
    )
    .unwrap();

    let (_fs, mut c) = serve(archive.path(), cache.path()).await;
    let v1 = "creator.tiny-pack/2024-01-05-tiny-u1";
    assert_eq!(ls(&mut c, v1).await, [DEDUP_FILE, "a.png"]);
    let fh = walk(&mut c, &format!("{v1}/a.png")).await.unwrap();
    assert_eq!(
        read_fh(&mut c, &fh).await.unwrap_err(),
        nfsstat3::NFS3ERR_IO
    );
    let fh = walk(&mut c, "creator.tiny-pack/2024-02-05-doc-u2/Doc.pdf")
        .await
        .unwrap();
    assert_eq!(
        read_fh(&mut c, &fh).await.unwrap_err(),
        nfsstat3::NFS3ERR_IO
    );
    assert!(snapshot(&cache.path().join("zips")).is_empty());
}

/// Packs appear once they have a committed version, each under its own
/// stable number, and a version being written is not served.
#[tokio::test(flavor = "multi_thread")]
async fn only_committed_versions_are_served() {
    let mock = Arc::new(Mock::default());
    mock.games.lock().unwrap().push(MockGame::new(
        600,
        "Empty Pack",
        "https://someone.itch.io/empty-pack",
        None,
    ));
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new("Tiny.zip", "2024-01-05", zip_of(&[("a.png", b"aaaa")])),
    );
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    scanner(addr, scan_config(archive.path(), cache.path()))
        .pass()
        .await
        .unwrap();
    // A version the scanner is half way through writing.
    let half = archive
        .path()
        .join("packs/creator.tiny-pack/2024-02-05-next-u2");
    std::fs::create_dir_all(half.join("upload")).unwrap();
    std::fs::write(half.join("upload/Next.zip"), b"PK\x03\x04 half").unwrap();
    std::fs::write(half.join("manifest.json"), b"{\"kind\":\"zip\",\"entr").unwrap();

    let (_fs, mut c) = serve(archive.path(), cache.path()).await;
    // The pack with no uploads has no directory; the half-written version is
    // not listed and cannot be looked up.
    assert_eq!(ls(&mut c, "/").await, ["creator.tiny-pack"]);
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        ["2024-01-05-tiny-u1"]
    );
    assert_eq!(
        walk(&mut c, "creator.tiny-pack/2024-02-05-next-u2")
            .await
            .unwrap_err(),
        nfsstat3::NFS3ERR_NOENT
    );
    assert_eq!(
        cat(&mut c, "creator.tiny-pack/2024-01-05-tiny-u1/a.png").await,
        b"aaaa"
    );
}

/// Every regular file under `path`, as paths relative to the export root.
async fn tree(c: &mut Conn, path: &str, out: &mut Vec<String>) {
    for name in ls(c, path).await {
        let child = format!("{path}/{name}");
        let fh = walk(c, &child).await.unwrap();
        let attr = c
            .getattr(&nfs3::GETATTR3args { object: fh })
            .await
            .unwrap()
            .unwrap()
            .obj_attributes;
        if matches!(attr.type_, nfs3::ftype3::NF3DIR) {
            Box::pin(tree(c, &child, out)).await;
        } else {
            out.push(child);
        }
    }
}

/// The fixture pack covers every archive format, nesting and de-duplication.
/// CI mounts the same pack with the kernel client and checks the same listing.
/// `ci/mock-expected.txt` predates the archive directory, so matching it shows
/// that the exported paths did not move.
#[tokio::test(flavor = "multi_thread")]
async fn every_format_is_unpacked_recursively() {
    let mock = Arc::new(Mock::default());
    *mock.uploads.lock().unwrap() = support::fixture_uploads();
    let mock_addr = support::spawn(mock.clone()).await;
    let archive_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    scanner(mock_addr, scan_config(archive_dir.path(), cache_dir.path()))
        .pass()
        .await
        .unwrap();
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 5);
    let (fs, mut c) = serve(archive_dir.path(), cache_dir.path()).await;

    let mut files = Vec::new();
    tree(&mut c, "creator.tiny-pack", &mut files).await;
    let listing = files.join("\n") + "\n";
    let expected =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ci/mock-expected.txt"))
            .unwrap_or_default();
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/ci/mock-expected.txt"),
            &listing,
        )
        .unwrap();
    } else {
        assert_eq!(listing, expected, "run with UPDATE_EXPECTED=1 to accept");
    }

    let v1 = "creator.tiny-pack/2024-01-05-tiny-pack-v1-u1/tiny";
    assert_eq!(
        cat(
            &mut c,
            &format!("{v1}/extras.tar.gz/more/deeper.zip/deep.txt")
        )
        .await,
        b"deep\n"
    );
    let v2 = "creator.tiny-pack/2024-02-05-tiny-pack-v2-u2/tiny-v2";
    assert_eq!(
        cat(&mut c, &format!("{v2}/sounds.tar.xz/s.ogg")).await,
        b"ogg1"
    );
    assert_eq!(
        cat(
            &mut c,
            "creator.tiny-pack/2024-03-05-tiny-docs-u3/docs/readme.txt"
        )
        .await,
        b"read me\n"
    );
    assert_eq!(
        cat(
            &mut c,
            "creator.tiny-pack/2024-04-05-tiny-extras-u4/fonts.tar.zst/f.ttf"
        )
        .await,
        b"font"
    );
    // An upload that is not an archive is served as the file itself.
    assert_eq!(
        cat(
            &mut c,
            "creator.tiny-pack/2024-05-05-tiny-manual-u5/Tiny Manual.pdf"
        )
        .await,
        b"%PDF-1.4 not really"
    );
    assert_eq!(fs.archive_reads(), 5);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 5);
}
