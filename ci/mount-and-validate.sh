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

cleanup() {
    status=$?
    sudo umount -f "$mnt" 2>/dev/null || true
    kill "$server" 2>/dev/null || true
    if [ "$status" -ne 0 ]; then
        echo "::group::server log"
        # "new pack" lines would list the token owner's whole library.
        if [ "${SHOW_NAMES:-}" = 1 ]; then cat "$log"; else grep -v "new pack: " "$log" || true; fi
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
