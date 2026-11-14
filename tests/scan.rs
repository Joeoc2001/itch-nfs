//! The scanner against a mock itch.io API: what ends up in the archive.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use itch_nfs::archive::Kind;
use itch_nfs::store::{Store, VersionMeta, sha256_hex};
use support::{GAME, Mock, MockGame, MockUpload, scan_config, scanner, snapshot, zip_of};

const PACK: &str = "creator.tiny-pack";

fn paths(dir: &Path) -> Vec<String> {
    snapshot(dir).into_keys().collect()
}

fn versions(archive: &Path, pack: &str) -> Vec<VersionMeta> {
    Store::new(archive, None)
        .load()
        .unwrap()
        .into_iter()
        .find(|p| p.meta.name == pack)
        .map(|p| p.versions)
        .unwrap_or_default()
}

/// After one pass every upload of every pack is in the archive, in the
/// documented layout, without any client having browsed anything.
#[tokio::test(flavor = "multi_thread")]
async fn one_pass_archives_every_upload_of_every_pack() {
    let mock = Arc::new(Mock::default());
    *mock.uploads.lock().unwrap() = support::fixture_uploads();
    // A second pack by another creator, which nobody will ever browse.
    mock.games.lock().unwrap().push(MockGame::new(
        600,
        "Other Pack",
        "https://someone.itch.io/other-pack",
        Some("assets"),
    ));
    mock.uploads.lock().unwrap().insert(
        60,
        MockUpload::new("Other.zip", "2024-07-01", zip_of(&[("o.png", b"oooo")])).of_game(600),
    );
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let scan = scanner(addr, scan_config(archive.path(), cache.path()));

    let report = scan.pass().await.unwrap();
    assert_eq!(report.failed, Vec::<String>::new());
    assert_eq!(report.packs, 2);
    assert_eq!(
        report.archived,
        [
            "creator.tiny-pack/2024-01-05-tiny-pack-v1-u1",
            "creator.tiny-pack/2024-02-05-tiny-pack-v2-u2",
            "creator.tiny-pack/2024-03-05-tiny-docs-u3",
            "creator.tiny-pack/2024-04-05-tiny-extras-u4",
            "creator.tiny-pack/2024-05-05-tiny-manual-u5",
            "someone.other-pack/2024-07-01-other-u60",
        ]
    );
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 6);

    // The layout: the original upload always; content.zip only when the
    // upload is not already a plain zip; nothing else (no temporary files).
    assert_eq!(
        paths(archive.path()),
        [
            // A zip, but with archives nested in it: flattened.
            "packs/creator.tiny-pack/2024-01-05-tiny-pack-v1-u1/content.zip",
            "packs/creator.tiny-pack/2024-01-05-tiny-pack-v1-u1/manifest.json",
            "packs/creator.tiny-pack/2024-01-05-tiny-pack-v1-u1/upload/Tiny Pack v1.zip",
            "packs/creator.tiny-pack/2024-01-05-tiny-pack-v1-u1/version.json",
            "packs/creator.tiny-pack/2024-02-05-tiny-pack-v2-u2/content.zip",
            "packs/creator.tiny-pack/2024-02-05-tiny-pack-v2-u2/manifest.json",
            "packs/creator.tiny-pack/2024-02-05-tiny-pack-v2-u2/upload/Tiny Pack v2.7z",
            "packs/creator.tiny-pack/2024-02-05-tiny-pack-v2-u2/version.json",
            "packs/creator.tiny-pack/2024-03-05-tiny-docs-u3/content.zip",
            "packs/creator.tiny-pack/2024-03-05-tiny-docs-u3/manifest.json",
            "packs/creator.tiny-pack/2024-03-05-tiny-docs-u3/upload/Tiny Docs.rar",
            "packs/creator.tiny-pack/2024-03-05-tiny-docs-u3/version.json",
            "packs/creator.tiny-pack/2024-04-05-tiny-extras-u4/content.zip",
            "packs/creator.tiny-pack/2024-04-05-tiny-extras-u4/manifest.json",
            "packs/creator.tiny-pack/2024-04-05-tiny-extras-u4/upload/Tiny Extras.tar.bz2",
            "packs/creator.tiny-pack/2024-04-05-tiny-extras-u4/version.json",
            // Not an archive: served as it is.
            "packs/creator.tiny-pack/2024-05-05-tiny-manual-u5/manifest.json",
            "packs/creator.tiny-pack/2024-05-05-tiny-manual-u5/upload/Tiny Manual.pdf",
            "packs/creator.tiny-pack/2024-05-05-tiny-manual-u5/version.json",
            "packs/creator.tiny-pack/pack.json",
            // A plain zip: served as it is, no content.zip.
            "packs/someone.other-pack/2024-07-01-other-u60/manifest.json",
            "packs/someone.other-pack/2024-07-01-other-u60/upload/Other.zip",
            "packs/someone.other-pack/2024-07-01-other-u60/version.json",
            "packs/someone.other-pack/pack.json",
        ]
    );
    assert!(
        paths(&cache.path().join("scan-tmp")).is_empty(),
        "scratch space is cleaned up"
    );

    // The archive describes itself: names, numbers, what to serve, checksums.
    let store = Store::new(archive.path(), None);
    let packs = store.load().unwrap();
    assert_eq!(packs.len(), 2);
    let mut numbers: Vec<u32> = packs.iter().map(|p| p.meta.no).collect();
    numbers.sort();
    assert_eq!(numbers, [1, 2]);
    let tiny = packs.iter().find(|p| p.meta.name == PACK).unwrap();
    assert_eq!(tiny.meta.game_id, GAME);
    assert_eq!(tiny.meta.title, "Tiny Pack");
    assert_eq!(tiny.meta.url, "https://creator.itch.io/tiny-pack");
    let uploads = support::fixture_uploads();
    for (i, v) in tiny.versions.iter().enumerate() {
        let original = &uploads[&v.upload_id];
        assert_eq!(v.no as usize, i + 1, "numbered oldest first");
        assert_eq!(v.filename, original.filename);
        assert_eq!(v.fingerprint, support::mock_md5(&original.data));
        assert_eq!(v.size, Some(original.data.len() as u64));
        // The original upload, byte for byte.
        let upload = v.file(v.upload.as_ref().unwrap()).unwrap();
        assert_eq!(upload.path, format!("upload/{}", original.filename));
        assert_eq!(upload.sha256, sha256_hex(&original.data));
        assert_eq!(
            std::fs::read(store.file_path(PACK, &v.name, upload)).unwrap(),
            original.data
        );
        // Every recorded checksum is that of the file in the archive.
        for f in &v.files {
            let bytes = std::fs::read(store.file_path(PACK, &v.name, f)).unwrap();
            assert_eq!(
                (bytes.len() as u64, sha256_hex(&bytes)),
                (f.size, f.sha256.clone())
            );
        }
        assert_eq!(v.note, None);
    }
    let serve: Vec<(Kind, &str)> = tiny
        .versions
        .iter()
        .map(|v| (v.kind, v.serve.as_str()))
        .collect();
    assert_eq!(
        serve,
        [
            (Kind::Zip, "content.zip"),
            (Kind::Zip, "content.zip"),
            (Kind::Zip, "content.zip"),
            (Kind::Zip, "content.zip"),
            (Kind::Raw, "upload/Tiny Manual.pdf"),
        ]
    );
    let other = &packs
        .iter()
        .find(|p| p.meta.name == "someone.other-pack")
        .unwrap()
        .versions[0];
    assert_eq!(
        (other.kind, other.serve.as_str()),
        (Kind::Zip, "upload/Other.zip")
    );

    // A second pass, and a second scanner starting from nothing but the
    // archive (no cache directory), download nothing twice and write nothing.
    let before = snapshot(archive.path());
    let report = scan.pass().await.unwrap();
    assert!(report.archived.is_empty() && report.failed.is_empty());
    let fresh_cache = tempfile::tempdir().unwrap();
    let report = scanner(addr, scan_config(archive.path(), fresh_cache.path()))
        .pass()
        .await
        .unwrap();
    assert!(report.archived.is_empty() && report.failed.is_empty());
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 6);
    assert_eq!(snapshot(archive.path()), before);
}

/// The archive is write-once: a later pass only adds files. Nothing that
/// exists is rewritten, replaced (which is what a rename would do) or removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_replaced_upload_becomes_a_new_version_and_nothing_is_rewritten() {
    let mock = Arc::new(Mock::default());
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new("Tiny.zip", "2024-01-05", zip_of(&[("a.png", b"aaaa")])),
    );
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let scan = scanner(addr, scan_config(archive.path(), cache.path()));
    scan.pass().await.unwrap();
    let before = snapshot(archive.path());
    assert_eq!(before.len(), 4);

    // The creator replaces the file behind upload 1, deletes nothing else,
    // and adds a tarball as upload 2.
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new("Tiny.zip", "2024-01-05", zip_of(&[("a.png", b"AAAA")])),
    );
    mock.uploads.lock().unwrap().insert(
        2,
        MockUpload::new(
            "More.tar.gz",
            "2024-02-01",
            support::tar_gz_of(&[("m.png", b"mmmm")]),
        ),
    );
    let report = scan.pass().await.unwrap();
    assert_eq!(report.failed, Vec::<String>::new());
    assert_eq!(
        report.archived,
        [
            // Same upload id, same name, same date: told apart by a suffix.
            "creator.tiny-pack/2024-01-05-tiny-u1-2",
            "creator.tiny-pack/2024-02-01-more-u2",
        ]
    );
    let vs = versions(archive.path(), PACK);
    let numbered: Vec<(u16, &str)> = vs.iter().map(|v| (v.no, v.name.as_str())).collect();
    assert_eq!(
        numbered,
        [
            (1, "2024-01-05-tiny-u1"),
            (2, "2024-01-05-tiny-u1-2"),
            (3, "2024-02-01-more-u2"),
        ]
    );
    assert_ne!(vs[0].fingerprint, vs[1].fingerprint);

    let after = snapshot(archive.path());
    for (path, stamp) in &before {
        assert_eq!(after.get(path), Some(stamp), "{path} was touched");
    }
    let added: Vec<&str> = after
        .keys()
        .filter(|p| !before.contains_key(*p))
        .map(String::as_str)
        .collect();
    assert_eq!(
        added,
        [
            "packs/creator.tiny-pack/2024-01-05-tiny-u1-2/manifest.json",
            "packs/creator.tiny-pack/2024-01-05-tiny-u1-2/upload/Tiny.zip",
            "packs/creator.tiny-pack/2024-01-05-tiny-u1-2/version.json",
            "packs/creator.tiny-pack/2024-02-01-more-u2/content.zip",
            "packs/creator.tiny-pack/2024-02-01-more-u2/manifest.json",
            "packs/creator.tiny-pack/2024-02-01-more-u2/upload/More.tar.gz",
            "packs/creator.tiny-pack/2024-02-01-more-u2/version.json",
        ]
    );
    // version.json is the commit marker: nothing in a version is written after it.
    for v in ["2024-01-05-tiny-u1-2", "2024-02-01-more-u2"] {
        let dir = format!("packs/creator.tiny-pack/{v}/");
        let marker = after[&format!("{dir}version.json")].mtime;
        for (path, stamp) in after.iter().filter(|(p, _)| p.starts_with(&dir)) {
            assert!(stamp.mtime <= marker, "{path} was written after the marker");
        }
    }
}

/// A version that was being written when the scanner died has no commit
/// marker. The next pass redoes it, under the same name and number.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_version_is_redone() {
    let mock = Arc::new(Mock::default());
    let data = zip_of(&[("a.png", b"aaaa")]);
    mock.uploads.lock().unwrap().insert(
        1,
        MockUpload::new(
            "Tiny.7z",
            "2024-01-05",
            support::sevenz_of(&[("a.png", b"aaaa")]),
        ),
    );
    mock.uploads
        .lock()
        .unwrap()
        .insert(2, MockUpload::new("Two.zip", "2024-02-05", data.clone()));
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());

    // What an interrupted pass leaves: pack.json, and some of the files of
    // the first version (one of them cut short), but no version.json.
    let store = Store::new(archive.path(), None);
    let dir = store.version_dir(PACK, "2024-01-05-tiny-u1");
    std::fs::create_dir_all(dir.join("upload")).unwrap();
    std::fs::write(dir.join("upload/Tiny.7z"), b"7z\xbc\xaf half an upl").unwrap();
    std::fs::write(dir.join("stale.tmp"), b"junk").unwrap();
    // And scratch files of the dead run.
    std::fs::create_dir_all(cache.path().join("scan-tmp/500-1")).unwrap();
    std::fs::write(cache.path().join("scan-tmp/500-1/upload"), b"half").unwrap();
    assert!(versions(archive.path(), PACK).is_empty());

    let scan = scanner(addr, scan_config(archive.path(), cache.path()));
    let report = scan.pass().await.unwrap();
    assert_eq!(report.failed, Vec::<String>::new());
    assert_eq!(report.archived.len(), 2);
    let vs = versions(archive.path(), PACK);
    assert_eq!((vs[0].no, vs[0].name.as_str()), (1, "2024-01-05-tiny-u1"));
    assert_eq!((vs[1].no, vs[1].name.as_str()), (2, "2024-02-05-two-u2"));
    assert_eq!(
        paths(&dir),
        [
            "content.zip",
            "manifest.json",
            "upload/Tiny.7z",
            "version.json"
        ],
        "the leftovers are gone"
    );
    let upload = vs[0].file("upload/Tiny.7z").unwrap();
    assert_eq!(
        upload.sha256,
        sha256_hex(&mock.uploads.lock().unwrap()[&1].data)
    );
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 2);
    assert!(paths(&cache.path().join("scan-tmp")).is_empty());
}

/// A failed upload is reported and retried on the next pass; it does not
/// stop the pack's other uploads or the other packs.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_upload_is_retried_and_blocks_nothing_else() {
    let mock = Arc::new(Mock::default());
    mock.games.lock().unwrap().push(MockGame::new(
        600,
        "Other Pack",
        "https://someone.itch.io/other-pack",
        None,
    ));
    {
        let mut u = mock.uploads.lock().unwrap();
        u.insert(
            1,
            MockUpload::new("One.zip", "2024-01-05", zip_of(&[("a", b"1")])),
        );
        u.insert(
            2,
            MockUpload::new("Two.zip", "2024-02-05", zip_of(&[("b", b"2")])),
        );
        u.insert(
            60,
            MockUpload::new("Other.zip", "2024-07-01", zip_of(&[("o", b"o")])).of_game(600),
        );
    }
    mock.broken.lock().unwrap().insert(1);
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let scan = scanner(addr, scan_config(archive.path(), cache.path()));

    let report = scan.pass().await.unwrap();
    assert_eq!(
        report.archived,
        [
            "creator.tiny-pack/2024-02-05-two-u2",
            "someone.other-pack/2024-07-01-other-u60"
        ]
    );
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert!(
        report.failed[0].starts_with("creator.tiny-pack/2024-01-05-one-u1: "),
        "{:?}",
        report.failed
    );
    assert!(
        !archive
            .path()
            .join("packs/creator.tiny-pack/2024-01-05-one-u1/version.json")
            .exists()
    );
    assert!(paths(&cache.path().join("scan-tmp")).is_empty());

    // itch.io recovers: the next pass archives it. Versions are numbered in
    // the order they were committed.
    mock.broken.lock().unwrap().clear();
    let report = scan.pass().await.unwrap();
    assert_eq!(report.archived, ["creator.tiny-pack/2024-01-05-one-u1"]);
    assert!(report.failed.is_empty());
    let numbered: Vec<(u16, String)> = versions(archive.path(), PACK)
        .into_iter()
        .map(|v| (v.no, v.name))
        .collect();
    assert_eq!(
        numbered,
        [
            (1, "2024-02-05-two-u2".to_string()),
            (2, "2024-01-05-one-u1".to_string())
        ]
    );
    // 3 in the first pass (one of them failed), 1 in the second.
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 4);
}

/// The library cannot be listed: the pass fails as a whole, and says so.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_api_fails_the_pass() {
    let mock = Arc::new(Mock::default());
    let (addr, server) = support::spawn_stoppable(mock.clone()).await;
    server.abort();
    let _ = server.await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let err = scanner(addr, scan_config(archive.path(), cache.path()))
        .pass()
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("list owned packs"), "{err:#}");
    assert!(paths(archive.path()).is_empty());
}

/// `--list` prints what would be archived and downloads nothing; `--only`
/// restricts a scan to named packs.
#[tokio::test(flavor = "multi_thread")]
async fn list_downloads_nothing_and_only_restricts_the_scan() {
    let mock = Arc::new(Mock::default());
    *mock.uploads.lock().unwrap() = support::fixture_uploads();
    mock.games.lock().unwrap().push(MockGame::new(
        600,
        "Other Pack",
        "https://someone.itch.io/other-pack",
        None,
    ));
    mock.uploads.lock().unwrap().insert(
        60,
        MockUpload::new("Other.zip", "2024-07-01", zip_of(&[("o.png", b"oooo")])).of_game(600),
    );
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());

    let report = scanner(addr, scan_config(archive.path(), cache.path()))
        .list()
        .await
        .unwrap();
    assert_eq!(report.packs, 2);
    let pending: Vec<(&str, &str, &str, Option<u64>)> = report
        .pending
        .iter()
        .map(|p| {
            (
                p.pack.as_str(),
                p.version.as_str(),
                p.filename.as_str(),
                p.size,
            )
        })
        .collect();
    let sizes = support::fixture_uploads();
    let size = |id: u64| Some(sizes[&id].data.len() as u64);
    let other_size = Some(zip_of(&[("o.png", b"oooo")]).len() as u64);
    assert_eq!(
        pending,
        [
            (
                PACK,
                "2024-01-05-tiny-pack-v1-u1",
                "Tiny Pack v1.zip",
                size(1)
            ),
            (
                PACK,
                "2024-02-05-tiny-pack-v2-u2",
                "Tiny Pack v2.7z",
                size(2)
            ),
            (PACK, "2024-03-05-tiny-docs-u3", "Tiny Docs.rar", size(3)),
            (
                PACK,
                "2024-04-05-tiny-extras-u4",
                "Tiny Extras.tar.bz2",
                size(4)
            ),
            (
                PACK,
                "2024-05-05-tiny-manual-u5",
                "Tiny Manual.pdf",
                size(5)
            ),
            (
                "someone.other-pack",
                "2024-07-01-other-u60",
                "Other.zip",
                other_size
            ),
        ]
    );
    assert_eq!(
        mock.downloads.load(Ordering::SeqCst),
        0,
        "a dry run downloads nothing"
    );
    assert!(paths(archive.path()).is_empty(), "a dry run writes nothing");

    // Only the other pack.
    let mut cfg = scan_config(archive.path(), cache.path());
    cfg.only = vec!["someone.other-pack".into()];
    let report = scanner(addr, cfg).pass().await.unwrap();
    assert_eq!(report.packs, 1);
    assert_eq!(report.archived, ["someone.other-pack/2024-07-01-other-u60"]);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(
        Store::new(archive.path(), None).load().unwrap().len(),
        1,
        "the pack that was not asked for is not in the archive"
    );

    // What is archived is no longer listed.
    let report = scanner(addr, scan_config(archive.path(), cache.path()))
        .list()
        .await
        .unwrap();
    assert_eq!(report.pending.len(), 5);
    assert!(report.pending.iter().all(|p| p.pack == PACK));

    // A name that matches nothing is a reported problem, not a silent no-op.
    let mut cfg = scan_config(archive.path(), cache.path());
    cfg.only = vec!["no.such-pack".into()];
    let report = scanner(addr, cfg).pass().await.unwrap();
    assert_eq!(report.packs, 0);
    assert_eq!(report.failed, ["no.such-pack: no such pack in the library"]);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 1);
}

/// By default only asset packs are archived, not the games the account owns.
#[tokio::test(flavor = "multi_thread")]
async fn classification_decides_what_is_in_scope() {
    let mock = Arc::new(Mock::default());
    {
        let mut games = mock.games.lock().unwrap();
        games.clear();
        games.push(MockGame::new(
            1,
            "Assets",
            "https://a.itch.io/assets",
            Some("assets"),
        ));
        games.push(MockGame::new(
            2,
            "A Game",
            "https://a.itch.io/a-game",
            Some("game"),
        ));
        games.push(MockGame::new(
            3,
            "Music",
            "https://a.itch.io/music",
            Some("soundtrack"),
        ));
        // The API says nothing about this one: it is not left out on a guess.
        games.push(MockGame::new(
            4,
            "Unknown",
            "https://a.itch.io/unknown",
            None,
        ));
        let mut u = mock.uploads.lock().unwrap();
        for id in 1..=4u64 {
            let body = zip_of(&[("f", id.to_string().as_bytes())]);
            u.insert(id, MockUpload::new("f.zip", "2024-01-05", body).of_game(id));
        }
    }
    let addr = support::spawn(mock.clone()).await;
    let names = |archive: &Path| -> Vec<String> {
        let mut n: Vec<String> = Store::new(archive, None)
            .load()
            .unwrap()
            .into_iter()
            .map(|p| p.meta.name)
            .collect();
        n.sort();
        n
    };

    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut cfg = scan_config(archive.path(), cache.path());
    cfg.classifications = vec!["assets".into()];
    let report = scanner(addr, cfg).pass().await.unwrap();
    assert_eq!(report.packs, 2);
    assert_eq!(names(archive.path()), ["a.assets", "a.unknown"]);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 2);

    let mut cfg = scan_config(archive.path(), cache.path());
    cfg.classifications = vec!["assets".into(), "Soundtrack".into()];
    scanner(addr, cfg).pass().await.unwrap();
    assert_eq!(names(archive.path()), ["a.assets", "a.music", "a.unknown"]);

    // No filter: everything, games included. Pack numbers follow the order
    // packs were first archived in, and never change.
    let cfg = scan_config(archive.path(), cache.path());
    scanner(addr, cfg).pass().await.unwrap();
    assert_eq!(
        names(archive.path()),
        ["a.a-game", "a.assets", "a.music", "a.unknown"]
    );
    let mut numbers: Vec<(u32, String)> = Store::new(archive.path(), None)
        .load()
        .unwrap()
        .into_iter()
        .map(|p| (p.meta.no, p.meta.name))
        .collect();
    numbers.sort();
    assert_eq!(numbers[2], (3, "a.music".to_string()));
    assert_eq!(numbers[3], (4, "a.a-game".to_string()));
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 4);
}

/// An upload over the unpacking limit is still archived, and served as the
/// upload itself, with the reason on record.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_over_the_limit_is_archived_as_a_single_file() {
    let mock = Arc::new(Mock::default());
    let big = vec![0u8; 200_000];
    let bomb = support::tar_gz_of(&[("a", &big), ("b", &big), ("c", &big)]);
    {
        let mut u = mock.uploads.lock().unwrap();
        u.insert(
            1,
            MockUpload::new("Bomb.tar.gz", "2024-01-05", bomb.clone()),
        );
        u.insert(
            2,
            MockUpload::new("Fine.zip", "2024-02-05", zip_of(&[("a", b"1")])),
        );
    }
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut cfg = scan_config(archive.path(), cache.path());
    cfg.max_unpacked_bytes = 500_000;
    let report = scanner(addr, cfg).pass().await.unwrap();
    assert_eq!(report.failed, Vec::<String>::new());
    assert_eq!(report.archived.len(), 2);

    let vs = versions(archive.path(), PACK);
    assert_eq!(vs[0].kind, Kind::Raw);
    assert_eq!(vs[0].serve, "upload/Bomb.tar.gz");
    let note = vs[0].note.as_deref().unwrap();
    assert!(
        note.contains("more than the limit of 500000 bytes"),
        "{note}"
    );
    assert_eq!(
        paths(&archive.path().join("packs").join(PACK).join(&vs[0].name)),
        ["manifest.json", "upload/Bomb.tar.gz", "version.json"]
    );
    assert_eq!(
        std::fs::read(
            archive
                .path()
                .join("packs")
                .join(PACK)
                .join(&vs[0].name)
                .join("upload/Bomb.tar.gz")
        )
        .unwrap(),
        bomb
    );
    assert_eq!((vs[1].kind, vs[1].note.as_deref()), (Kind::Zip, None));
}

/// The file behind an upload id is replaced while it is being downloaded.
/// What was downloaded no longer matches what itch.io describes, so it is not
/// archived under the old description; the next pass archives the new file.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_replaced_mid_download_is_not_archived_as_the_old_one() {
    let mock = Arc::new(Mock::default());
    let old = zip_of(&[("a.png", b"old!")]);
    let new = zip_of(&[("a.png", b"new!"), ("b.png", b"more")]);
    mock.uploads
        .lock()
        .unwrap()
        .insert(1, MockUpload::new("Tiny.zip", "2024-01-05", old));
    mock.swap_on_download
        .lock()
        .unwrap()
        .insert(1, MockUpload::new("Tiny.zip", "2024-03-01", new.clone()));
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let scan = scanner(addr, scan_config(archive.path(), cache.path()));

    let report = scan.pass().await.unwrap();
    assert!(report.archived.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert!(
        report.failed[0].contains("changed while it was downloaded"),
        "{:?}",
        report.failed
    );
    assert!(versions(archive.path(), PACK).is_empty());

    let report = scan.pass().await.unwrap();
    assert_eq!(report.archived, ["creator.tiny-pack/2024-03-01-tiny-u1"]);
    let vs = versions(archive.path(), PACK);
    assert_eq!(vs.len(), 1);
    assert_eq!(vs[0].no, 1);
    assert_eq!(
        vs[0].file("upload/Tiny.zip").unwrap().sha256,
        sha256_hex(&new)
    );
}

/// Requests to itch.io are spaced out, however many packs are scanned at once.
#[tokio::test(flavor = "multi_thread")]
async fn requests_are_paced() {
    let mock = Arc::new(Mock::default());
    for id in 2..=5u64 {
        mock.games.lock().unwrap().push(MockGame::new(
            id,
            "P",
            &format!("https://c.itch.io/p{id}"),
            None,
        ));
    }
    let addr = support::spawn(mock.clone()).await;
    let (archive, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut cfg = scan_config(archive.path(), cache.path());
    cfg.concurrency = 5;
    let delay = std::time::Duration::from_millis(40);
    let scan = itch_nfs::scan::Scanner::new(cfg, support::client(addr).with_delay(delay)).unwrap();
    let started = std::time::Instant::now();
    let report = scan.pass().await.unwrap();
    assert_eq!(report.packs, 5);
    // Two pages of owned keys, then one uploads call per pack.
    let requests = mock.requests.load(Ordering::SeqCst) as u32;
    assert_eq!(requests, 7);
    assert!(
        started.elapsed() >= delay * (requests - 1),
        "{requests} requests took only {:?}",
        started.elapsed()
    );
}
