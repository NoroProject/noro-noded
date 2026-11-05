# noro-noded

The node daemon of the Noro hosting panel. It runs Minecraft servers in Docker
on one machine and takes its orders from the master over a single outgoing
WebSocket — so a node works from behind NAT, without any inbound port.

Documentation: **[https://noro.rest/docs](https://noro.rest/docs)**

One daemon serves as many servers as the machine holds: console, power, files,
build rollout, backups, schedules and SFTP all travel over that one socket.

## What it does

- **Containers.** Creates, starts, stops and rebuilds servers through the Docker
  API, with memory, CPU, pids and disk limits. Exactly one mount per container:
  the server's own directory.
- **Network isolation.** Every server gets a Docker network of its own and
  cannot reach the cloud metadata service, the host's ports or the other
  servers of the node; the internet and its game ports stay open. A proxy and
  the backends behind it are allowed to reach each other.
- **Install.** Fetches the core (Paper, Fabric, NeoForge, Forge, Vanilla),
  runs loader installers **inside a throwaway container of the target image** —
  never on the host, because an installer is arbitrary code from the internet —
  seeds `server.properties`, accepts the EULA, pulls the image.
- **Console.** Streams the log to the master in 50 ms batches and passes
  commands back into the container's stdin.
- **Files.** One path resolver for both entry points (the master's API and
  SFTP); a symlink planted through the file manager leads nowhere.
- **Build rollout.** Downloads the server side of a modpack by sha1 and removes
  stale files **only inside managed roots** — `world/`, `logs/` and the rest are
  never in scope.
- **Backups.** `save-off` → `save-all flush` → snapshot → `save-on`: a copy of a
  live world gives broken regions. Backups live in `<data_dir>/backups/<server>/`,
  outside the directory mounted into the container, so a plugin cannot delete or
  swap a restore point. They have their own limit, set by the master, apart from
  the server's disk quota; the limit counts what is really on disk.
  - **Incremental by default.** Files are cut into content-defined chunks
    (FastCDC, 256 KiB–4 MiB, ~1 MiB average), each stored once under its blake3
    hash and compressed with zstd (`chunks/`); a snapshot is a manifest of files
    and their chunks (`snapshots/<id>.snap`). A world that changed by a few
    percent adds a few percent. Files whose size and ctime did not move since
    the previous snapshot are not even read.
  - **Crash-safe.** The manifest is written last, after the chunks are synced,
    so a snapshot either exists whole or not at all. Deleting a snapshot
    removes its manifest and then sweeps every chunk no manifest refers to
    (mark-and-sweep, no reference counts to go stale); an unreadable manifest
    stops the sweep instead of guessing.
  - **Restore** is full or of a single file or directory, writes only regular
    files and directories, and never leaves the server directory. Downloads and
    uploads to S3 are a plain `.tar.gz` assembled on the fly — no temporary copy
    of the world on disk; S3 gets it as a streamed multipart upload.
  - **Rollback:** `[backups] incremental = false` in `noded.toml` returns to full
    `.tar.gz` archives. Snapshots already taken still restore and download, and
    old archives keep working either way.
- **Crash fingerprints.** When a server crashes or is killed for memory, the
  last 200 console lines and that run's `crash-reports/*.txt` are kept in
  `<data_dir>/crashes/<server>/` (the last five), before the automatic restart
  scrolls them away.
- **SFTP.** `russh` on port 2022. Authentication is decided by the master: the
  list of keys never leaves it, so a compromised node cannot enumerate them.

## Install

```sh
curl -fsSL https://<your-master>/api/nodes/install.sh | sudo bash -s -- \
  --token noronode_… --master https://<your-master>
```

Or as a container, with the Docker socket passed in:

```sh
docker run -d --name noro-noded --restart=always \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /var/lib/noro:/var/lib/noro \
  -e NORO_MASTER_URL=https://<your-master> \
  -e NORO_NODE_TOKEN=noronode_… \
  -e NORO_HOST_DATA_DIR=/var/lib/noro \
  -p 2022:2022 \
  noroproject/noded:latest
```

`NORO_HOST_DATA_DIR` is not optional in the container mode and it is the easiest
thing to get wrong: `dockerd` resolves bind paths **on the host**, so a path that
is only valid inside this container produces a silently empty mount.

## Operations

- `curl -s localhost:8444/status` on the node shows what the daemon itself
  knows: whether the master link is up and when it last heard from it, how
  many events are queued, how many console lines were dropped, and what each
  server is doing. Only loopback clients get an answer; the master gets the
  same report through `NodeInfo`.
- `disk_reserve_mb` in the config (2048 by default, `0` to turn it off) is the
  space on the data disk that uploads, unpacking, downloads and backups never
  take, so running servers always have room to save their worlds.
- **Disk quota.** When the data directory is on xfs or ext4 mounted with
  `prjquota`, the kernel holds each server's limit — the server's own process
  cannot write past it — and usage costs one system call. Otherwise the daemon
  walks the directory and refuses only its own writes (uploads, unpacking).
  `curl localhost:8444/status` shows which mode is on (`disk_quota`). To turn
  project quota on for an ext4 volume, unmount it, run
  `tune2fs -O quota,project <device>` and mount it with `prjquota` (in
  `/etc/fstab`); a root volume needs this done from a rescue system. On xfs,
  mounting with `prjquota` is enough. Set `project_quota = false` to stay on
  the walk.
- The daemon warns the master once when a server reaches 90% of its disk quota
  or the data disk reaches 90% of what the reserve leaves, and once more when it
  drops back under 85%.

## Network isolation

On by default for containers created or rebuilt by this version; older ones
keep the default bridge until the panel rebuilds them. Each server's network
is a bridge named `nr-<id>` with a /28 cut from `subnet_pool`, and two chains
hang off those names:

- `NORO-FWD`, jumped to first from `DOCKER-USER` (or `FORWARD` when Docker has
  no `DOCKER-USER`), refuses 169.254.0.0/16, the private ranges and the other
  servers — except DNS to the host's resolvers and the groups the master sends
  (a proxy with its backends and limbos, through published ports);
- `NORO-IN`, jumped to first from `INPUT` for `nr-+`, refuses everything the
  host itself listens on.

The master's own address and port (from `master_url`) are always let
through: the game agent and authlib-injector talk to it, and on a
single-machine install that address is the host itself.

The daemon re-applies them every minute and at start, before anything else,
so a reboot or `ufw reload` does not leave a gap. It needs `iptables` and
`iptables-restore` on the host and the host's network namespace — the
container mode above has neither, and keeps servers on the default bridge.

```toml
[network]
isolation = true               # false takes the rules down at start
subnet_pool = "10.209.0.0/16"  # must not overlap the machine's own networks
allow = ["10.0.0.5:3306"]      # what servers may still reach: ip, cidr, :port
```

## Security

The daemon holds `docker.sock`, which is root on the host. Nothing from the API
reaches `Binds`, `Privileged`, `CapAdd`, `NetworkMode: host`, `Devices` or
`SecurityOpt`; images come from an allowlist; the daemon touches only containers
named `noro-<uuid>`. Running Docker rootless or with `--userns-remap` is
recommended.

## Building

```sh
cargo build --release
cargo clippy --all-targets -- -D warnings
cargo test
```

The wire protocol lives in [noro-shared](https://github.com/NoroProject/noro-shared)
and is pulled from git. To build against a local copy of it, create
`.cargo/config.toml`:

```toml
[patch."https://github.com/NoroProject/noro-shared.git"]
schema = { path = "../noro-shared/crates/schema" }
```

It is deliberately not in `Cargo.toml`: there the path would have to exist in
every clone, and CI has no sibling checkout.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
