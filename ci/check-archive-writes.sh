#!/usr/bin/env bash
# Check, system call by system call, how the scanner writes to the archive:
# only files that did not exist, each opened for writing once and written
# front to back, with no rename, link, seek, positioned write or truncation
# anywhere under the archive directory. That is what an object store mount
# (mountpoint-s3 and the like) allows.
#
#   ITCH_TOKEN=... ITCH_NFS_API_URL=... ci/check-archive-writes.sh
#
# Runs two passes against whatever the API serves; needs strace.
set -euo pipefail

: "${ITCH_TOKEN:?ITCH_TOKEN is not set}"
bin=${ITCH_NFS_BIN:-target/release/itch-nfs}
work=$(mktemp -d)
export ITCH_NFS_ARCHIVE_DIR=$work/archive ITCH_NFS_CACHE_DIR=$work/cache
mkdir -p "$ITCH_NFS_ARCHIVE_DIR" "$ITCH_NFS_CACHE_DIR"

calls=openat,open,creat,rename,renameat,renameat2,link,linkat,symlink,symlinkat,lseek,pwrite64,pwritev,pwritev2,truncate,ftruncate,fallocate,copy_file_range,unlink,unlinkat
for pass in 1 2; do
    strace -f -y -qq -e "trace=$calls" -o "$work/trace.$pass" "$bin" scan 2>"$work/scan.$pass.log" \
        || { cat "$work/scan.$pass.log"; exit 1; }
done
python3 "$(dirname "$0")/check-archive-writes.py" "$ITCH_NFS_ARCHIVE_DIR" "$work/trace.1" "$work/trace.2"
