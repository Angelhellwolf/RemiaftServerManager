# remiaft

`RemiaftServerManager` is a modern self-hosted Minecraft server manager for
Remiaft. It is designed to run as a normal user, without `screen`, `tmux`, or
root privileges.

## Goals

- `remiaft` opens an interactive terminal UI from any shell.
- Manage multiple server directories and jar paths.
- Configure Java memory, Java args, server args, auto restart, and console commands.
- Fetch Minecraft version metadata from Mojang's version manifest instead of
  hard-coding release numbers.
- Keep runtime state under the user's local data directory.
- Ship with GitHub CI and tagged release artifacts.

## One-Line Install

```sh
curl -fsSL https://raw.githubusercontent.com/Angelhellwolf/RemiaftServerManager/master/scripts/install-remote.sh | sh
```

The installer downloads the latest GitHub Release binary and writes it to:

```text
$HOME/.local/bin/remiaft
```

Make sure `$HOME/.local/bin` is on `PATH`.

## Install From Source

```sh
./scripts/install.sh
```

The local installer builds the release binary and writes it to:

```text
$HOME/.local/bin/remiaft
```

## Usage

```sh
remiaft
remiaft status
remiaft add survival --dir /srv/survival --jar server.jar
remiaft add room-1 --dir /srv/room-1 --docker --image eclipse-temurin:21-jre
remiaft docker room-1 --port 25565:25565 --volume /srv/shared/plugins:/home/remiaft/server/plugins:ro
remiaft start survival
remiaft stop survival
remiaft restart survival
remiaft versions --limit 20
```

On first launch, `remiaft` asks for the interface language and saves it in the
user config. English and Simplified Chinese are supported now; press `l` in the
TUI to change language later.

Inside the TUI:

```text
u edit the startup command, for example: java -Xms1G -Xmx4G -jar server.jar nogui
o attach a native console for the selected running server
type directly in the native console; Tab, arrows, and editing keys go to the server
Ctrl-C in the native console sends an interrupt to the selected server
Ctrl-U detaches from the native console without stopping the server
b show/hide the right side panel
use your terminal scrollback for attached console history
n add server
s start selected server
x stop selected server
r restart selected server
c send console command
a toggle auto-restart
e edit Java args
g edit Minecraft server args
p edit server directory
j edit jar path
d delete selected server/group
q quit
```

The TUI server creation flow asks for the runtime backend. Choose `native` for
the classic supervisor flow or `docker` for Docker-backed rooms. Docker creation
then asks for image, directory mapping, image entrypoint usage, and RCON mode.

The startup command is the normal terminal command you would run by hand.
`remiaft` parses it into Java path, memory flags, jar path, JVM args, and server
args, then keeps managing it without requiring `screen`.

## Runtime Model

The CLI is not the long-running runtime. Starting a server launches a per-server
background `remiaft supervise <id>` process. On Unix this supervisor starts the
server inside a PTY, so Minecraft/Velocity sees a real terminal and ANSI colors
are preserved in the console log. Pressing `o` temporarily leaves the TUI
alternate screen and attaches the current terminal to the server stream: input
bytes are forwarded to the server, and raw PTY output is written back to your
terminal for native JLine/Paper/Velocity completion behavior. Closing the TUI,
pressing Ctrl-C in the manager, or pressing Ctrl-U to detach from the native
console only exits the management interface. The Minecraft process keeps running.

The supervisor owns the Minecraft child process, writes raw terminal logs,
forwards queued console commands, and restarts the server when `auto_restart` is
enabled. The next time `remiaft` opens, it reloads the saved config and reads
runtime PID files to show the existing server state. This removes the need to
switch into `screen` sessions.

Docker-backed servers use Docker Engine API through the local Docker socket
instead of shelling out to the `docker` CLI. Remiaft labels every managed
container with `remiaft.manager=true` and `com.remiaft.server_id=<id>`, and
refuses to manage any container without that marker. Docker server commands are
sent through RCON. In automatic RCON mode, Remiaft generates a password, stores
it in `config.toml`, allocates a host port from `25575-25999`, and updates
`server.properties` when the server directory is bind-mounted.

## Config

The config file is created on first run:

```text
~/.config/remiaft/config.toml
```

Runtime files and logs are stored below the user's local data directory, usually:

```text
~/.local/share/remiaft/runtime
```

### Docker Servers

Set a server's runtime to Docker in `config.toml`:

```toml
[[servers]]
id = "survival-12345678"
name = "survival"
directory = "/home/remiaft/servers/survival"
jar_path = "server.jar"
min_memory_mb = 1024
max_memory_mb = 4096
java_args = []
server_args = ["nogui"]
auto_restart = false
restart_delay_secs = 10

[servers.runtime]
kind = "docker"

[servers.runtime.docker]
# Empty image means Remiaft chooses a Java image from the Minecraft version.
# For example, 1.21 defaults to eclipse-temurin:21-jre.
mount_server_directory = true
server_dir = "/home/remiaft/server"

[servers.runtime.docker.rcon]
mode = "auto"
port_range_start = 25575
port_range_end = 25999
```

For reusable room images such as Bed Wars rooms that do not need map
persistence, let the image start the server and disable the default bind mount:

```toml
[servers.runtime]
kind = "docker"

[servers.runtime.docker]
image = "your-registry/bedwars-room:latest"
mount_server_directory = false
use_image_entrypoint = true
auto_remove = true
```

Custom ports and volumes are explicit:

```toml
[[servers.runtime.docker.ports]]
name = "minecraft"
container_port = 25565
host_port = 25565
protocol = "tcp"

[[servers.runtime.docker.ports]]
name = "rcon"
container_port = 25575
host_port = 25575
host_ip = "127.0.0.1"
protocol = "tcp"

[[servers.runtime.docker.volumes]]
host = "/home/remiaft/shared/plugins"
container = "/home/remiaft/server/plugins"
read_only = false
```

When a bind mount is used, Remiaft runs the container as the host user's numeric
UID/GID by default. This keeps files created by Minecraft readable and writable
by the local user instead of root.

Remiaft refuses dangerous Docker settings before starting a container: host
paths such as `/`, `/etc`, `/root`, and `/var/run/docker.sock` cannot be
bind-mounted, privileged containers are not allowed, and host PID or host
network mode is rejected. Docker API calls are appended to
`~/.local/share/remiaft/runtime/docker-audit.log`.

The same Docker fields can be configured from the CLI:

```sh
remiaft add paper --dir /home/remiaft/servers/paper --jar server.jar \
  --docker --image eclipse-temurin:21-jre --rcon auto

remiaft docker paper --port 25565:25565
remiaft docker paper --volume /home/remiaft/shared/plugins:/home/remiaft/server/plugins:ro
remiaft docker paper --no-mount --use-image-entrypoint --image your-registry/bedwars-room:latest
```

## Minecraft Versions

Vanilla version data is read from Mojang's public version manifest:

```text
https://piston-meta.mojang.com/mc/game/version_manifest_v2.json
```

Custom server jars such as Paper, Fabric, Forge, and modpack launchers are also
supported because each server entry points at an arbitrary jar path and argument
list.

## Project Design

The runtime boundary is intentionally separate from the UI. `src/process.rs`
owns supervisor processes, PID files, command queues, PTY handling, and server
lifecycle. `src/tui.rs` owns TUI state and event dispatch, while `src/tui/`
contains focused modules for rendering, input editing, startup command parsing,
console log rendering, and terminal setup/cleanup.

Near-term development should continue reducing `src/tui.rs`: move tree
selection into `src/tui/tree.rs`, form submission into `src/tui/forms.rs`, and
console scroll/input state into `src/tui/console.rs`. Runtime work should focus
on incremental log reads, stronger supervisor tests, explicit config migrations,
and CI coverage for `cargo fmt --check`, `cargo clippy`, and `cargo test`.
