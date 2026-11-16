# itch-nfs
A vibecoded virtual NFS to self-host itch.io asset packs

Give it an itch.io API key and it archives every asset pack you own, then lets you browse decompressed versions of them over NFS, without the server needing the key or itch.io at all.

I had claude build this because I was fed up with self-hosting git lfs and also keeping my asset packs up to date.

## What you get

An archive of every upload of every asset pack you own, and a read-only NFSv3
export of it, laid out as

```
/<creator>.<pack>/<contents of every version, merged ...>
```

for example

```
/kenney.tiny-town/Tiles/tile_0001.png        from the first version
/kenney.tiny-town/Tiles/tile_0001.v2.png     the second version changed it
/kenney.tiny-town/Tiles/tile_0200.png        new in the second version
/kenney.tiny-town/.itch-nfs-deduplicated.tsv
```

* **A pack is one directory, holding every version of it.** A version is one
  upload on the pack's itch.io page, and itch-nfs numbers a pack's versions
  from 1 in the order it archives them. Versions are merged in that order:
  directories are shared, and a file goes where it is in its upload.
* **A path that works keeps working, and keeps its content.** Nothing is ever
  renamed, replaced or removed when a version is added, so there is no
  `latest`: the plain path is always the *oldest* content.
* **A changed file is served next to the original.** If an earlier version
  already has other content at a path, the newer file gets its version number
  before its type suffix: `tile.v2.png`, `sfx.v2.tar.gz` (archive extensions
  are kept whole), and `LICENSE.v2` or `.gitignore.v2` for a name with no
  suffix. The same goes for a directory and a file of one name. If an upload
  really contains a `tile.v2.png`, the clashing file becomes `tile.v2-2.png`.
  "The current file" is therefore the one with the highest number.
* **An unchanged file is served once.** A file that is identical (same file
  name, size and CRC-32, regardless of folder) to one in an earlier version of
  the same pack is not served again, so the only path that works is the
  earliest one. A version re-zipped under another top-level folder
  (`tiny-v2/` after `tiny/`) therefore adds a folder holding only what
  changed.
* **Separate uploads of one pack are versions too**, so `Pack-PNG.zip` and
  `Pack-SVG.zip` on one page land in the same directory, and a `README.txt`
  in each makes the second `README.v2.txt`.
* **`.itch-nfs-deduplicated.tsv`**, at the top of every pack, lists every
  file that is not served at the path it has in its upload, tab-separated:
  the version (its name in the archive, `<upload date>-<file name>-u<upload
  id>`), the path in the upload, the path it is served at, and `renamed`
  (tagged as above) or `duplicate` (not served; the path is the earliest
  copy's).
* **Archives are unpacked, recursively.** Uploads in zip, 7z, rar, tar, tar.gz,
  tar.bz2, tar.xz or tar.zst form (and the short spellings: `.tgz`, `.tbz2`,
  `.tbz`, `.txz`, `.tzst`) are exposed as their contents. An archive
  *inside* an upload becomes a directory named after it, up to 8 levels deep:
  `Pack.zip` containing `sfx.tar.gz` shows up as `<pack>/sfx.tar.gz/boom.ogg`.
* Only files with an archive extension are opened, so archives in disguise
  (`.unitypackage`, `.jar`, `.love`, ...) stay whole. Anything that is not an
  archive, or cannot be opened (corrupt, password-protected), is exposed as the
  file itself. `__MACOSX` and `.DS_Store` entries are dropped.

itch.io keeps no history for an upload that a creator replaces or deletes, so
itch-nfs keeps its own: everything it has ever downloaded stays in its archive,
and stays served, whatever happens to it on itch.io. History starts with the
first scan.

## How it works

itch-nfs is two programs around one directory:

```
             itch.io
                |
   scan  -------+          the only part with the token,
     |                     the only writer to the archive
     v
 <archive dir>             durable: a disk, or a mount of an S3 bucket or similar
     |
     v
   serve  ---> NFS         no token, no access to itch.io, reads the archive only
```

* **The scanner** (`itch-nfs scan`) lists every pack the account owns and
  every upload of every pack, and archives each upload it has not archived
  yet: it downloads it, unpacks it, and writes the result to the archive
  directory. It runs once and exits (for cron, or a Kubernetes CronJob), or
  loops with `--interval`.
* **The frontend** (`itch-nfs serve`) exports whatever is in the archive
  directory. It finds newly archived versions by listing the archive every
  `--rescan-interval` seconds.

Nothing has to be browsed for it to be archived, and nothing that was archived
depends on itch.io still having it.

### The archive directory

```
<archive>/packs/<pack name>/
  pack.json                      the pack's number, itch.io id, title, URL
  <version name>/
    upload/<original file name>  the bytes itch.io served, untouched
    content.zip                  flattened zip, only when the upload is not already a plain zip
    manifest.json                the listing of the files the version exports
    version.json                 written last: the commit marker
```

It describes itself: the pack directory names are the exported names, the
listings say what each version holds, and `pack.json` and `version.json`
carry the numbers that NFS file handles and the `.v<n>` tags are made from. Losing every other directory loses nothing — paths, handles and contents
are the same after a restart on the archive alone.

`version.json` records the upload (id, build, file name, size, timestamp), how
the version is served (as the contents of a zip, or as one file), and the size
and SHA-256 of every file next to it. A version directory without
`version.json` is incomplete: the frontend ignores it and the scanner redoes
it on its next pass.

The archive is made for object storage:

* The scanner only creates files that do not exist yet and writes each from
  front to back. It never renames, seeks, appends or rewrites inside the
  archive (CI checks this system call by system call), so a mount that only
  supports sequential writes of new objects is enough. Everything is built in
  local scratch space first.
* The one exception is cleaning up after itself: if a scan is interrupted, the
  next one deletes the files of the unfinished version before writing them
  again. So the scanner's mount must allow deletes (`--allow-delete` for
  mountpoint-s3) or overwrites (`--allow-overwrite`).
* The frontend should get the archive read-only. Reads are assumed to be slow
  or to cost money, so it reads as little as it can: see [Caching](#caching).
* **Run exactly one scanner per archive.** File locks do not work on such
  mounts, so nothing enforces this. Two scanners would hand the same numbers
  to different packs and versions.
* Numbers are only given out from a complete view of the archive. If a
  `version.json` cannot be read (an I/O error, as opposed to the file being
  missing, empty or cut short), the scanner leaves that pack alone for the
  pass and reports it as a problem. If a `pack.json` cannot be read, no pack
  gets a new number in that pass; packs that already have theirs carry on.
  Both are retried on the next pass.

## Running

```sh
cargo build --release
# Archive everything (cron this, or add --interval 900 to keep it running):
ITCH_TOKEN=... ./target/release/itch-nfs scan --archive-dir /srv/itch-archive
# Serve it:
./target/release/itch-nfs serve --archive-dir /srv/itch-archive
```

The token is an itch.io API key (<https://itch.io/user/settings/api-keys>).
The first scan downloads the whole library; `scan --list` prints what a scan
would download (tab-separated pack, version, file name and size in bytes)
without downloading anything, and `scan --only <pack name>` restricts a scan
to the packs named.

By default only projects itch.io classifies as `assets` are archived, so that
the games an account owns are not downloaded too. Pass `--classification`
(repeatable: `assets`, `soundtrack`, `tool`, `game`, ...) to choose, or
`--classification any` for everything. A project itch.io gives no
classification for is always archived.

An upload that decompresses to more than `--max-unpacked-gb` is still
archived, but exported as the upload itself (one file) rather than unpacked;
`version.json` says why in `note`. That is decided when it is archived and
then fixed, like everything else about a version.

### Docker

Every merge to `main` that passes CI publishes `ghcr.io/joeoc2001/itch-nfs`
(`latest`, and `sha-<commit>` for pinning; linux/amd64). It is one image with
both programs; `serve` is the default command.

```yaml
services:
  scan:
    image: ghcr.io/joeoc2001/itch-nfs
    command: ["scan", "--interval", "900"]
    restart: unless-stopped
    environment:
      ITCH_TOKEN: "..."
    volumes:
      - archive:/archive
      - scan-cache:/cache
  serve:
    image: ghcr.io/joeoc2001/itch-nfs
    restart: unless-stopped
    ports:
      - "11111:11111"
    volumes:
      - archive:/archive:ro
      - serve-cache:/cache

volumes:
  archive:      # the one to keep; or bind-mount an object store mount here
  scan-cache:
  serve-cache:
```

`/archive` is `--archive-dir` and must be kept. `/cache` is `--cache-dir` and
can be thrown away at any time. Both programs run as uid 10001, so
bind-mounted host directories must be writable by that uid (the archive only
for the scanner). All flags below work as arguments or environment variables.

Both programs:

| Flag | Environment | Default | |
|---|---|---|---|
| `--archive-dir` | `ITCH_NFS_ARCHIVE_DIR` | `./itch-nfs-archive` | the archive. **Keep it** |
| `--cache-dir` | `ITCH_NFS_CACHE_DIR` | `$TMPDIR/itch-nfs` | local scratch space and caches. Disposable |

`itch-nfs scan`:

| Flag | Environment | Default | |
|---|---|---|---|
| `--token` | `ITCH_TOKEN` | | itch.io token |
| `--token-file` | `ITCH_TOKEN_FILE` | | file containing the token |
| `--interval` | `ITCH_NFS_SCAN_INTERVAL` | | keep running, with this many seconds between scans. Without it: scan once and exit, non-zero if anything failed |
| `--api-url` | `ITCH_NFS_API_URL` | `https://api.itch.io` | itch.io API base URL (the tests point it at a mock) |
| `--only` | `ITCH_NFS_ONLY` | | only scan this pack directory name (repeatable; comma-separated in the environment) |
| `--list` | | | print what would be archived, download nothing |
| `--classification` | `ITCH_NFS_CLASSIFICATION` | `assets` | itch.io classifications to archive (repeatable; comma-separated in the environment), or `any` |
| `--max-unpacked-gb` | `ITCH_NFS_MAX_UNPACKED_GB` | `100` | do not unpack uploads that decompress to more than this (archive bombs) |
| `--concurrency` | `ITCH_NFS_CONCURRENCY` | `2` | packs scanned and downloaded at once |
| `--api-delay-ms` | `ITCH_NFS_API_DELAY_MS` | `250` | minimum time between two requests to itch.io |

A scan makes one request per pack to list its uploads, so with the defaults a
library of 400 packs takes a little under two minutes to check when nothing
is new. An upload that fails (to download, to unpack, to be written) is logged
and tried again on the next scan; the other uploads of the scan are not
affected. Versions are numbered in the order they are archived.

`itch-nfs serve`:

| Flag | Environment | Default | |
|---|---|---|---|
| `--listen` | `ITCH_NFS_LISTEN` | `0.0.0.0:11111` | NFS, MOUNT and portmap all share this TCP port |
| `--rescan-interval` | `ITCH_NFS_RESCAN_INTERVAL` | `900` | seconds between listings of the archive for new versions |
| `--disk-cache-max-gb` | `ITCH_NFS_DISK_CACHE_MAX_GB` | `20` | budget for local copies of the archive's zips |
| `--cache-max-mb` | `ITCH_NFS_CACHE_MAX_MB` | `1024` | budget for decompressed assets in memory |
| `--cache-ttl` | `ITCH_NFS_CACHE_TTL` | `600` | seconds an unused asset stays in memory |

There is no authentication on the export: bind it to a private interface.

## Mounting

```sh
# Linux
mount -t nfs -o ro,vers=3,tcp,nolock,port=11111,mountport=11111 nfs-host:/ /mnt/itch
# macOS
mount -t nfs -o ro,vers=3,tcp,nolocks,port=11111,mountport=11111 nfs-host:/ /mnt/itch
```

For GitLab CI with the Docker executor, mount it on the runner host and add the
mount point to `volumes` in the runner's `config.toml`
(`volumes = ["/mnt/itch:/mnt/itch:ro"]`), or declare it as an NFS Docker volume.

## Caching

The frontend assumes that reading the archive is slow or costs money, and
keeps three things in `--cache-dir` and in memory so that it rarely has to:

* **Metadata** (`pack.json`, `version.json`, `manifest.json`) is immutable, so
  each file is read from the archive once and kept in `<cache-dir>/meta`.
  Looking for new versions then costs one directory listing per pack, every
  `--rescan-interval` seconds. Listing directories and looking at file sizes
  never reads a zip.
* **Zips.** The first time a file of a version is read, the file the version
  is served from (`content.zip`, or the upload itself) is copied whole, front
  to back, to `<cache-dir>/zips`, checked against the SHA-256 in
  `version.json`, and every read after that uses the local copy: reading a
  version twice reads its zip from the archive once. A copy that does not
  match its checksum is discarded and the read fails with an I/O error. The
  least recently used copies are deleted when `--disk-cache-max-gb` is
  exceeded, except a copy in use and the most recently used copy. A zip
  larger than the whole budget is therefore copied once and kept until a
  different zip is read, so the directory can hold one file more than the
  budget. Reading two such zips in turn copies each from the archive again
  every time the reads switch; a warning is logged whenever a file over the
  budget is copied, and the budget should be raised above the largest served
  file if it appears. A zip the frontend still holds open is closed when its
  copy is deleted, so the space is given back at once.
* **Decompressed assets** are cached in memory, dropped `--cache-ttl` seconds
  after their last read, and evicted at random when `--cache-max-mb` is
  exceeded. A single file larger than the whole budget is extracted to
  `<cache-dir>/spill` instead.

Opening a pack needs the listings of every version of that pack (to merge
and de-duplicate them), which is metadata only: one read of the archive per
version, once per cache directory. If a listing cannot be read, the whole
pack fails with an I/O error until it can, rather than being served with a
version missing.

The cache directory belongs to one archive: if you point either program at a
different archive, give it an empty cache directory.

The scanner uses `<cache-dir>/scan-tmp` to download and unpack (it needs room
for the largest upload, unpacked, `--concurrency` times over) and keeps the
same `meta` copy, so that finding out what is already archived does not read
the archive either. The copy is only a shortcut: one that is damaged (a power
loss can leave it empty) is discarded and the file is read from the archive
again. The two programs can share a cache directory or not.

## Not supported

* Butler/wharf build history: only the build that is current when the scanner
  runs is archived.
* Packs you can download without owning a key (free packs you never claimed).

## Development

`cargo test` runs the scanner against a mock itch.io API, and the frontend
against the archive that produces, with a real NFSv3 client.

CI (`.github/workflows/ci.yml`) additionally runs the release binary the way
a deployment would — `scan` once, then `serve` with no token and the archive
made read-only — mounts it with the Linux kernel NFS client and checks the
mount with `ci/validate.py`:

* **against the mock**, where the whole tree must match `ci/mock-expected.txt`
  (regenerate with `UPDATE_EXPECTED=1 cargo test`). The mock API is stopped
  before the frontend starts. `ci/check-archive-writes.sh` also runs the
  scanner under `strace` and fails on any rename, seek, positioned write,
  truncation or second write to a file in the archive;
* **against itch.io**, using the `ITCH_TOKEN` repository secret. It uses
  `scan --list` to find the pack that is the smallest download, archives it
  with `scan --only`, reads every file of the pack, and checks that every
  file of every version in the archive is served exactly once (at its own
  path, tagged, or not at all if an earlier version has the same file) and
  that nothing is writable. Set the
  repository variable `ITCH_NFS_CI_PACK` to a pack directory name to pin which
  pack is used. Pack names are kept out of the logs unless the variable
  `ITCH_NFS_CI_SHOW_NAMES` is `1`.

The Docker image is built on every run, and must archive and serve the mock
fixture pack through a kernel mount before it is pushed.

To run the same check by hand: `ITCH_TOKEN=... ci/mount-and-validate.sh`.
