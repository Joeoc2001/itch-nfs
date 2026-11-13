//! End-to-end: a mock itch.io API, the real server, and a real NFSv3 client.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use itch_nfs::fs::{Config, ItchFs};
use itch_nfs::itch::ItchClient;
use itch_nfs::tree::DEDUP_FILE;
use nfs3_client::Nfs3ConnectionBuilder;
use nfs3_client::nfs3_types::nfs3::{self, Nfs3Result, filename3, nfsstat3};
use nfs3_client::nfs3_types::xdr_codec::Opaque;
use nfs3_client::tokio::{TokioConnector, TokioIo};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use support::{GAME, Mock, MockUpload, TOKEN, zip_of};

type Conn = nfs3_client::Nfs3Connection<TokioIo<tokio::net::TcpStream>>;

async fn serve(mock_addr: SocketAddr, state_dir: &Path, archive_dir: &Path) -> (ItchFs, Conn) {
    let cfg = Config {
        state_dir: state_dir.to_path_buf(),
        archive_dir: archive_dir.to_path_buf(),
        cache_max_bytes: 64 * 1024,
        cache_ttl: Duration::from_secs(60),
        refresh_interval: Duration::from_secs(3600),
        max_unpacked_bytes: 1 << 30,
    };
    let fs = ItchFs::new(
        cfg,
        ItchClient::new(&format!("http://{mock_addr}"), TOKEN.into()).unwrap(),
    )
    .unwrap();
    let listener = NFSTcpListener::bind("127.0.0.1:0", fs.clone())
        .await
        .unwrap();
    let port = listener.get_listen_port();
    tokio::spawn(async move { listener.handle_forever().await });
    let conn = Nfs3ConnectionBuilder::new(TokioConnector, "127.0.0.1", "/")
        .portmapper_port(port)
        .mount()
        .await
        .unwrap();
    (fs, conn)
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

#[tokio::test(flavor = "multi_thread")]
async fn versions_are_pinned_and_deduplicated() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    // Bigger than the 64 KiB memory cache, and incompressible enough to matter.
    let big: Vec<u8> = (0..300_000u32)
        .flat_map(|i| i.wrapping_mul(2654435761).to_le_bytes())
        .collect();

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
    let mock_addr = support::spawn(mock.clone()).await;

    let state_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let (fs, mut c) = serve(mock_addr, state_dir.path(), archive_dir.path()).await;

    let v1 = "creator.tiny-pack/2024-01-05-tiny-pack-v1-u1";
    assert_eq!(ls(&mut c, "/").await, ["creator.tiny-pack"]);
    assert_eq!(
        mock.downloads.load(Ordering::SeqCst),
        0,
        "browsing the library downloads nothing"
    );
    assert_eq!(
        ls(&mut c, "creator.tiny-pack").await,
        ["2024-01-05-tiny-pack-v1-u1"]
    );
    assert_eq!(ls(&mut c, v1).await, [DEDUP_FILE, "tiny"]);
    assert_eq!(
        ls(&mut c, &format!("{v1}/tiny")).await,
        ["a.png", "b.png", "big.bin"]
    );
    assert_eq!(cat(&mut c, &format!("{v1}/tiny/a.png")).await, b"aaaa");
    assert_eq!(cat(&mut c, &format!("{v1}/tiny/big.bin")).await, big);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 1);
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
    fs.refresh_all().await;
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

    // Restart the server: handles held by clients stay valid, nothing is re-downloaded.
    let b1 = walk(&mut c, &format!("{v1}/tiny/b.png")).await.unwrap();
    let downloads = mock.downloads.load(Ordering::SeqCst);
    assert_eq!(downloads, 2);
    drop(fs);
    let (fs, mut c) = serve(mock_addr, state_dir.path(), archive_dir.path()).await;
    assert_eq!(read_fh(&mut c, &b1).await.unwrap(), b"bbbb");
    assert_eq!(ls(&mut c, v2).await, [DEDUP_FILE, "tiny-v2"]);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), downloads);

    // The creator replaces upload 1 in place. That is a third version; v1 keeps
    // its listing, and keeps serving from the cached archive.
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new(
            "Tiny Pack v1.zip",
            "2024-09-01",
            zip_of(&[("tiny/a.png", b"aaaa"), ("tiny/d.png", b"dddd")]),
        ),
    );
    fs.refresh_all().await;
    let v3 = "creator.tiny-pack/2024-09-01-tiny-pack-v1-u1";
    assert_eq!(ls(&mut c, "creator.tiny-pack").await.len(), 3);
    assert_eq!(ls(&mut c, &format!("{v3}/tiny")).await, ["d.png"]);
    assert_eq!(cat(&mut c, &format!("{v3}/tiny/d.png")).await, b"dddd");
    assert_eq!(cat(&mut c, &format!("{v1}/tiny/a.png")).await, b"aaaa");

    // If the cached archive of a superseded version is lost, its files fail
    // loudly rather than being served from the replacement upload.
    drop(fs);
    std::fs::remove_file(archive_dir.path().join(format!("{GAME}-1.zip"))).unwrap();
    let (_fs, mut c) = serve(mock_addr, state_dir.path(), archive_dir.path()).await;
    assert_eq!(
        ls(&mut c, &format!("{v1}/tiny")).await,
        ["a.png", "b.png", "big.bin"]
    );
    assert_eq!(
        read_fh(&mut c, &b1).await.unwrap_err(),
        nfsstat3::NFS3ERR_IO
    );
    assert_eq!(cat(&mut c, &format!("{v2}/tiny-v2/b.png")).await, b"BBBB");
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
#[tokio::test(flavor = "multi_thread")]
async fn every_format_is_unpacked_recursively() {
    let mock = Arc::new(Mock::default());
    *mock.uploads.lock().unwrap() = support::fixture_uploads();
    let mock_addr = support::spawn(mock.clone()).await;
    let state_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let (_fs, mut c) = serve(mock_addr, state_dir.path(), archive_dir.path()).await;

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
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 5);
}
