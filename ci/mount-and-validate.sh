#!/usr/bin/env bash
# Archive with the scanner, serve the archive with the frontend, mount it with
# the kernel NFS client, and validate the mount.
#
#   ITCH_TOKEN=... ci/mount-and-validate.sh [--all | --pack NAME] [validate.py arguments...]
#
# With --all every pack is archived and validated; with --pack NAME only that
# one. With neither, `scan --list` is used to find the pack that is smallest to
# download, and only that one is archived and validated.
#
# Environment: ITCH_TOKEN (required), ITCH_NFS_API_URL (optional, for the mock),
# ITCH_NFS_BIN (default target/release/itch-nfs), SHOW_NAMES=1 to let pack
# names into the output, STOP_AFTER_SCAN (optional: a pid to kill once the
# scanner is done, e.g. the mock API's, to show that the frontend needs no API).
set -euo pipefail

: "${ITCH_TOKEN:?ITCH_TOKEN is not set}"
bin=${ITCH_NFS_BIN:-target/release/itch-nfs}
port=${ITCH_NFS_PORT:-11111}
here=$(dirname "$0")
command -v sudo >/dev/null || sudo() { "$@"; }
work=$(mktemp -d)
mnt=$work/mnt
log=$work/server.log
scanlog=$work/scan.log
pending=$work/pending.tsv
# The scanner and the frontend share the archive and nothing else.
export ITCH_NFS_ARCHIVE_DIR=$work/archive
mkdir -p "$mnt" "$ITCH_NFS_ARCHIVE_DIR" "$work/scan-cache" "$work/serve-cache"
# The Docker image runs as its own user.
chmod 755 "$work"
chmod 777 "$ITCH_NFS_ARCHIVE_DIR" "$work/scan-cache" "$work/serve-cache"
: >"$scanlog" >"$log"
server=

mode=choose
pack=
case "${1:-}" in
    --all) mode=all ;;
    --pack) mode=pack; pack=${2:?--pack needs a name} ;;
esac

# Replace every pack directory name the scanner came across, and every itch.io
# URL, with a placeholder.
redact() {
    python3 -c '
import os, re, sys
names = set()
try:
    names.update(os.listdir(os.path.join(sys.argv[1], "packs")))
except OSError:
    pass
try:
    with open(sys.argv[2]) as f:
        names.update(line.split("\t")[0] for line in f)
except OSError:
    pass
names = sorted((n for n in names if n), key=len, reverse=True)
pattern = re.compile("|".join([r"https?://\S+"] + [re.escape(n) for n in names]))
for line in sys.stdin:
    line = pattern.sub("<redacted>", line)
    # With no names known, drop the lines that carry them.
    if not names and re.search(r"new pack: |new version: |archived |downloading |could not list uploads|no such pack|too many versions", line):
        continue
    sys.stdout.write(line)
' "$ITCH_NFS_ARCHIVE_DIR" "$pending"
}

cleanup() {
    status=$?
    sudo umount -f "$mnt" 2>/dev/null || true
    [ -n "$server" ] && kill "$server" 2>/dev/null || true
    if [ "$status" -ne 0 ]; then
        # Pack names and URLs would list the token owner's library, so they
        # are redacted.
        for f in "$scanlog" "$log"; do
            echo "::group::$(basename "$f")"
            if [ "${SHOW_NAMES:-}" = 1 ]; then cat "$f"; else redact <"$f"; fi
            echo "::endgroup::"
        done
    fi
    exit "$status"
}
trap cleanup EXIT

scan() {
    ITCH_NFS_CACHE_DIR=$work/scan-cache "$bin" scan "$@" 2>>"$scanlog"
}

case $mode in
    all)
        scan
        ;;
    pack)
        scan --only "$pack"
        ;;
    choose)
        # Listing costs one API call per pack and downloads nothing.
        scan --list >"$pending"
        pack=$(python3 -c '
import sys
sizes = {}
for line in open(sys.argv[1]):
    name, _version, _file, size = line.rstrip("\n").split("\t")
    sizes[name] = sizes.get(name, 0) + (float("inf") if size == "-" else int(size))
if sizes:
    print(min(sizes, key=lambda n: (sizes[n], n)))
' "$pending")
        if [ -z "$pack" ]; then
            echo "::error::nothing to archive: is the token valid, and does the account own any asset packs?"
            exit 1
        fi
        scan --only "$pack"
        set -- --pack "$pack" "$@"
        ;;
esac
echo "scanned"

# From here on nothing needs the token or itch.io, and nothing may write to
# the archive.
if [ -n "${STOP_AFTER_SCAN:-}" ]; then
    kill "$STOP_AFTER_SCAN"
    echo "stopped the itch.io API"
fi
sudo chmod -R a-w "$ITCH_NFS_ARCHIVE_DIR"
find "$ITCH_NFS_ARCHIVE_DIR" -type f -exec sha256sum {} + | sort -k2 >"$work/archive.before"

ITCH_NFS_CACHE_DIR=$work/serve-cache env -u ITCH_TOKEN -u ITCH_TOKEN_FILE -u ITCH_NFS_API_URL \
    "$bin" serve --listen "127.0.0.1:$port" >"$log" 2>&1 &
server=$!

for _ in $(seq 50); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then break; fi
    kill -0 "$server" 2>/dev/null || { echo "::error::the server exited on startup"; exit 1; }
    sleep 0.2
done

sudo mount -t nfs -o "ro,vers=3,tcp,nolock,port=$port,mountport=$port" 127.0.0.1:/ "$mnt"
echo "mounted"

timeout 45m python3 "$here/validate.py" "$mnt" --archive-dir "$ITCH_NFS_ARCHIVE_DIR" ${SHOW_NAMES:+--names} "$@"

find "$ITCH_NFS_ARCHIVE_DIR" -type f -exec sha256sum {} + | sort -k2 >"$work/archive.after"
if ! cmp -s "$work/archive.before" "$work/archive.after"; then
    echo "::error::serving changed the archive"
    exit 1
fi
echo "the archive is unchanged"
