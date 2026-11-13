//! Unpacking uploads into a flat zip.

mod support;

use std::collections::BTreeMap;
use std::io::Read;

use itch_nfs::ingest::{Limits, Outcome, ingest};
use support::*;

const LIMITS: Limits = Limits {
    max_unpacked_bytes: 1 << 20,
    max_depth: 8,
};

/// Ingest `data` as an upload called `name`; return the outcome and, if it
/// was unpacked, every path and its contents.
fn run(
    name: &str,
    data: &[u8],
    limits: Limits,
) -> anyhow::Result<(Outcome, BTreeMap<String, Vec<u8>>)> {
    let dir = tempfile::tempdir().unwrap();
    let (src, out) = (dir.path().join("upload"), dir.path().join("flat"));
    std::fs::write(&src, data).unwrap();
    let outcome = ingest(&src, name, &out, dir.path(), limits)?;
    let zip_path = match outcome {
        Outcome::ZipAsIs => &src,
        Outcome::Flattened => &out,
        Outcome::Raw => {
            assert!(!out.exists(), "a failed unpack leaves nothing behind");
            return Ok((outcome, BTreeMap::new()));
        }
    };
    let mut zip = zip::ZipArchive::new(std::fs::File::open(zip_path).unwrap()).unwrap();
    let mut files = BTreeMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        files.insert(f.name().unwrap().into_owned(), buf);
    }
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "upload" && n != "flat")
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
    Ok((outcome, files))
}

fn names(files: &BTreeMap<String, Vec<u8>>) -> Vec<&str> {
    files.keys().map(String::as_str).collect()
}

const FILES: Files = &[("dir/a.txt", b"alpha"), ("b.txt", b"beta")];

#[test]
fn every_format_unpacks_to_the_same_files() {
    type Pack = fn(Files) -> Vec<u8>;
    let formats: &[(&str, Pack)] = &[
        ("p.7z", sevenz_of),
        ("p.rar", rar_of),
        ("p.tar", tar_of),
        ("p.tar.gz", tar_gz_of),
        ("p.tgz", tar_gz_of),
        ("p.tar.bz2", tar_bz2_of),
        ("p.tar.xz", tar_xz_of),
        ("p.tar.zst", tar_zst_of),
    ];
    for (name, pack) in formats {
        let (outcome, files) = run(name, &pack(FILES), LIMITS).unwrap();
        assert_eq!(outcome, Outcome::Flattened, "{name}");
        assert_eq!(names(&files), ["b.txt", "dir/a.txt"], "{name}");
        assert_eq!(files["dir/a.txt"], b"alpha", "{name}");
    }
    // The extension does not have to tell the truth about the format.
    let (outcome, files) = run("p.zip", &sevenz_of(FILES), LIMITS).unwrap();
    assert_eq!(
        (outcome, names(&files)),
        (Outcome::Flattened, vec!["b.txt", "dir/a.txt"])
    );
}

#[test]
fn a_plain_zip_is_served_as_it_is() {
    let (outcome, files) = run("p.zip", &zip_of(FILES), LIMITS).unwrap();
    assert_eq!(outcome, Outcome::ZipAsIs);
    assert_eq!(names(&files), ["b.txt", "dir/a.txt"]);
}

#[test]
fn nested_archives_become_directories() {
    let inner = zip_of(&[("deep.txt", b"deep")]);
    let middle = tar_gz_of(&[("m.txt", b"middle"), ("sub/inner.zip", &inner)]);
    let outer = zip_of(&[
        ("top.txt", b"top"),
        ("packs/middle.tar.gz", &middle),
        ("packs/also.7z", &sevenz_of(FILES)),
        ("__MACOSX/._top.txt", b"junk"),
    ]);
    let (outcome, files) = run("outer.zip", &outer, LIMITS).unwrap();
    assert_eq!(outcome, Outcome::Flattened);
    assert_eq!(
        names(&files),
        [
            "packs/also.7z/b.txt",
            "packs/also.7z/dir/a.txt",
            "packs/middle.tar.gz/m.txt",
            "packs/middle.tar.gz/sub/inner.zip/deep.txt",
            "top.txt"
        ]
    );
    assert_eq!(files["packs/middle.tar.gz/sub/inner.zip/deep.txt"], b"deep");
}

#[test]
fn archives_in_disguise_and_broken_archives_stay_whole() {
    let unitypackage = tar_gz_of(FILES);
    let outer = tar_of(&[
        ("tool.unitypackage", &unitypackage),
        ("broken.zip", b"PK\x03\x04 nope"),
        ("lies.rar", b"just text"),
        ("ok.zip", &zip_of(FILES)),
    ]);
    let (_, files) = run("outer.tar", &outer, LIMITS).unwrap();
    assert_eq!(
        names(&files),
        [
            "broken.zip",
            "lies.rar",
            "ok.zip/b.txt",
            "ok.zip/dir/a.txt",
            "tool.unitypackage"
        ]
    );
    assert_eq!(files["tool.unitypackage"], unitypackage);
    assert_eq!(files["broken.zip"], b"PK\x03\x04 nope");

    // At the top level, those are served as the single uploaded file.
    assert_eq!(
        run("tool.unitypackage", &unitypackage, LIMITS).unwrap().0,
        Outcome::Raw
    );
    assert_eq!(
        run("broken.zip", b"PK\x03\x04 nope", LIMITS).unwrap().0,
        Outcome::Raw
    );
    assert_eq!(
        run("truncated.7z", &sevenz_of(FILES)[..40], LIMITS)
            .unwrap()
            .0,
        Outcome::Raw
    );
    assert_eq!(run("notes.txt", b"hello", LIMITS).unwrap().0, Outcome::Raw);
}

#[test]
fn nesting_depth_is_limited() {
    let mut data = zip_of(&[("core.txt", b"core")]);
    for _ in 0..3 {
        data = zip_of(&[("n.zip", &data)]);
    }
    let (_, files) = run(
        "p.zip",
        &data,
        Limits {
            max_depth: 2,
            ..LIMITS
        },
    )
    .unwrap();
    // Two levels are opened; the third is left as a file.
    assert_eq!(names(&files), ["n.zip/n.zip/n.zip"]);
    let (_, files) = run("p.zip", &data, LIMITS).unwrap();
    assert_eq!(names(&files), ["n.zip/n.zip/n.zip/core.txt"]);
}

#[test]
fn archive_bombs_are_refused() {
    let big = vec![0u8; 200_000];
    let bomb = tar_gz_of(&[("a", &big), ("b", &big), ("c", &big)]);
    assert!(bomb.len() < 10_000);
    let err = run(
        "bomb.tar.gz",
        &bomb,
        Limits {
            max_unpacked_bytes: 500_000,
            ..LIMITS
        },
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("limit"), "{err:#}");
    // Nested, too: an error rather than a silent fallback.
    let nested = zip_of(&[("inner.tar.gz", &bomb)]);
    assert!(
        run(
            "p.zip",
            &nested,
            Limits {
                max_unpacked_bytes: 500_000,
                ..LIMITS
            }
        )
        .is_err()
    );
    assert!(run("p.zip", &nested, LIMITS).is_ok());
}
