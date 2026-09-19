# noro-noded

The node daemon of the Noro hosting panel. It runs Minecraft servers in Docker
on one machine and takes its orders from the master over a single outgoing
WebSocket — so a node works from behind NAT, without any inbound port.

One daemon serves as many servers as the machine holds: console, power, files,
build rollout, backups, schedules and SFTP all travel over that one socket.

## What it does

- **Containers.** Creates, starts, stops and rebuilds servers through the Docker
  API, with memory, CPU, pids and disk limits. Exactly one mount per container:
  the server's own directory.
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
- **Backups.** `save-off` → `save-all flush` → tar → `save-on`: a tar of a live
  world gives broken regions.
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
