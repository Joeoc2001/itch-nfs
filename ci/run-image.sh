#!/usr/bin/env bash
# Stand-in for the itch-nfs binary that runs the Docker image instead, so that
# ci/mount-and-validate.sh can validate the image (ITCH_NFS_BIN=ci/run-image.sh).
# Host networking keeps the listen address and the mock API URL meaningful.
exec docker run --rm --network host -e ITCH_TOKEN -e ITCH_NFS_API_URL "${ITCH_NFS_IMAGE:-itch-nfs:ci}" "$@"
