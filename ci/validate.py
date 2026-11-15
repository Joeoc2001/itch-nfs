#!/usr/bin/env python3
"""Validate a mounted itch-nfs export.

Walks the mount the way a build would and checks the promises the server
makes: a pack is one directory holding every version, every file reads back
in full, every file the pack's listing points at is there, and nothing is
writable. With --archive-dir the export is also checked against the archive's
own listings: every file of every version is served exactly once, at its own
path, or tagged (`name.v<version no>.ext`) if an earlier version has other
content there, or not at all if an earlier version has the same file.

By default pack names are not printed, because CI logs may be public and the
library belongs to whoever owns the token. Pass --names to print them.
"""

import argparse
import errno
import hashlib
import json
import os
import re
import sys
import zlib

DEDUP_FILE = ".itch-nfs-deduplicated.tsv"
ARCHIVE_SUFFIXES = (".zip", ".7z", ".rar", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tbz", ".tar.xz", ".txz", ".tar.zst", ".tzst")

failures = []
warnings = []


def fail(message):
    failures.append(message)
    print(f"::error::{message}")


def warn(message):
    warnings.append(message)
    print(f"::warning::{message}")


def files_under(root):
    for base, dirs, names in os.walk(root, onerror=lambda e: fail(f"cannot list {os.path.relpath(e.filename, root)}: {e.strerror}")):
        dirs.sort()
        for name in sorted(names):
            yield os.path.join(base, name)


def digest(path):
    """(sha256, size, CRC-32) of a file, reading all of it."""
    h, size, crc = hashlib.sha256(), 0, 0
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
            crc = zlib.crc32(chunk, crc)
            size += len(chunk)
    return h.hexdigest(), size, crc


def tagged(name, version_no):
    """The names a file called `name` may be served under when version
    `version_no` finds the path taken: the tag goes before the type suffix."""
    lower = name.lower()
    suffix = max((s for s in ARCHIVE_SUFFIXES if lower.endswith(s) and len(s) < len(name)), key=len, default=None)
    if suffix:
        cut = len(name) - len(suffix)
    else:
        dot = name.rfind(".")
        cut = dot if dot > 0 else len(name)
    return re.compile(re.escape(name[:cut]) + rf"\.v{version_no}(-[1-9][0-9]*)?" + re.escape(name[cut:]) + "$")


def archived_versions(archive_dir, pack):
    """The committed versions of a pack, in number order, each with the
    entries of its listing: [(no, name, [(path, size, crc)])]."""
    versions = []
    pack_dir = os.path.join(archive_dir, "packs", pack)
    for version in os.listdir(pack_dir):
        try:
            with open(os.path.join(pack_dir, version, "version.json")) as f:
                meta = json.load(f)
        except (OSError, ValueError):
            continue  # pack.json, or a version that is not committed
        with open(os.path.join(pack_dir, version, "manifest.json")) as f:
            manifest = json.load(f)
        versions.append((meta["no"], meta["name"], [(e["path"], e["size"], e["crc"]) for e in manifest["entries"]]))
    return sorted(versions)


def check_against_archive(label, versions, served, rows):
    """Recompute, from the archive's listings alone, where every file of every
    version must be, and compare with what the export serves.

    `served`: path in the pack directory -> (size, crc) of what was read there.
    `rows`: (version name, path in upload) -> (served path, why), the listing.
    """
    seen = set()  # (file name, size, crc) of every file of the earlier versions
    taken = {}  # served path -> the version it came from
    for no, version, entries in versions:
        fresh, paths, dirs = set(), set(), set()
        for path, size, crc in entries:
            # Within one version, a path that repeats (as a file or as a
            # directory of an earlier entry), or that runs through a file,
            # cannot exist: the first entry wins.
            parts = path.split("/")
            above = ["/".join(parts[:i]) for i in range(1, len(parts))]
            if path in paths or path in dirs or any(d in paths for d in above):
                continue
            paths.add(path)
            dirs.update(above)
            key = (parts[-1], size, crc)
            row = rows.pop((version, path), None)
            if key in seen:
                # Identical to a file of an earlier version: not served again.
                if row is None or row[1] != "duplicate":
                    fail(f"{label}: {version}: {path} repeats a file of an earlier version, but is not listed as a duplicate")
                elif served.get(row[0]) != (size, crc):
                    fail(f"{label}: {version}: {path} is listed as a duplicate of {row[0]}, which is not the same file")
                elif taken.get(row[0]) in (None, version):
                    fail(f"{label}: {version}: {path} is listed as a duplicate of {row[0]}, which no earlier version brought")
                continue
            fresh.add(key)
            if row is None:
                at = path
            elif row[1] != "renamed":
                fail(f"{label}: {version}: {path} is new in this version, but is listed as {row[1]}")
                continue
            else:
                at = row[0]
                # Renamed only for a reason: at each level the name is its
                # own, or tagged with this version's number.
                theirs, ours = at.split("/"), parts
                if len(theirs) != len(ours) or not all(a == b or tagged(b, no).match(a) for a, b in zip(theirs, ours)) or at == path:
                    fail(f"{label}: {version}: {path} is served as {at}, which is not that path tagged with .v{no}")
                # ... and the plain path belongs to an earlier version.
                clash = next(("/".join(ours[:i]) for i in range(1, len(ours) + 1) if theirs[i - 1] != ours[i - 1]), path)
                if not any(p == clash or p.startswith(clash + "/") for p, v in taken.items() if v != version):
                    fail(f"{label}: {version}: {path} is served as {at}, but no earlier version has taken {clash}")
            if at in taken:
                fail(f"{label}: {version}: {path} is served at {at}, which is also {taken[at]}'s")
            elif served.get(at) != (size, crc):
                fail(f"{label}: {version}: {path} should be served at {at}" + (", which is missing" if at not in served else ", which has other content"))
            taken[at] = version
        seen |= fresh
    for (version, path), (at, why) in rows.items():
        fail(f"{label}: the listing has a row for {version}: {path}, which the archive does not have")
    for path in sorted(set(served) - set(taken)):
        fail(f"{label}: {path} is served, but is no file of any version in the archive")


def upload_sizes(archive_dir):
    """Pack directory name -> total size of its archived uploads, from the archive."""
    sizes = {}
    packs = os.path.join(archive_dir, "packs")
    try:
        names = os.listdir(packs)
    except OSError:
        return sizes
    for pack in names:
        total, versions = 0, 0
        try:
            for version in os.listdir(os.path.join(packs, pack)):
                try:
                    with open(os.path.join(packs, pack, version, "version.json")) as f:
                        total += json.load(f).get("size") or 0
                    versions += 1
                except (OSError, ValueError):
                    continue  # pack.json, or a version that is not committed
        except OSError:
            continue
        if versions:
            sizes[pack] = total
    return sizes


def choose_packs(args, packs):
    if args.pack:
        missing = [p for p in args.pack if p not in packs]
        if missing:
            fail(f"requested pack(s) not in the export: {len(missing)} of {len(args.pack)}" + (f" ({', '.join(missing)})" if args.names else ""))
        return [p for p in args.pack if p in packs]
    if args.all:
        return packs
    # Look at a handful and take the smallest.
    candidates = packs[: args.scan]
    sizes = upload_sizes(args.archive_dir) if args.archive_dir else {}
    candidates = [p for p in candidates if os.listdir(os.path.join(args.mount, p))]
    candidates.sort(key=lambda p: (sizes.get(p, float("inf")), p))
    return candidates[: args.count]


def validate_pack(args, pack, label):
    pack_dir = os.path.join(args.mount, pack)
    stats = {"versions": "?", "files": 0, "bytes": 0, "renamed": 0, "hidden": 0}
    if not os.listdir(pack_dir):
        fail(f"{label}: empty")
    served = {}  # path in the pack directory -> (size, crc)
    for path in files_under(pack_dir):
        rel = os.path.relpath(path, pack_dir)
        if os.path.islink(path):
            fail(f"{label}: {rel} is a symlink")
            continue
        try:
            _, size, crc = digest(path)
        except OSError as e:
            fail(f"{label}: cannot read {rel}: {e.strerror}")
            continue
        if size != os.stat(path).st_size:
            fail(f"{label}: {rel} reads {size} bytes but stats as {os.stat(path).st_size}")
        if rel == DEDUP_FILE:
            continue
        served[rel] = (size, crc)
        stats["files"] += 1
        stats["bytes"] += size
        if path.lower().endswith(ARCHIVE_SUFFIXES):
            (fail if args.strict_archives else warn)(f"{label}: {rel} is an archive that was not unpacked")

    # The pack's listing: every file not served at its own path, and where
    # it, or the earliest copy of it, is.
    rows = {}
    listing = os.path.join(pack_dir, DEDUP_FILE)
    if not os.path.isfile(listing):
        fail(f"{label}: no {DEDUP_FILE}")
    else:
        with open(listing, encoding="utf-8") as f:
            for line in f:
                if line.startswith("#") or not line.strip():
                    continue
                fields = line.rstrip("\n").split("\t")
                if len(fields) != 4 or fields[3] not in ("renamed", "duplicate"):
                    fail(f"{label}: malformed row in {DEDUP_FILE}: {line.rstrip()!r}")
                    continue
                version, path, at, why = fields
                if (version, path) in rows:
                    fail(f"{label}: {DEDUP_FILE} lists {version}: {path} twice")
                rows[(version, path)] = (at, why)
                stats["renamed" if why == "renamed" else "hidden"] += 1
                if at not in served:
                    fail(f"{label}: {version}: {path} is listed as served at {at}, which is not a file of the export")
                if why == "renamed" and at == path:
                    fail(f"{label}: {version}: {path} is listed as renamed to itself")

    if args.archive_dir:
        try:
            versions = archived_versions(args.archive_dir, pack)
        except (OSError, ValueError, KeyError) as e:
            fail(f"{label}: cannot read the pack's listings in the archive: {e}")
        else:
            stats["versions"] = len(versions)
            check_against_archive(label, versions, served, rows)

    try:
        with open(os.path.join(pack_dir, "itch-nfs-write-test"), "w"):
            pass
        fail(f"{label}: the export is writable")
    except OSError as e:
        if e.errno not in (errno.EROFS, errno.EACCES, errno.EPERM):
            fail(f"{label}: writing failed with {e.strerror} rather than read-only")
    return stats


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mount")
    ap.add_argument("--archive-dir", help="the archive being served, used to pick the smallest packs and to check that the export has every archived pack")
    ap.add_argument("--pack", action="append", help="validate this pack directory (repeatable)")
    ap.add_argument("--all", action="store_true", help="validate every pack")
    ap.add_argument("--count", type=int, default=1, help="how many packs to validate otherwise")
    ap.add_argument("--scan", type=int, default=25, help="how many packs to consider when choosing")
    ap.add_argument("--expect", help="file listing every path the export must contain, exactly")
    ap.add_argument("--strict-archives", action="store_true", help="fail if any archive is left unpacked")
    ap.add_argument("--names", action="store_true", help="print pack names")
    args = ap.parse_args()

    packs = sorted(os.listdir(args.mount))
    print(f"{len(packs)} pack(s) in the export")
    if not packs:
        fail("the export is empty: has the scanner archived anything?")
    if args.archive_dir:
        # Every pack with a committed version is exported, and nothing else is.
        archived = sorted(upload_sizes(args.archive_dir))
        if archived != packs:
            fail(f"the export has {len(packs)} pack(s) but the archive has {len(archived)} with committed versions" + (f": {sorted(set(archived) ^ set(packs))}" if args.names else ""))
    try:
        os.mkdir(os.path.join(args.mount, "itch-nfs-write-test"))
        fail("the export root is writable")
    except OSError:
        pass

    chosen = choose_packs(args, packs)
    if packs and not chosen:
        fail("no pack with any uploads to validate")
    for i, pack in enumerate(chosen, 1):
        label = pack if args.names else f"pack #{i}"
        stats = validate_pack(args, pack, label)
        print(f"{label}: {stats['versions']} version(s), {stats['files']} file(s), {stats['bytes']} bytes read, {stats['renamed']} renamed, {stats['hidden']} de-duplicated")

    if args.expect:
        with open(args.expect) as f:
            expected = sorted(line.rstrip("\n") for line in f if line.strip())
        actual = sorted(os.path.relpath(p, args.mount) for p in files_under(args.mount))
        for path in sorted(set(expected) - set(actual)):
            fail(f"missing from the export: {path}")
        for path in sorted(set(actual) - set(expected)):
            fail(f"unexpected in the export: {path}")

    summary = f"{len(failures)} failure(s), {len(warnings)} warning(s)"
    print(summary)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(f"### itch-nfs mount validation\n\n{len(packs)} pack(s) exported. {summary}.\n")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
