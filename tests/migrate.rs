//! `migrate`: importing a deployment from before the archive directory existed.

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use itch_nfs::archive::{self, Kind};
use itch_nfs::fs::{Config, ItchFs};
use itch_nfs::ingest::{Limits, Outcome, ingest};
use itch_nfs::migrate::{MigrateConfig, legacy_deployment, migrate};
use itch_nfs::state::{GameState, State, VersionState, parse_ts, version_dir_name};
use itch_nfs::store::Store;
use nfs3_client::Nfs3ConnectionBuilder;
use nfs3_client::nfs3_types::nfs3::{self, Nfs3Result, filename3, nfsstat3};
use nfs3_client::nfs3_types::xdr_codec::Opaque;
use nfs3_client::tokio::{TokioConnector, TokioIo};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use support::{GAME, KEY, Mock, MockUpload, scan_config, scanner, snapshot};

type Conn = nfs3_client::Nfs3Connection<TokioIo<tokio::net::TcpStream>>;

/// The pack number the old deployment had given the pack. Not 1, so that a
/// migration that renumbered would be caught.
const PACK_NO: u32 = 3;
const PACK: &str = "creator.tiny-pack";
const NAMES: [&str; 5] = [
    "2024-01-05-tiny-pack-v1-u1",
    "2024-02-05-tiny-pack-v2-u2",
    "2024-03-05-tiny-docs-u3",
    "2024-04-05-tiny-extras-u4",
    "2024-05-05-tiny-manual-u5",
];

struct Old {
    state_dir: tempfile::TempDir,
    archive_dir: tempfile::TempDir,
}

/// Lay out what the old single-process server left behind for the fixture
/// pack, the way it did it: `state.json` listing every version, and, for each
/// version in `downloaded`, `<game id>-<n>.zip|raw` (a plain zip as it was
/// downloaded, anything else flattened, the original thrown away) plus
/// `manifests/<game id>-<n>.json`.
fn old_deployment(uploads: &BTreeMap<u64, MockUpload>, downloaded: &[u64]) -> Old {
    let old = Old {
        state_dir: tempfile::tempdir().unwrap(),
        archive_dir: tempfile::tempdir().unwrap(),
    };
    let (state_dir, archive_dir) = (old.state_dir.path(), old.archive_dir.path());
    std::fs::create_dir_all(state_dir.join("manifests")).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let mut versions = Vec::new();
    for (i, (id, u)) in uploads.iter().enumerate() {
        let ver = i + 1;
        let timestamp = parse_ts(Some(&u.updated_at)).unwrap();
        versions.push(VersionState {
            name: version_dir_name(timestamp, &u.filename, *id, None),
            upload_id: *id,
            build_id: None,
            fingerprint: support::mock_md5(&u.data),
            filename: u.filename.clone(),
            size: Some(u.data.len() as u64),
            timestamp,
        });
        if !downloaded.contains(id) {
            continue;
        }
        let part = archive_dir.join(format!("{GAME}-{ver}.part"));
        let flat = archive_dir.join(format!("{GAME}-{ver}.flat"));
        std::fs::write(&part, &u.data).unwrap();
        let limits = Limits {
            max_unpacked_bytes: 1 << 30,
            max_depth: 8,
        };
        let outcome = ingest(&part, &u.filename, &flat, scratch.path(), limits).unwrap();
        let (from, zip) = match outcome {
            Outcome::ZipAsIs => (&part, true),
            Outcome::Flattened => (&flat, true),
            Outcome::Raw => (&part, false),
        };
        let dest = archive_dir.join(format!("{GAME}-{ver}.{}", if zip { "zip" } else { "raw" }));
        std::fs::rename(from, &dest).unwrap();
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(&flat);
        let manifest = archive::scan(&dest, &u.filename, zip).unwrap();
        // The old server wrote the kind as "Zip"/"Raw".
        let json = serde_json::to_string(&manifest)
            .unwrap()
            .replace("\"kind\":\"zip\"", "\"kind\":\"Zip\"")
            .replace("\"kind\":\"raw\"", "\"kind\":\"Raw\"");
        assert!(json.contains("\"kind\":\"Zip\"") || json.contains("\"kind\":\"Raw\""));
        std::fs::write(state_dir.join(format!("manifests/{GAME}-{ver}.json")), json).unwrap();
    }
    let mut state = State::default();
    // Packs 1 and 2 existed once; an owned pack that was never browsed.
    state.games.insert(
        2,
        GameState {
            game_id: 444,
            name: "someone.never-browsed".into(),
            title: "Never Browsed".into(),
            key_id: Some(1444),
            versions: Vec::new(),
            changed_at: 1_700_000_000,
        },
    );
    state.games.insert(
        PACK_NO,
        GameState {
            game_id: GAME,
            name: PACK.into(),
            title: "Tiny Pack".into(),
            key_id: Some(KEY),
            versions,
            changed_at: 1_700_000_000,
        },
    );
    state.changed_at = 1_700_000_000;
    state.save(state_dir).unwrap();
    old
}

fn migrate_config(old: &Old, archive: &Path, cache: &Path) -> MigrateConfig {
    MigrateConfig {
        state_dir: old.state_dir.path().to_path_buf(),
        old_archive_dir: old.archive_dir.path().to_path_buf(),
        archive_dir: archive.to_path_buf(),
        cache_dir: cache.to_path_buf(),
        max_unpacked_bytes: 1 << 30,
    }
}

async fn serve(archive_dir: &Path, cache_dir: &Path) -> (ItchFs, Conn) {
    let fs = ItchFs::new(Config {
        archive_dir: archive_dir.to_path_buf(),
        cache_dir: cache_dir.to_path_buf(),
        cache_max_bytes: 64 * 1024,
        cache_ttl: Duration::from_secs(60),
        rescan_interval: Duration::from_secs(3600),
        disk_cache_max_bytes: 1 << 30,
    })
    .unwrap();
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

async fn walk(c: &mut Conn, path: &str) -> Result<nfs3::nfs_fh3, nfsstat3> {
    let mut fh = c.root_nfs_fh3();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let args = nfs3::LOOKUP3args {
            what: nfs3::diropargs3 {
                dir: fh.clone(),
                name: filename3(Opaque::borrowed(part.as_bytes())),
            },
        };
        fh = match c.lookup(&args).await.unwrap() {
            Nfs3Result::Ok(r) => r.object,
            Nfs3Result::Err((code, _)) => return Err(code),
        };
    }
    Ok(fh)
}

async fn ls(c: &mut Conn, path: &str) -> Vec<String> {
    let dir = walk(c, path).await.unwrap();
    let r = c
        .readdirplus(&nfs3::READDIRPLUS3args {
            dir,
            cookie: nfs3::cookie3::default(),
            cookieverf: nfs3::cookieverf3::default(),
            dircount: 100_000,
            maxcount: 100_000,
        })
        .await
        .unwrap()
        .unwrap();
    assert!(r.reply.eof);
    let mut names: Vec<String> = r
        .reply
        .entries
        .into_inner()
        .into_iter()
        .map(|e| String::from_utf8(e.name.0.to_vec()).unwrap())
        .filter(|n| n != "." && n != "..")
        .collect();
    names.sort();
    names
}

async fn cat(c: &mut Conn, path: &str) -> Vec<u8> {
    let fh = walk(c, path)
        .await
        .unwrap_or_else(|e| panic!("{path}: {e:?}"));
    let mut out = Vec::new();
    loop {
        let r = c
            .read(&nfs3::READ3args {
                file: fh.clone(),
                offset: out.len() as u64,
                count: 50_000,
            })
            .await
            .unwrap()
            .unwrap();
        out.extend_from_slice(&r.data.0);
        if r.eof {
            return out;
        }
    }
}

/// Every regular file under `path`, with its NFS file handle.
async fn tree(c: &mut Conn, path: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    for name in ls(c, path).await {
        let child = format!("{path}/{name}");
        let fh = walk(c, &child).await.unwrap();
        let attr = c
            .getattr(&nfs3::GETATTR3args { object: fh.clone() })
            .await
            .unwrap()
            .unwrap()
            .obj_attributes;
        if matches!(attr.type_, nfs3::ftype3::NF3DIR) {
            Box::pin(tree(c, &child, out)).await;
        } else {
            out.insert(child, fh.data.0.to_vec());
        }
    }
}

fn handle(pack: u32, version: u16, node: u32) -> Vec<u8> {
    ((pack as u64) << 44 | (version as u64) << 28 | node as u64)
        .to_be_bytes()
        .to_vec()
}

/// A fully downloaded old deployment keeps its pack name, version names,
/// numbers, paths and file handles; and the scanner carries on from there.
#[tokio::test(flavor = "multi_thread")]
async fn a_migrated_deployment_keeps_its_names_and_handles() {
    let uploads = support::fixture_uploads();
    let old = old_deployment(&uploads, &[1, 2, 3, 4, 5]);
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let cfg = migrate_config(&old, archive.path(), cache.path());

    // Until it is migrated, starting on top of it is refused (see main.rs):
    // whether the old state directory is still configured,
    let state = Some(old.state_dir.path());
    let why = legacy_deployment(archive.path(), cache.path(), state).unwrap();
    assert!(why.contains("state.json exists"), "{why}");
    // or --archive-dir still points at the old directory of downloads.
    let why = legacy_deployment(old.archive_dir.path(), cache.path(), None).unwrap();
    assert!(why.contains("download from the old layout"), "{why}");
    // or, as after an upgrade of the Docker image that kept the old volumes,
    // the state directory is neither mounted nor configured any more and the
    // old downloads are in what is now the cache directory, next to a new and
    // empty archive.
    let why = legacy_deployment(archive.path(), old.archive_dir.path(), None).unwrap();
    assert!(why.contains("download from the old layout"), "{why}");
    let nowhere = archive.path().join("not-created-yet");
    let why = legacy_deployment(&nowhere, old.archive_dir.path(), None).unwrap();
    assert!(why.contains("download from the old layout"), "{why}");
    assert_eq!(legacy_deployment(archive.path(), cache.path(), None), None);

    let before = (
        snapshot(old.state_dir.path()),
        snapshot(old.archive_dir.path()),
    );
    // No token: nothing is fetched, nothing needs to be.
    let report = migrate(&cfg, None).await.unwrap();
    assert_eq!(report.packs, 2);
    assert_eq!(
        report.imported,
        NAMES.map(|n| format!("{PACK}/{n}")).to_vec()
    );
    assert!(report.fetched.is_empty() && report.gaps.is_empty());
    assert_eq!(
        before,
        (
            snapshot(old.state_dir.path()),
            snapshot(old.archive_dir.path())
        ),
        "the old directories are only read"
    );
    assert_eq!(
        legacy_deployment(archive.path(), cache.path(), state),
        None,
        "once migrated, the old state is no longer an obstacle"
    );

    // The archive records the old numbers and names.
    let store = Store::new(archive.path(), None);
    let packs = store.load().unwrap();
    let numbers: Vec<(u32, &str, usize)> = packs
        .iter()
        .map(|p| (p.meta.no, p.meta.name.as_str(), p.versions.len()))
        .collect();
    assert_eq!(
        numbers,
        [(2, "someone.never-browsed", 0), (PACK_NO, PACK, 5)]
    );
    let vs = &packs[1].versions;
    for (i, v) in vs.iter().enumerate() {
        assert_eq!((v.no as usize, v.name.as_str()), (i + 1, NAMES[i]));
        assert_eq!(
            v.fingerprint,
            support::mock_md5(&uploads[&v.upload_id].data)
        );
    }
    // v1 has archives nested in it, so the old server had flattened it, like
    // the 7z, rar and tar.bz2: no original exists, and version.json says so.
    for v in &vs[..4] {
        assert_eq!((v.kind, v.serve.as_str()), (Kind::Zip, "content.zip"));
        assert_eq!(v.upload, None);
        assert!(
            v.note
                .as_deref()
                .unwrap()
                .contains("did not keep original uploads")
        );
    }
    // The pdf was kept as downloaded: it is its own original.
    assert_eq!(
        (vs[4].kind, vs[4].serve.as_str(), vs[4].upload.as_deref()),
        (
            Kind::Raw,
            "upload/Tiny Manual.pdf",
            Some("upload/Tiny Manual.pdf")
        )
    );
    assert_eq!(vs[4].note, None);

    // Served: the same tree as ever (ci/mock-expected.txt predates all this),
    // under the old numbers.
    let (_fs, mut c) = serve(archive.path(), cache.path()).await;
    assert_eq!(ls(&mut c, "/").await, [PACK]);
    let mut migrated = BTreeMap::new();
    tree(&mut c, PACK, &mut migrated).await;
    let listing: String = migrated.keys().map(|p| format!("{p}\n")).collect();
    let expected =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ci/mock-expected.txt"))
            .unwrap();
    assert_eq!(listing, expected);
    assert_eq!(
        walk(&mut c, PACK).await.unwrap().data.0,
        handle(PACK_NO, 0, 0)
    );
    for (i, name) in NAMES.iter().enumerate() {
        assert_eq!(
            walk(&mut c, &format!("{PACK}/{name}"))
                .await
                .unwrap()
                .data
                .0,
            handle(PACK_NO, i as u16 + 1, 0)
        );
    }
    assert_eq!(
        cat(
            &mut c,
            &format!(
                "{PACK}/{}/tiny/extras.tar.gz/more/deeper.zip/deep.txt",
                NAMES[0]
            )
        )
        .await,
        b"deep\n"
    );
    assert_eq!(
        cat(&mut c, &format!("{PACK}/{}/Tiny Manual.pdf", NAMES[4])).await,
        b"%PDF-1.4 not really"
    );

    // File handles: the same as a deployment that was never migrated has for
    // the same uploads, but for the pack number (the top 20 bits), which is
    // the old deployment's. Node numbers come from the listings alone.
    let mock = Arc::new(Mock::default());
    *mock.uploads.lock().unwrap() = support::fixture_uploads();
    let addr = support::spawn(mock.clone()).await;
    let (fresh_archive, fresh_cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    scanner(addr, scan_config(fresh_archive.path(), fresh_cache.path()))
        .pass()
        .await
        .unwrap();
    let (_fresh_fs, mut fresh_c) = serve(fresh_archive.path(), fresh_cache.path()).await;
    let mut fresh = BTreeMap::new();
    tree(&mut fresh_c, PACK, &mut fresh).await;
    assert_eq!(fresh.len(), migrated.len());
    for (path, fh) in &migrated {
        let id = u64::from_be_bytes(fh.as_slice().try_into().unwrap());
        let fresh_id = u64::from_be_bytes(fresh[path].as_slice().try_into().unwrap());
        assert_eq!(id >> 44, PACK_NO as u64, "{path}");
        assert_eq!(fresh_id >> 44, 1, "{path}");
        assert_eq!(id & 0xFFF_FFFF_FFFF, fresh_id & 0xFFF_FFFF_FFFF, "{path}");
    }

    // Running it again changes nothing.
    let stamp = snapshot(archive.path());
    let report = migrate(&cfg, None).await.unwrap();
    assert_eq!(report.already, 5);
    assert!(report.imported.is_empty() && report.gaps.is_empty());
    assert_eq!(snapshot(archive.path()), stamp);

    // The scanner picks up where the old server left off: everything itch.io
    // lists is already archived, and a new upload gets the next number.
    let scan = scanner(addr, scan_config(archive.path(), cache.path()));
    let report = scan.pass().await.unwrap();
    assert!(report.archived.is_empty() && report.failed.is_empty());
    assert_eq!(
        mock.downloads.load(Ordering::SeqCst),
        5,
        "only the fresh archive's"
    );
    mock.uploads.lock().unwrap().insert(
        6,
        MockUpload::new(
            "Six.zip",
            "2024-06-05",
            support::zip_of(&[("six.png", b"6666")]),
        ),
    );
    let report = scan.pass().await.unwrap();
    assert_eq!(report.archived, [format!("{PACK}/2024-06-05-six-u6")]);
    let packs = store.load().unwrap();
    let tiny = packs.iter().find(|p| p.meta.name == PACK).unwrap();
    assert_eq!(tiny.meta.no, PACK_NO);
    assert_eq!(tiny.versions.last().unwrap().no, 6);
}

/// Versions the old server had listed but never downloaded: fetched if
/// itch.io still serves exactly that upload, otherwise left as a gap.
#[tokio::test(flavor = "multi_thread")]
async fn versions_that_were_never_downloaded_are_fetched_or_left_as_gaps() {
    let uploads = support::fixture_uploads();
    // Only the first and the fourth were ever opened.
    let old = old_deployment(&uploads, &[1, 4]);
    let mock = Arc::new(Mock::default());
    *mock.uploads.lock().unwrap() = support::fixture_uploads();
    // Since then the creator deleted upload 2 and replaced upload 3; upload 5
    // is still what it was.
    mock.uploads.lock().unwrap().remove(&2);
    mock.uploads.lock().unwrap().insert(
        3,
        MockUpload::new(
            "Tiny Docs.rar",
            "2024-03-05",
            support::rar_of(&[("docs/readme.txt", b"rewritten\n")]),
        ),
    );
    let addr = support::spawn(mock.clone()).await;

    // Without a token nothing can be fetched: three gaps.
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let report = migrate(&migrate_config(&old, archive.path(), cache.path()), None)
        .await
        .unwrap();
    assert_eq!(report.imported.len(), 2);
    assert_eq!(report.gaps.len(), 3);
    assert_eq!(mock.requests.load(Ordering::SeqCst), 0);

    // With one, into a new archive: version 5 is fetched under its old name
    // and number; 2 and 3 are gone for good.
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let cfg = migrate_config(&old, archive.path(), cache.path());
    let client = support::client(addr);
    let report = migrate(&cfg, Some(&client)).await.unwrap();
    assert_eq!(
        report.imported,
        [
            format!("{PACK}/{}", NAMES[0]),
            format!("{PACK}/{}", NAMES[3])
        ]
    );
    assert_eq!(report.fetched, [format!("{PACK}/{}", NAMES[4])]);
    assert_eq!(
        report.gaps,
        [
            format!("{PACK}/{}", NAMES[1]),
            format!("{PACK}/{}", NAMES[2])
        ]
    );
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 1);

    let store = Store::new(archive.path(), None);
    let packs = store.load().unwrap();
    let tiny = packs.iter().find(|p| p.meta.name == PACK).unwrap();
    let numbered: Vec<(u16, &str)> = tiny
        .versions
        .iter()
        .map(|v| (v.no, v.name.as_str()))
        .collect();
    assert_eq!(numbered, [(1, NAMES[0]), (4, NAMES[3]), (5, NAMES[4])]);
    // The fetched one has its original, like anything the scanner archives.
    assert_eq!(
        tiny.versions[2].upload.as_deref(),
        Some("upload/Tiny Manual.pdf")
    );

    // The frontend serves around the gap, with the old handles.
    let (_fs, mut c) = serve(archive.path(), cache.path()).await;
    assert_eq!(ls(&mut c, PACK).await, [NAMES[0], NAMES[3], NAMES[4]]);
    for (no, name) in numbered {
        assert_eq!(
            walk(&mut c, &format!("{PACK}/{name}"))
                .await
                .unwrap()
                .data
                .0,
            handle(PACK_NO, no, 0)
        );
    }
    assert_eq!(
        cat(&mut c, &format!("{PACK}/{}/fonts.tar.zst/f.ttf", NAMES[3])).await,
        b"font"
    );
    assert_eq!(
        cat(&mut c, &format!("{PACK}/{}/Tiny Manual.pdf", NAMES[4])).await,
        b"%PDF-1.4 not really"
    );
    // b.png of version 4 duplicates version 2's, which is a gap: with no
    // earlier copy to point at, it is visible rather than lost.
    assert_eq!(
        cat(&mut c, &format!("{PACK}/{}/b.png", NAMES[3])).await,
        b"BBBB"
    );

    // Migrating again fetches nothing more and breaks nothing.
    let stamp = snapshot(archive.path());
    let report = migrate(&cfg, Some(&client)).await.unwrap();
    assert_eq!((report.already, report.gaps.len()), (3, 2));
    assert_eq!(snapshot(archive.path()), stamp);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 1);
}

/// An archive that already holds a different history is not mixed with.
#[tokio::test(flavor = "multi_thread")]
async fn a_conflicting_archive_is_refused() {
    let uploads = support::fixture_uploads();
    let old = old_deployment(&uploads, &[1]);
    let mock = Arc::new(Mock::default());
    *mock.uploads.lock().unwrap() = support::fixture_uploads();
    let addr = support::spawn(mock.clone()).await;
    // The scanner got there first, and numbered the pack 1 rather than 3.
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    scanner(addr, scan_config(archive.path(), cache.path()))
        .pass()
        .await
        .unwrap();
    let stamp = snapshot(archive.path());
    let err = migrate(&migrate_config(&old, archive.path(), cache.path()), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("conflicts"), "{err:#}");
    assert_eq!(snapshot(archive.path()), stamp);

    // And with nothing to migrate from, it says so.
    let empty = Old {
        state_dir: tempfile::tempdir().unwrap(),
        archive_dir: tempfile::tempdir().unwrap(),
    };
    let err = migrate(&migrate_config(&empty, archive.path(), cache.path()), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nothing to migrate"), "{err:#}");
}

/// A plain zip was kept as downloaded by the old server, so it is imported
/// as the original upload it is.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_zip_is_imported_as_the_original_upload() {
    let body = support::zip_of(&[("a.png", b"aaaa"), ("b.png", b"bbbb")]);
    let uploads = BTreeMap::from([(1, MockUpload::new("Plain.zip", "2024-01-05", body.clone()))]);
    let old = old_deployment(&uploads, &[1]);
    assert_eq!(
        std::fs::read(old.archive_dir.path().join(format!("{GAME}-1.zip"))).unwrap(),
        body
    );
    // The listing was never written (the old server died first): it is made now.
    std::fs::remove_file(
        old.state_dir
            .path()
            .join(format!("manifests/{GAME}-1.json")),
    )
    .unwrap();
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let report = migrate(&migrate_config(&old, archive.path(), cache.path()), None)
        .await
        .unwrap();
    assert_eq!(report.imported, [format!("{PACK}/2024-01-05-plain-u1")]);

    let store = Store::new(archive.path(), None);
    let packs = store.load().unwrap();
    let v = &packs.iter().find(|p| p.meta.name == PACK).unwrap().versions[0];
    assert_eq!(
        (
            v.kind,
            v.serve.as_str(),
            v.upload.as_deref(),
            v.note.as_deref()
        ),
        (
            Kind::Zip,
            "upload/Plain.zip",
            Some("upload/Plain.zip"),
            None
        )
    );
    let upload = v.file("upload/Plain.zip").unwrap();
    assert_eq!(upload.sha256, itch_nfs::store::sha256_hex(&body));
    assert_eq!(
        snapshot(archive.path()).into_keys().collect::<Vec<_>>(),
        [
            "packs/creator.tiny-pack/2024-01-05-plain-u1/manifest.json",
            "packs/creator.tiny-pack/2024-01-05-plain-u1/upload/Plain.zip",
            "packs/creator.tiny-pack/2024-01-05-plain-u1/version.json",
            "packs/creator.tiny-pack/pack.json",
            "packs/someone.never-browsed/pack.json",
        ]
    );
    let (_fs, mut c) = serve(archive.path(), cache.path()).await;
    assert_eq!(
        cat(&mut c, &format!("{PACK}/2024-01-05-plain-u1/b.png")).await,
        b"bbbb"
    );
}
