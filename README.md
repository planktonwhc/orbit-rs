# orbit-rs

Capture the live **1080p H.264** stream from **DJI Goggles 3 / N3 / 2** over USB
and hand it to a renderer — a small capture + supervisor process for a
Raspberry Pi FPV appliance. Rust, `libc`-only, Linux.

The board pretends to be the Android Open Accessory the goggles expect, replays
the DUML keep-alive commands, unwraps the `55 CC` framing, and feeds the H.264
elementary stream (channel `0x4A`) to **orbit-kms** (spawned as a child) or to a
file / stdout.

It mirrors the `dji-rs` → `dji-kms` split of the reference appliance: one
process owns the gadget capture *and* starts the renderer, so the renderer
inherits the environment and a single config file configures the whole chain.

## Build

```sh
cargo build --release
# -> target/release/orbit-rs
```

Linux only (`mount`, `configfs`/`FunctionFS`, `pthread` via FFI). Cross-compile
for a Pi:

```sh
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

## Run

Must run as root (it mounts FunctionFS and writes configfs).

```sh
sudo ./target/release/orbit-rs
```

That is the whole command: `orbit-rs` folds `config/orbit-rs.env` into the
environment, spawns the renderer, and pipes the stream into its stdin. No
manual pipe, no `sudo -E`.

| invocation | what it does |
|---|---|
| `orbit-rs` | capture → spawn renderer, feed its stdin |
| `orbit-rs stdout` | write the raw stream to stdout (manual pipe / debugging) |
| `orbit-rs /tmp/feed.h264` | write the raw stream to a file |
| `ORBIT_RENDERER=none orbit-rs` | write the default file, spawn nothing |
| `ORBIT_RENDERER="/usr/local/bin/orbit-kms --h265" orbit-rs` | custom renderer command |

When the renderer exits (or crashes), `orbit-rs` exits too — leave it under a
respawn loop (`startup.sh` / a BusyBox init service).

## Configuration

### Environment read by orbit-rs

| var | meaning |
|---|---|
| `ORBIT_RENDERER` | renderer command, whitespace-split. `none` / `off` / empty → just write the default file. Unset → `/usr/local/bin/orbit-kms` when `/dev/dri/card0` or `card1` exists, else the default file. |
| `ORBIT_CODEC` / `CODEC` | `h264` (default) or `h265` / `hevc` → passed to an auto-picked `orbit-kms` as `--h264` / `--h265`. |
| `ORBIT_ENV_FILE` | path to the `KEY=value` config file. `none` to skip loading. |

### Config file — [`config/orbit-rs.env`](config/orbit-rs.env)

Plain `KEY=value` (systemd `EnvironmentFile` compatible). Loaded into the
environment at startup; **variables already set are never overwritten**. Search
order when `ORBIT_ENV_FILE` is unset:

```
./config/orbit.env
./config/orbit-rs.env
<exe dir>/config/orbit.env            (and orbit-rs.env)
<exe dir>/../config/orbit.env         (and orbit-rs.env)
/etc/orbit-rs/orbit.env               (and orbit-rs.env)
```

The file holds the renderer's `DJI_KMS_*` knobs (connector, decoder, vsync,
flip, crop, lens dewarp, colour grade). It also configures the **waiting
screen** — `DJI_KMS_STANDBY` = a still image (`.png`/`.jpg`) or a looping clip
(`.mp4`/`.mov`/`.mkv` or a raw `.h264` stream), shown whenever there is no live
feed. See the file itself for the full annotated list.

## CLI

```
usage: orbit-rs [OUTPUT|stdout] [options]

  OUTPUT             file for the H.264 elementary stream; 'stdout' pipes it.
                     With no OUTPUT, orbit-rs spawns the renderer and feeds it.

  -h, --help         show help, then exit
  -V, --version      print the version, then exit

Controller
  --udc NAME         device controller to bind (default: the board's only one)
  --list-udc         list the device controllers, then exit

Self-composed gadget (default)
  --gadget NAME      configfs gadget + FunctionFS instance name (default 'goggles')
  --mount PATH       where to mount FunctionFS (default /dev/ffs-<NAME>)
  --cleanup          remove a gadget left behind by a killed run, then exit

Attach to a gadget composed elsewhere
  --ffs PATH         FunctionFS already mounted here; leave configfs alone
  --gadget-dir DIR   with --ffs: bind/unbind this configfs gadget around the run
```

## Layout

| file | role |
|---|---|
| [`src/main.rs`](src/main.rs)      | args, version, config load, renderer spawn, wiring |
| [`src/envcfg.rs`](src/envcfg.rs)  | load `config/orbit.env` into the environment |
| [`src/capture.rs`](src/capture.rs)| CRCs, DUML commands, EP0 events, the per-plug-in session loop |
| [`src/gadget.rs`](src/gadget.rs)  | configfs / FunctionFS AOA gadget lifecycle |
| [`src/common.rs`](src/common.rs)  | logging, `Worker` threads, `Fd`, `55 CC` framing |

### Notes

- `Worker`: a thread blocks `SIGINT`/`SIGTERM`; `stop()` sets a flag then loops
  `pthread_kill(tid, SIGUSR1)` until the thread has left its blocking endpoint
  transfer (`JoinHandleExt::as_pthread_t`).
- Signal handlers use `sigaction` with **no** `SA_RESTART`, so a signal makes
  the blocked syscall return `EINTR`.
- The renderer child inherits the process environment, so anything
  `config/orbit.env` sets (`DJI_KMS_*`) reaches it without a wrapper.
- FunctionFS descriptor / string blocks and the EP0 event struct are written and
  parsed as raw little-endian bytes — no `linux/usb/functionfs.h` at build time.
