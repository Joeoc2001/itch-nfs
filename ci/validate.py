#!/usr/bin/env python3
"""Validate a mounted itch-nfs export.

Walks the mount the way a build would and checks the promises the server
makes: packs contain pinned versions only, every file reads back in full,
later versions never repeat a file of an earlier one, and nothing is writable.

By default pack names are not printed, because CI logs may be public and the
library belongs to whoever owns the token. Pass --names to print them.
"""

import argparse
import errno
import hashlib
import json
import os
import sys

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
    for base, dirs, names in os.walk(root, onerror=lambda e: fail(f"cannot list {e.filename}: {e.strerror}")):
        dirs.sort()
        for name in sorted(names):
            yield os.path.join(base, name)


def digest(path):
    h, size = hashlib.sha256(), 0
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
            size += len(chunk)
    return h.hexdigest(), size


def upload_sizes(state_dir):
    """Pack directory name -> total size of its uploads, from the server's state."""
    try:
        with open(os.path.join(state_dir, "state.json")) as f:
            games = json.load(f)["games"].values()
    except (OSError, ValueError, KeyError):
        return {}
    return {g["name"]: sum(v.get("size") or 0 for v in g["versions"]) for g in games if g["versions"]}


def choose_packs(args, packs):
    if args.pack:
        missing = [p for p in args.pack if p not in packs]
        if missing:
            fail(f"requested pack(s) not in the export: {len(missing)} of {len(args.pack)}" + (f" ({', '.join(missing)})" if args.names else ""))
        return [p for p in args.pack if p in packs]
    if args.all:
        return packs
    # Listing a pack's versions costs one API call and no download, so look at
    # a handful and take the cheapest to download.
    candidates = packs[: args.scan]
    for p in candidates:
        os.listdir(os.path.join(args.mount, p))
    sizes = upload_sizes(args.state_dir) if args.state_dir else {}
    candidates = [p for p in candidates if os.listdir(os.path.join(args.mount, p))]
    candidates.sort(key=lambda p: (sizes.get(p, float("inf")), p))
    return candidates[: args.count]


def validate_pack(args, pack, label):
    pack_dir = os.path.join(args.mount, pack)
    versions = sorted(os.listdir(pack_dir))
    if not versions:
        fail(f"{label}: no versions")
    if any(v.lower() in ("latest", "current") for v in versions):
        fail(f"{label}: has an unpinned version directory")
    seen = {}  # (file name, sha256) -> the version it appears in
    stats = {"versions": len(versions), "files": 0, "bytes": 0, "hidden": 0}
    for version in versions:
        vdir = os.path.join(pack_dir, version)
        fresh = {}
        for path in files_under(vdir):
            rel = os.path.relpath(path, pack_dir)
            if os.path.islink(path):
                fail(f"{label}: {rel} is a symlink")
                continue
            try:
                sha, size = digest(path)
            except OSError as e:
                fail(f"{label}: cannot read {rel}: {e.strerror}")
                continue
            if size != os.stat(path).st_size:
                fail(f"{label}: {rel} reads {size} bytes but stats as {os.stat(path).st_size}")
            if os.path.basename(path) == DEDUP_FILE and os.path.dirname(path) == vdir:
                continue
            stats["files"] += 1
            stats["bytes"] += size
            key = (os.path.basename(path), sha)
            if key in seen:
                fail(f"{label}: {rel} is also in version {seen[key]}: only the earliest copy should be visible")
            fresh.setdefault(key, version)
            if path.lower().endswith(ARCHIVE_SUFFIXES):
                (fail if args.strict_archives else warn)(f"{label}: {rel} is an archive that was not unpacked")
        seen.update({k: v for k, v in fresh.items() if k not in seen})

        listing = os.path.join(vdir, DEDUP_FILE)
        if not os.path.isfile(listing):
            fail(f"{label}: {version} has no {DEDUP_FILE}")
            continue
        with open(listing, encoding="utf-8") as f:
            for line in f:
                if line.startswith("#") or not line.strip():
                    continue
                hidden, _, earliest = line.rstrip("\n").partition("\t")
                stats["hidden"] += 1
                if os.path.lexists(os.path.join(vdir, hidden)):
                    fail(f"{label}: {version}/{hidden} is listed as hidden but exists")
                if not os.path.isfile(os.path.join(pack_dir, earliest)):
                    fail(f"{label}: {version}/{hidden} points at {earliest}, which does not exist")

    try:
        with open(os.path.join(pack_dir, versions[0] if versions else "", "itch-nfs-write-test"), "w"):
            pass
        fail(f"{label}: the export is writable")
    except OSError as e:
        if e.errno not in (errno.EROFS, errno.EACCES, errno.EPERM):
            fail(f"{label}: writing failed with {e.strerror} rather than read-only")
    return stats


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mount")
    ap.add_argument("--state-dir", help="the server's --state-dir, used to pick the smallest packs")
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
        fail("the export is empty: is the token valid, and does the account own anything?")
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
        print(f"{label}: {stats['versions']} version(s), {stats['files']} file(s), {stats['bytes']} bytes read, {stats['hidden']} de-duplicated")

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
