#!/usr/bin/env bash
# Start the server, mount it with the kernel NFS client, and validate the mount.
#
#   ITCH_TOKEN=... ci/mount-and-validate.sh [validate.py arguments...]
#
# Environment: ITCH_TOKEN (required), ITCH_NFS_API_URL (optional, for the mock),
# ITCH_NFS_BIN (default target/release/itch-nfs), SHOW_NAMES=1 to let pack
# names into the output.
set -euo pipefail

: "${ITCH_TOKEN:?ITCH_TOKEN is not set}"
bin=${ITCH_NFS_BIN:-target/release/itch-nfs}
port=${ITCH_NFS_PORT:-11111}
work=$(mktemp -d)
mnt=$work/mnt
log=$work/server.log
mkdir -p "$mnt"

"$bin" --listen "127.0.0.1:$port" --state-dir "$work/state" --archive-dir "$work/archives" >"$log" 2>&1 &
server=$!

# Replace every pack directory name the server recorded, and every itch.io
# URL, with a placeholder.
redact() {
    python3 -c '
import json, re, sys
try:
    with open(sys.argv[1]) as f:
        names = [g["name"] for g in json.load(f)["games"].values()]
except (OSError, ValueError, KeyError):
    names = []
names.sort(key=len, reverse=True)
pattern = re.compile("|".join([r"https?://\S+"] + [re.escape(n) for n in names if n]))
for line in sys.stdin:
    line = pattern.sub("<redacted>", line)
    # Without state.json the names are unknown: drop the lines that carry them.
    if not names and re.search(r"new pack: |new version: |could not list uploads of |has too many versions", line):
        continue
    sys.stdout.write(line)
' "$work/state/state.json"
}

cleanup() {
    status=$?
    sudo umount -f "$mnt" 2>/dev/null || true
    kill "$server" 2>/dev/null || true
    if [ "$status" -ne 0 ]; then
        echo "::group::server log"
        # Pack names and URLs (in "new pack", "new version" and warning lines)
        # would list the token owner's library, so they are redacted.
        if [ "${SHOW_NAMES:-}" = 1 ]; then cat "$log"; else redact <"$log"; fi
        echo "::endgroup::"
    fi
    exit "$status"
}
trap cleanup EXIT

for _ in $(seq 50); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then break; fi
    kill -0 "$server" 2>/dev/null || { echo "::error::the server exited on startup"; exit 1; }
    sleep 0.2
done

sudo mount -t nfs -o "ro,vers=3,tcp,nolock,port=$port,mountport=$port" 127.0.0.1:/ "$mnt"
echo "mounted"

timeout 45m python3 "$(dirname "$0")/validate.py" "$mnt" --state-dir "$work/state" ${SHOW_NAMES:+--names} "$@"
