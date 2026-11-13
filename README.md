# itch-nfs
A vibecoded virtual NFS to self-host itch.io asset packs

Lets you supply an oauth token for your account and then 'browse' decompressed versions of the asset packs you own.

I had claude build this because I was fed up with self-hosting git lfs and also keeping my asset packs up to date.

## What you get

A read-only NFSv3 export, laid out as

```
/<creator>.<pack>/<version>/<contents of the zip ...>
```

for example

```
/kenney.tiny-town/2024-01-05-tiny-town-v1-u123456/Tiles/tile_0001.png
/kenney.tiny-town/2024-06-01-tiny-town-v2-u234567/Tiles/tile_0200.png
```

* **Every version is its own directory, and there is no `latest`.** A version is one
  upload on the pack's itch.io page, named `<upload date>-<file name>-u<upload id>`.
  Names are assigned once and never change, so a path you pin keeps working.
* **Later versions only contain what is new.** A file that is identical (same file
  name, size and CRC-32, regardless of folder) to one in an earlier version of the
  same pack is hidden, so the only path that works is the earliest one. Each
  version has a `.itch-nfs-deduplicated.tsv` at its top listing what was hidden
  and where the earliest copy is.
* Zip uploads are exposed decompressed. Anything else (`.7z`, `.rar`, a lone
  `.png`, ...) is exposed as the single uploaded file. `__MACOSX` and `.DS_Store`
  entries are dropped.

itch.io keeps no history for an upload that a creator replaces or deletes, so
history starts when itch-nfs first sees a pack: it polls the packs you have
browsed and records each new upload as a new version.

## Running

```sh
cargo build --release
ITCH_TOKEN=... ./target/release/itch-nfs --state-dir /var/lib/itch-nfs
```

The token is an itch.io OAuth access token or an API key
(<https://itch.io/user/settings/api-keys>).

| Flag | Environment | Default | |
|---|---|---|---|
| `--listen` | `ITCH_NFS_LISTEN` | `0.0.0.0:11111` | NFS, MOUNT and portmap all share this TCP port |
| `--token` | `ITCH_TOKEN` | | itch.io token |
| `--token-file` | `ITCH_TOKEN_FILE` | | file containing the token |
| `--state-dir` | `ITCH_NFS_STATE_DIR` | `./itch-nfs-state` | version history and file listings. **Keep it** |
| `--archive-dir` | `ITCH_NFS_ARCHIVE_DIR` | `$TMPDIR/itch-nfs` | downloaded archives |
| `--cache-max-mb` | `ITCH_NFS_CACHE_MAX_MB` | `1024` | budget for decompressed assets in memory |
| `--cache-ttl` | `ITCH_NFS_CACHE_TTL` | `600` | seconds an unused asset stays in memory |
| `--refresh-interval` | `ITCH_NFS_REFRESH_INTERVAL` | `900` | seconds between polls of itch.io |

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

* **Archives** are downloaded on first use to `--archive-dir` and reused across
  restarts. Listing a pack's versions downloads nothing; opening a version
  downloads it and every earlier version of that pack once (their listings are
  needed for de-duplication, and are then kept in `--state-dir`). The directory
  has no size limit.
* **Decompressed assets** are cached in memory, dropped `--cache-ttl` seconds after
  their last read, and evicted at random when `--cache-max-mb` is exceeded. A
  single file larger than the whole budget is extracted to
  `<archive-dir>/spill` instead.

If a creator has replaced or deleted an upload *and* its archive is gone from
`--archive-dir`, that version still lists but its files fail with an I/O error.
Point `--archive-dir` at durable storage if you need old versions to outlive
their upload.

## Not supported

* Butler/wharf build history: only the build that is current when itch-nfs polls
  is recorded.
* Packs you can download without owning a key (free packs you never claimed).

## Development

`cargo test` runs the server against a mock itch.io API and a real NFSv3 client.
