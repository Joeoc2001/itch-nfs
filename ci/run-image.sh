#!/usr/bin/env bash
# Stand-in for the itch-nfs binary that runs the Docker image instead, so that
# ci/mount-and-validate.sh can validate the image (ITCH_NFS_BIN=ci/run-image.sh).
# Host networking keeps the listen address and the mock API URL meaningful.
# The archive and cache directories the script chose are mounted where the
# image expects them; only the scanner gets the archive writable.
set -euo pipefail
: "${ITCH_NFS_ARCHIVE_DIR:?}" "${ITCH_NFS_CACHE_DIR:?}"
archive=rw
if [ "${1:-}" = serve ]; then archive=ro; fi
exec docker run --rm --network host -e ITCH_TOKEN -e ITCH_NFS_API_URL \
    -v "$ITCH_NFS_ARCHIVE_DIR:/archive:$archive" -v "$ITCH_NFS_CACHE_DIR:/cache" \
    "${ITCH_NFS_IMAGE:-itch-nfs:ci}" "$@"
