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
o open/close live console for the selected server
i send console command
b show/hide the right side panel
Up/Down scroll console output when console is open
PageUp/PageDown scroll console output faster
End follow new console output again
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
v fetch recent Mojang versions
d delete selected server
q quit
```

The startup command is the normal terminal command you would run by hand.
`remiaft` parses it into Java path, memory flags, jar path, JVM args, and server
args, then keeps managing it without requiring `screen`.

## Runtime Model

The CLI is not the long-running runtime. Starting a server launches a per-server
background `remiaft supervise <id>` process. On Unix this supervisor creates a new
session before it starts Minecraft, so closing the TUI or pressing Ctrl-C only
exits the management interface. The Minecraft process keeps running.

The supervisor owns the Minecraft child process, writes logs, forwards queued
console commands, and restarts the server when `auto_restart` is enabled. The
next time `remiaft` opens, it reloads the saved config and reads runtime PID
files to show the existing server state. This removes the need to switch into
`screen` sessions.

## Config

The config file is created on first run:

```text
~/.config/remiaft/config.toml
```

Runtime files and logs are stored below the user's local data directory, usually:

```text
~/.local/share/remiaft/runtime
```

## Minecraft Versions

Vanilla version data is read from Mojang's public version manifest:

```text
https://piston-meta.mojang.com/mc/game/version_manifest_v2.json
```

Custom server jars such as Paper, Fabric, Forge, and modpack launchers are also
supported because each server entry points at an arbitrary jar path and argument
list.
