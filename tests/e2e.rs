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

/// The handle a file id makes: 20 bits pack, 16 bits version, 28 bits node.
fn handle(pack: u32, version: u16, node: u32) -> nfs3::nfs_fh3 {
    let id = (pack as u64) << 44 | (version as u64) << 28 | node as u64;
    nfs3::nfs_fh3 {
        data: Opaque::owned(id.to_be_bytes().to_vec()),
    }
}

async fn getattr(c: &mut Conn, fh: &nfs3::nfs_fh3) -> Result<nfs3::fattr3, nfsstat3> {
    match c
        .getattr(&nfs3::GETATTR3args { object: fh.clone() })
        .await
        .unwrap()
    {
        Nfs3Result::Ok(r) => Ok(r.obj_attributes),
        Nfs3Result::Err((code, _)) => Err(code),
    }
}

/// The rows of a pack's `.itch-nfs-deduplicated.tsv`, without the comments.
async fn dedup_rows(c: &mut Conn, pack: &str) -> Vec<String> {
    String::from_utf8(cat(c, &format!("{pack}/{DEDUP_FILE}")).await)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(str::to_string)
        .collect()
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
async fn versions_are_merged_and_deduplicated() {
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

    // The pack directory holds the contents themselves: there is no
    // directory per version.
    let pack = "creator.tiny-pack";
    assert_eq!(ls(&mut c, "/").await, [pack]);
    assert_eq!(ls(&mut c, pack).await, [DEDUP_FILE, "tiny"]);
    assert_eq!(
        ls(&mut c, &format!("{pack}/tiny")).await,
        ["a.png", "b.png", "big.bin"]
    );
    assert_eq!(dedup_rows(&mut c, pack).await, Vec::<String>::new());
    assert_eq!(
        fs.archive_reads(),
        0,
        "listing needs no zip from the archive"
    );
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/a.png")).await, b"aaaa");
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/big.bin")).await, big);
    assert_eq!(
        fs.archive_reads(),
        1,
        "the version's zip is read from the archive once, however many files are read"
    );
    for gone in ["latest", "2024-01-05-tiny-pack-v1-u1"] {
        assert_eq!(
            walk(&mut c, &format!("{pack}/{gone}")).await.unwrap_err(),
            nfsstat3::NFS3ERR_NOENT
        );
    }

    // The export is read-only.
    let dir = walk(&mut c, pack).await.unwrap();
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

    // What a client has of version 1, to compare once more versions exist.
    let v1_paths = [
        pack.to_string(),
        format!("{pack}/{DEDUP_FILE}"),
        format!("{pack}/tiny"),
        format!("{pack}/tiny/a.png"),
        format!("{pack}/tiny/b.png"),
        format!("{pack}/tiny/big.bin"),
    ];
    let mut v1_handles = Vec::new();
    for path in &v1_paths {
        v1_handles.push(walk(&mut c, path).await.unwrap());
    }
    // File handles are the ids they always were: pack 1, version 1, and the
    // node's number in the version's own listing (tiny is 1, a.png 2, ...).
    // Only the pack's listing is new, in a range that was never used.
    let expected = [
        handle(1, 0, 0),
        handle(1, 0, 1),
        handle(1, 1, 1),
        handle(1, 1, 2),
        handle(1, 1, 3),
        handle(1, 1, 4),
    ];
    for ((path, got), want) in v1_paths.iter().zip(&v1_handles).zip(&expected) {
        assert_eq!(got.data.0, want.data.0, "the handle of {path}");
    }
    // A handle of a version directory, from when the export had them, is
    // stale; so is the rest of the range the listing's id is from.
    for old in [handle(1, 1, 0), handle(1, 0, 2), handle(1, 2, 0)] {
        assert_eq!(
            getattr(&mut c, &old).await.unwrap_err(),
            nfsstat3::NFS3ERR_STALE
        );
    }
    // `..` leads back up, from the top level to the pack directory.
    let tiny = walk(&mut c, &format!("{pack}/tiny")).await.unwrap();
    let a = walk(&mut c, &format!("{pack}/tiny/a.png")).await.unwrap();
    assert_eq!(
        lookup(&mut c, &tiny, "..").await.unwrap().data.0,
        handle(1, 0, 0).data.0
    );
    assert_eq!(
        lookup(&mut c, &handle(1, 0, 0), "..").await.unwrap().data.0,
        c.root_nfs_fh3().data.0
    );
    assert_eq!(
        lookup(&mut c, &a, "x").await.unwrap_err(),
        nfsstat3::NFS3ERR_NOTDIR
    );

    // The creator publishes v2 as a second upload, re-zipped under another
    // folder: a unchanged, b changed, c new. It also carries a patch for the
    // old folder: another b, and a new e.
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
                ("tiny/b.png", b"B2B2"),
                ("tiny/e.png", b"eeee"),
                ("tiny/a.png", b"aaaa"),
            ]),
        ),
    );
    let pack_before = mtime(&mut c, pack).await;
    let tiny_before = mtime(&mut c, &format!("{pack}/tiny")).await;
    let listing_before = mtime(&mut c, &format!("{pack}/{DEDUP_FILE}")).await;
    scan.pass().await.unwrap();
    assert_eq!(
        ls(&mut c, pack).await,
        [DEDUP_FILE, "tiny"],
        "not visible until the frontend rescans"
    );
    assert_eq!(
        ls(&mut c, &format!("{pack}/tiny")).await,
        ["a.png", "b.png", "big.bin"]
    );
    fs.rescan().await.unwrap();
    assert!(
        mtime(&mut c, pack).await > pack_before,
        "a new version changes the pack directory's mtime"
    );
    assert!(
        mtime(&mut c, &format!("{pack}/tiny")).await > tiny_before,
        "and the mtime of a directory it adds to, so clients re-read that too"
    );
    assert!(
        mtime(&mut c, &format!("{pack}/{DEDUP_FILE}")).await > listing_before,
        "and the listing's"
    );
    assert_eq!(ls(&mut c, pack).await, [DEDUP_FILE, "tiny", "tiny-v2"]);
    // What changed at a path that was taken is served next to the original,
    // tagged with the version's number; what is identical is not repeated.
    assert_eq!(
        ls(&mut c, &format!("{pack}/tiny")).await,
        ["a.png", "b.png", "b.v2.png", "big.bin", "e.png"]
    );
    assert_eq!(
        ls(&mut c, &format!("{pack}/tiny-v2")).await,
        ["b.png", "extra"]
    );
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/b.png")).await, b"bbbb");
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/b.v2.png")).await, b"B2B2");
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/e.png")).await, b"eeee");
    assert_eq!(cat(&mut c, &format!("{pack}/tiny-v2/b.png")).await, b"BBBB");
    assert_eq!(
        cat(&mut c, &format!("{pack}/tiny-v2/extra/c.png")).await,
        b"cccc"
    );
    assert_eq!(
        walk(&mut c, &format!("{pack}/tiny-v2/a.png"))
            .await
            .unwrap_err(),
        nfsstat3::NFS3ERR_NOENT
    );
    let v2 = "2024-06-01-tiny-pack-v2-u2";
    assert_eq!(
        dedup_rows(&mut c, pack).await,
        [
            format!("{v2}\ttiny/b.png\ttiny/b.v2.png\trenamed"),
            format!("{v2}\ttiny-v2/a.png\ttiny/a.png\tduplicate"),
            format!("{v2}\ttiny-v2/big.bin\ttiny/big.bin\tduplicate"),
            format!("{v2}\ttiny/a.png\ttiny/a.png\tduplicate"),
        ]
    );
    // Nothing a client had moved: same paths, same handles, same contents.
    for (path, old) in v1_paths.iter().zip(&v1_handles) {
        let new = walk(&mut c, path).await.unwrap();
        assert_eq!(new.data.0, old.data.0, "the handle of {path} changed");
    }
    assert_eq!(read_fh(&mut c, &v1_handles[4]).await.unwrap(), b"bbbb");
    assert_eq!(fs.archive_reads(), 2, "one read of the archive per version");

    // The creator replaces upload 1 in place and deletes upload 2. That makes
    // a third version; what the first two brought stays exactly as it was.
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new(
            "Tiny Pack v1.zip",
            "2024-09-01",
            zip_of(&[
                ("tiny/a.png", b"aaaa"),
                ("tiny/d.png", b"dddd"),
                ("tiny/b.png", b"bbbb"),
            ]),
        ),
    );
    mock.uploads.lock().unwrap().remove(&2);
    scan.pass().await.unwrap();
    fs.rescan().await.unwrap();
    let tiny_now = ["a.png", "b.png", "b.v2.png", "big.bin", "d.png", "e.png"];
    // b went back to what it was in version 1: nothing new to serve.
    assert_eq!(ls(&mut c, &format!("{pack}/tiny")).await, tiny_now);
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/d.png")).await, b"dddd");
    assert_eq!(
        dedup_rows(&mut c, pack).await[4..],
        [
            "2024-09-01-tiny-pack-v1-u1\ttiny/a.png\ttiny/a.png\tduplicate",
            "2024-09-01-tiny-pack-v1-u1\ttiny/b.png\ttiny/b.png\tduplicate",
        ]
    );
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 3);

    // Handles a client holds, to compare across what follows.
    let handles = [
        format!("{pack}/tiny/b.png"),
        format!("{pack}/tiny/b.v2.png"),
        format!("{pack}/tiny/d.png"),
        format!("{pack}/tiny"),
        format!("{pack}/tiny-v2/extra/c.png"),
        format!("{pack}/tiny-v2/extra"),
        format!("{pack}/tiny-v2"),
        format!("{pack}/{DEDUP_FILE}"),
        pack.to_string(),
    ];
    let mut held = Vec::new();
    for path in &handles {
        held.push(walk(&mut c, path).await.unwrap());
    }
    let (b1, b2) = (held[0].clone(), held[1].clone());
    let listing = cat(&mut c, &format!("{pack}/{DEDUP_FILE}")).await;

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

    // Same handles, same paths, same contents: including what came from the
    // version whose upload was replaced and the one whose upload was deleted.
    // The handles are used before anything is looked up by name, so the
    // server builds the pack's tree from a handle alone.
    assert_eq!(read_fh(&mut c, &b1).await.unwrap(), b"bbbb");
    assert_eq!(read_fh(&mut c, &b2).await.unwrap(), b"B2B2");
    for (path, old) in handles.iter().zip(&held) {
        let new = walk(&mut c, path).await.unwrap();
        assert_eq!(new.data.0, old.data.0, "the handle of {path} changed");
    }
    assert_eq!(ls(&mut c, pack).await, [DEDUP_FILE, "tiny", "tiny-v2"]);
    assert_eq!(ls(&mut c, &format!("{pack}/tiny")).await, tiny_now);
    assert_eq!(cat(&mut c, &format!("{pack}/{DEDUP_FILE}")).await, listing);
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/a.png")).await, b"aaaa");
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/big.bin")).await, big);
    assert_eq!(
        ls(&mut c, &format!("{pack}/tiny-v2")).await,
        ["b.png", "extra"]
    );
    assert_eq!(cat(&mut c, &format!("{pack}/tiny-v2/b.png")).await, b"BBBB");
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/d.png")).await, b"dddd");
    assert_eq!(fs.archive_reads(), 3);

    // A second read of a version does not touch the archive directory. To
    // prove it, take the archive away: a file of a version read above, that
    // has itself not been read since the restart (so is in no cache of
    // decompressed assets), still reads, from the local copy of the zip.
    let hidden = archive_dir.path().with_extension("hidden");
    std::fs::rename(archive_dir.path(), &hidden).unwrap();
    assert_eq!(
        cat(&mut c, &format!("{pack}/tiny-v2/extra/c.png")).await,
        b"cccc"
    );
    assert_eq!(cat(&mut c, &format!("{pack}/tiny/e.png")).await, b"eeee");
    assert_eq!(ls(&mut c, &format!("{pack}/tiny")).await, tiny_now);
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
    // Each version has its own small.txt, so the later ones are tagged; each
    // is read from its own version's zip.
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        [
            DEDUP_FILE,
            "f1.bin",
            "f2.bin",
            "f3.bin",
            "small.txt",
            "small.v2.txt",
            "small.v3.txt"
        ]
    );
    let small = |id: u64| match id {
        1 => "creator.tiny-pack/small.txt".to_string(),
        id => format!("creator.tiny-pack/small.v{id}.txt"),
    };
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
    assert_eq!(cat(&mut c, "creator.tiny-pack/f1.bin").await, expected);
    assert_eq!((fs.archive_reads(), zips().len()), (4, 2));
    // Version 3 is still local.
    let mut expected = big.clone();
    expected[0] = 3;
    assert_eq!(cat(&mut c, "creator.tiny-pack/f3.bin").await, expected);
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
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        [DEDUP_FILE, "Doc.pdf", "a.png"]
    );
    let fh = walk(&mut c, "creator.tiny-pack/a.png").await.unwrap();
    assert_eq!(
        read_fh(&mut c, &fh).await.unwrap_err(),
        nfsstat3::NFS3ERR_IO
    );
    let fh = walk(&mut c, "creator.tiny-pack/Doc.pdf").await.unwrap();
    assert_eq!(
        read_fh(&mut c, &fh).await.unwrap_err(),
        nfsstat3::NFS3ERR_IO
    );
    assert!(snapshot(&cache.path().join("zips")).is_empty());
}

/// A pack is every one of its versions, merged: if the listing of one cannot
/// be read, the pack is not served with that version missing. It fails, and
/// works again once the listing can be read.
#[tokio::test(flavor = "multi_thread")]
async fn a_pack_with_an_unreadable_listing_fails_until_it_can_be_read() {
    let mock = Arc::new(Mock::default());
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new("One.zip", "2024-01-05", zip_of(&[("a.png", b"aaaa")])),
    );
    mock.uploads.lock().unwrap().insert(
        2,
        MockUpload::new("Two.zip", "2024-02-05", zip_of(&[("a.png", b"AAAA")])),
    );
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    scanner(addr, scan_config(archive.path(), cache.path()))
        .pass()
        .await
        .unwrap();
    let manifest = archive
        .path()
        .join("packs/creator.tiny-pack/2024-02-05-two-u2/manifest.json");
    let good = std::fs::read(&manifest).unwrap();
    std::fs::write(&manifest, b"{}").unwrap();

    // A cache directory of its own: the scanner's has a copy of the listing.
    let serve_cache = tempfile::tempdir().unwrap();
    let (_fs, mut c) = serve(archive.path(), serve_cache.path()).await;
    assert_eq!(ls(&mut c, "/").await, ["creator.tiny-pack"]);
    let pack = walk(&mut c, "creator.tiny-pack").await.unwrap();
    assert!(getattr(&mut c, &pack).await.is_ok());
    // Not even version 1's file, which a client could otherwise mistake for
    // all there is.
    assert_eq!(
        lookup(&mut c, &pack, "a.png").await.unwrap_err(),
        nfsstat3::NFS3ERR_IO
    );
    assert_eq!(
        read_fh(&mut c, &handle(1, 1, 1)).await.unwrap_err(),
        nfsstat3::NFS3ERR_IO
    );

    std::fs::write(&manifest, &good).unwrap();
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        [DEDUP_FILE, "a.png", "a.v2.png"]
    );
    assert_eq!(read_fh(&mut c, &handle(1, 1, 1)).await.unwrap(), b"aaaa");
    assert_eq!(cat(&mut c, "creator.tiny-pack/a.v2.png").await, b"AAAA");
}

/// The export is unauthenticated and a handle is just a file id, so a client
/// can name any of the 2^20 pack numbers: none may cost the server memory.
#[tokio::test(flavor = "multi_thread")]
async fn a_handle_of_an_unknown_pack_is_stale_and_leaves_nothing_behind() {
    let mock = Arc::new(Mock::default());
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new("One.zip", "2024-01-05", zip_of(&[("a.png", b"aaaa")])),
    );
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    scanner(addr, scan_config(archive.path(), cache.path()))
        .pass()
        .await
        .unwrap();
    let (fs, mut c) = serve(archive.path(), cache.path()).await;
    assert_eq!(ls(&mut c, "/").await, ["creator.tiny-pack"]);
    assert_eq!(fs.packs_loaded(), 0);

    // Pack 1 exists; packs 2 and 0xfffff do not.
    for no in [2, 0xfffff] {
        let (dir, file) = (handle(no, 0, 0), handle(no, 1, 1));
        assert_eq!(
            lookup(&mut c, &dir, "a.png").await.unwrap_err(),
            nfsstat3::NFS3ERR_STALE
        );
        assert_eq!(
            lookup(&mut c, &file, "a.png").await.unwrap_err(),
            nfsstat3::NFS3ERR_STALE
        );
        assert_eq!(
            lookup(&mut c, &file, "..").await.unwrap_err(),
            nfsstat3::NFS3ERR_STALE
        );
        assert_eq!(
            read_fh(&mut c, &file).await.unwrap_err(),
            nfsstat3::NFS3ERR_STALE
        );
        assert_eq!(
            getattr(&mut c, &file).await.unwrap_err(),
            nfsstat3::NFS3ERR_STALE
        );
    }
    assert_eq!(fs.packs_loaded(), 0);

    // The pack that exists is loaded, once.
    assert_eq!(cat(&mut c, "creator.tiny-pack/a.png").await, b"aaaa");
    assert_eq!(fs.packs_loaded(), 1);
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
    std::fs::write(
        half.join("upload/Next.zip"),
        zip_of(&[("next.png", b"nnnn")]),
    )
    .unwrap();
    std::fs::write(
        half.join("manifest.json"),
        br#"{"kind":"zip","entries":[{"path":"next.png","size":4,"crc":0,"mtime":0,"index":0}]}"#,
    )
    .unwrap();

    let (_fs, mut c) = serve(archive.path(), cache.path()).await;
    // The pack with no uploads has no directory; nothing of the half-written
    // version is listed or can be looked up, although its listing is there.
    assert_eq!(ls(&mut c, "/").await, ["creator.tiny-pack"]);
    assert_eq!(ls(&mut c, "creator.tiny-pack").await, [DEDUP_FILE, "a.png"]);
    for absent in ["next.png", "2024-02-05-next-u2"] {
        assert_eq!(
            walk(&mut c, &format!("creator.tiny-pack/{absent}"))
                .await
                .unwrap_err(),
            nfsstat3::NFS3ERR_NOENT
        );
    }
    assert_eq!(cat(&mut c, "creator.tiny-pack/a.png").await, b"aaaa");
}

/// Should a version turn up with a lower number than the ones already
/// served, the pack's tree is rebuilt in number order: the result is what a
/// restart would serve, whatever was served before.
#[tokio::test(flavor = "multi_thread")]
async fn a_version_that_appears_out_of_order_is_merged_in_order() {
    let mock = Arc::new(Mock::default());
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new(
            "One.zip",
            "2024-01-05",
            zip_of(&[("tiny/b.png", b"bbbb"), ("tiny/a.png", b"aaaa")]),
        ),
    );
    mock.uploads.lock().unwrap().insert(
        2,
        MockUpload::new(
            "Two.zip",
            "2024-02-05",
            zip_of(&[
                ("tiny/a.png", b"aaaa"),
                ("tiny/b.png", b"BBBB"),
                ("tiny/c.png", b"cccc"),
            ]),
        ),
    );
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    scanner(addr, scan_config(archive.path(), cache.path()))
        .pass()
        .await
        .unwrap();
    // Version 1 is not there (yet) when the frontend starts.
    let one = archive
        .path()
        .join("packs/creator.tiny-pack/2024-01-05-one-u1");
    let aside = archive.path().join("aside");
    std::fs::rename(&one, &aside).unwrap();
    let serve_cache = tempfile::tempdir().unwrap();
    let (fs, mut c) = serve(archive.path(), serve_cache.path()).await;
    let tiny = "creator.tiny-pack/tiny";
    assert_eq!(ls(&mut c, tiny).await, ["a.png", "b.png", "c.png"]);
    assert_eq!(cat(&mut c, &format!("{tiny}/b.png")).await, b"BBBB");
    let c_png = walk(&mut c, &format!("{tiny}/c.png")).await.unwrap();

    std::fs::rename(&aside, &one).unwrap();
    fs.rescan().await.unwrap();
    let merged = ["a.png", "b.png", "b.v2.png", "c.png"];
    assert_eq!(ls(&mut c, tiny).await, merged);
    assert_eq!(cat(&mut c, &format!("{tiny}/b.png")).await, b"bbbb");
    assert_eq!(cat(&mut c, &format!("{tiny}/b.v2.png")).await, b"BBBB");
    assert_eq!(cat(&mut c, &format!("{tiny}/a.png")).await, b"aaaa");
    assert_eq!(
        dedup_rows(&mut c, "creator.tiny-pack").await,
        [
            "2024-02-05-two-u2\ttiny/b.png\ttiny/b.v2.png\trenamed",
            "2024-02-05-two-u2\ttiny/a.png\ttiny/a.png\tduplicate",
        ]
    );
    // A file keeps its handle through this, wherever it is served.
    assert_eq!(read_fh(&mut c, &c_png).await.unwrap(), b"cccc");
    let mut live = Vec::new();
    tree(&mut c, "creator.tiny-pack", &mut live).await;

    // A frontend started on the whole archive serves exactly the same.
    let fresh_cache = tempfile::tempdir().unwrap();
    let (_fresh_fs, mut fresh_c) = serve(archive.path(), fresh_cache.path()).await;
    let mut fresh = Vec::new();
    tree(&mut fresh_c, "creator.tiny-pack", &mut fresh).await;
    assert_eq!(live, fresh);
    for path in &fresh {
        assert_eq!(
            walk(&mut c, path).await.unwrap().data.0,
            walk(&mut fresh_c, path).await.unwrap().data.0,
            "{path}"
        );
        assert_eq!(cat(&mut c, path).await, cat(&mut fresh_c, path).await);
    }
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

/// The fixture pack covers every archive format, nesting, de-duplication and
/// clashing paths. CI mounts the same pack with the kernel client and checks
/// the same listing, `ci/mock-expected.txt`.
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

    // Every file, by its contents: the five uploads are one directory.
    let pack = "creator.tiny-pack";
    let unitypackage = support::tar_gz_of(&[("asset/pathname", b"Assets/a.png\n")]);
    let contents: [(&str, &[u8]); 16] = [
        // The zip. broken.zip could not be opened, so it is a file.
        ("tiny/a.png", b"aaaa"),
        ("tiny/b.png", b"bbbb"),
        ("tiny/broken.zip", b"PK\x03\x04 not really a zip"),
        ("tiny/extras.tar.gz/x.txt", b"x\n"),
        ("tiny/extras.tar.gz/more/deeper.zip/deep.txt", b"deep\n"),
        // The 7z, under another folder: only what version 1 does not have.
        ("tiny-v2/b.png", b"BBBB"),
        ("tiny-v2/sounds.tar.xz/s.ogg", b"ogg1"),
        // The rar: its b.png is a third one, at the first one's path.
        ("docs/readme.txt", b"read me\n"),
        ("tiny/b.v3.png", b"bbb3"),
        // The tar.bz2: an archive where version 1 has a file of that name,
        // and one that shares the directory of version 1's.
        ("fonts.tar.zst/f.ttf", b"font"),
        ("plain.tar/p.txt", b"plain\n"),
        ("tiny/broken.v4.zip/fixed.txt", b"fixed\n"),
        ("tiny/extras.tar.gz/x.v4.txt", b"x4\n"),
        ("tiny/extras.tar.gz/y.txt", b"y\n"),
        // An upload that is not an archive is served as the file itself.
        ("Tiny Manual.pdf", b"%PDF-1.4 not really"),
        // An archive in disguise stays whole.
        ("tiny/tool.unitypackage", &unitypackage),
    ];
    for (path, data) in &contents {
        assert_eq!(
            cat(&mut c, &format!("{pack}/{path}")).await,
            *data,
            "{path}"
        );
    }
    assert_eq!(files.len(), contents.len() + 1, "and the listing");
    assert_eq!(
        dedup_rows(&mut c, pack).await,
        [
            "2024-02-05-tiny-pack-v2-u2\ttiny-v2/a.png\ttiny/a.png\tduplicate",
            "2024-02-05-tiny-pack-v2-u2\ttiny-v2/extras.tar.gz/x.txt\ttiny/extras.tar.gz/x.txt\tduplicate",
            "2024-02-05-tiny-pack-v2-u2\ttiny-v2/extras.tar.gz/more/deeper.zip/deep.txt\ttiny/extras.tar.gz/more/deeper.zip/deep.txt\tduplicate",
            "2024-03-05-tiny-docs-u3\ttiny/b.png\ttiny/b.v3.png\trenamed",
            "2024-03-05-tiny-docs-u3\tdocs/a.png\ttiny/a.png\tduplicate",
            "2024-04-05-tiny-extras-u4\ttiny/broken.zip/fixed.txt\ttiny/broken.v4.zip/fixed.txt\trenamed",
            "2024-04-05-tiny-extras-u4\ttiny/extras.tar.gz/x.txt\ttiny/extras.tar.gz/x.v4.txt\trenamed",
            "2024-04-05-tiny-extras-u4\tb.png\ttiny-v2/b.png\tduplicate",
        ]
    );
    assert_eq!(fs.archive_reads(), 5);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 5);
}
