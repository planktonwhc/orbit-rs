# orbit-rs

Capture the live **1080p H.264** stream from **DJI Goggles 3 / N3 / 2** over USB
and hand it to a renderer — a small capture + supervisor process for a
Raspberry Pi FPV appliance. Rust, `libc`-only, Linux.

Two capture modes, picked by `ORBIT_MODE` (see [Modes](#modes)): **`aoa`**
(default) — the board pretends to be the Android Open Accessory the goggles
expect, replays the DUML keep-alive commands, unwraps the `55 CC` framing —
and **`goggles-net`** — the goggles' "Liveview sharing" RNDIS/UDP link on a
regular USB port instead. Both feed the H.264 elementary stream to
**orbit-kms** (spawned as a child) or to a file / stdout the same way.

It mirrors the `dji-rs` → `dji-kms` split of the reference appliance: one
process owns the capture *and* starts the renderer, so the renderer inherits
the environment and a single config file configures the whole chain. `dji-rs`
picks its own mode the same way too — a plain config key (`op_mode`), not a
spawned process — which is what `ORBIT_MODE` mirrors here.

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

## Install (on the appliance) — [`install.sh`](install.sh)

```sh
sudo apt install -y cargo          # or rustup
./install.sh
```

Builds `orbit-rs` and `orbit-net`, installs both to `/usr/local/bin`, seeds
`/data/orbit/config/{orbit-rs.env,network.conf}` from the packaged defaults
**without overwriting** files that already exist, installs the three systemd
units below, and starts everything. Safe to re-run after `git pull` — it
rebuilds and restarts either way.

| unit | does |
|---|---|
| `orbit-net.service` | applies `network.conf` (AP/client Wi-Fi) |
| `orbit-net.path` | re-applies it automatically whenever the file changes |
| `orbit-rs.service` | capture → spawns the renderer |

All three are `After=multi-user.target` (see [Boot ordering](#boot-ordering))
so they never sit on the boot critical path.

This installs the capture/network half only. The **renderer**
([orbit-kms](https://github.com/planktonwhc/orbit-kms)) and the **settings
panel** ([orbit-web](https://github.com/planktonwhc/orbit-web)) are separate
repos with their own `install.sh` — `orbit-rs` needs `orbit-kms` on the box to
actually show a picture. `orbit-web`'s Renderer tab includes the `ORBIT_MODE`
dropdown ([Modes](#modes)) alongside the `DJI_KMS_*` knobs — same file, same
save-and-restart action.

## Run

Must run as root (it mounts FunctionFS and writes configfs). For a one-off or
during development, run it directly instead of through `install.sh`:

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

## Modes

`ORBIT_MODE` picks which USB role this board takes. Both write the same
elementary stream to the same sink (file / stdout / spawned renderer) — this
only decides how the bytes get here:

| `ORBIT_MODE` | USB role | link | how the goggles reach it |
|---|---|---|---|
| `aoa` *(default)* | this board is the USB **device** (emulated AOA accessory) | USB-C gadget/OTG port | `src/capture.rs` + `src/gadget.rs`: FunctionFS gadget, DUML keep-alives, `55 CC` framing |
| `goggles-net` | this board is the USB **host** (plain network client) | regular (non-OTG) USB port | `src/goggles_net/`: goggles enumerate as an RNDIS network device (`idVendor:idProduct` `2ca3:0020`), video arrives over UDP |

They're mutually exclusive on typical single-OTG-port hardware — the same
USB controller can't be host and device at once — so this is a config choice
the operator makes to match how the cable is actually plugged in, not
something `orbit-rs` probes for itself. (`dji-rs`'s own `auto` mode only
disambiguates *within* its AOA transport — goggles cable vs. RC-N aerial,
both USB-device-role links where sniffing the wire is possible; it doesn't
span this AOA-vs-goggles-net boundary either, for the same physical reason.)

`goggles-net` mode:

1. Waits for (or takes `--iface`/`GOGGLES_NET_IFACE` as) the RNDIS interface,
   brings it up (`ip`), and tells NetworkManager to leave it alone if present.
2. Opens a UDP session to the goggles (`--peer`/`GOGGLES_NET_PEER`, default
   `192.168.60.2:9003`): a 48-byte opener resent every 250ms until ACK'd.
3. ACKs every upstream control packet (the actual keepalive — without it the
   goggles stall) and reassembles video fragments into Annex-B units.
4. Watches two liveness clocks: video quiet for 2.5s *while control traffic
   is still alive* is logged once as a hint (almost always "no signal from
   the air unit yet", not a dead link — matching the same
   HANDSHAKE-STALL/FEED-STALL distinction `dji-rs` itself makes); nothing at
   all — not even control traffic — for 10s ends the session so the service
   supervisor restarts fresh.

Needs root either way (this mode for `ip`/`nmcli` and interface bring-up, the
same as `aoa` mode needs it for FunctionFS/configfs).

## Configuration

### Environment read by orbit-rs

| var | meaning |
|---|---|
| `ORBIT_MODE` | `aoa` (default) or `goggles-net` — see [Modes](#modes). |
| `ORBIT_RENDERER` | renderer command, whitespace-split. `none` / `off` / empty → just write the default file. Unset → `/usr/local/bin/orbit-kms` when `/dev/dri/card0` or `card1` exists, else the default file. |
| `ORBIT_CODEC` / `CODEC` | `h264` (default) or `h265` / `hevc` → passed to an auto-picked `orbit-kms` as `--h264` / `--h265`. |
| `ORBIT_ENV_FILE` | path to the `KEY=value` config file. `none` to skip loading. |
| `GOGGLES_NET_IFACE` | `goggles-net` mode only: RNDIS interface name. Unset → auto-detect the `2ca3:0020` USB parent. Same as `--iface`. |
| `GOGGLES_NET_PEER` | `goggles-net` mode only: the goggles' UDP endpoint. Default `192.168.60.2:9003`. Same as `--peer`. |

All of these are read from the process environment, so `config/orbit-rs.env`
(below) sets them too — that's exactly what `orbit-web`'s Mode dropdown
writes to. `envcfg::load()` runs before any of them are read, specifically so
a value that only exists in the file (never a real shell/systemd env var)
still takes effect.

### Config file — [`config/orbit-rs.env`](config/orbit-rs.env)

Plain `KEY=value` (systemd `EnvironmentFile` compatible). Loaded into the
environment at startup; **variables already set are never overwritten**. Search
order when `ORBIT_ENV_FILE` is unset:

```
/data/orbit/config/orbit.env          the appliance's real, live config
/data/orbit/config/orbit-rs.env       (what orbit-web writes to)
./config/orbit.env                    dev convenience: repo checkout as CWD
./config/orbit-rs.env
<exe dir>/config/orbit.env            (and orbit-rs.env)
<exe dir>/../config/orbit.env         (and orbit-rs.env)
/etc/orbit-rs/orbit.env               (and orbit-rs.env)
```

The packaged file's own annotated contents are all `DJI_KMS_*` — the
renderer's knobs (connector, decoder, vsync, flip, crop, lens dewarp, colour
grade), plus the **waiting screen** (`DJI_KMS_STANDBY` = a still image
(`.png`/`.jpg`) or a looping clip (`.mp4`/`.mov`/`.mkv` or a raw `.h264`
stream), shown whenever there is no live feed). But it's the same generic
`KEY=value` file `envcfg::load()` folds wholesale into the environment, so
`orbit-rs`'s own vars (`ORBIT_MODE`, `ORBIT_RENDERER`, `ORBIT_CODEC`,
`GOGGLES_NET_IFACE`/`_PEER` — see [Environment read by
orbit-rs](#environment-read-by-orbit-rs)) belong in it too; they're just not
part of the packaged example since that one only demonstrates the renderer
knobs. See the file itself for the full annotated `DJI_KMS_*` list.

## Network — [`scripts/orbit-net`](scripts/orbit-net)

The appliance's Wi-Fi is driven by [`config/network.conf`](config/network.conf)
(installed at `/data/orbit/config/network.conf`) through NetworkManager:

| `MODE` | what happens |
|---|---|
| `ap` *(default)* | the Pi is a hotspot (`AP_SSID` / `AP_PASSWORD` / `AP_CHANNEL` / `AP_IP`). Connect a phone to it to reach the appliance and edit `orbit-rs.env`. NetworkManager runs DHCP; clients also get internet if the Pi has it on `eth0`. |
| `client` | the Pi joins `CLIENT_SSID` as a station — for firmware-update checks and other internet needs. If the join fails, it falls back to `ap` so the box stays reachable. |

```sh
sudo install -m755 scripts/orbit-net          /usr/local/bin/orbit-net
sudo install -m644 systemd/orbit-net.service   /etc/systemd/system/orbit-net.service
sudo install -m644 systemd/orbit-net.path      /etc/systemd/system/orbit-net.path
sudo install -m644 systemd/orbit-rs.service    /etc/systemd/system/orbit-rs.service
sudo systemctl daemon-reload
sudo systemctl enable --now orbit-net orbit-net.path orbit-rs

# switch at runtime
sudo orbit-net client     # or: edit network.conf -> MODE=client, then `sudo orbit-net apply`
sudo orbit-net ap
sudo orbit-net status
```

`network.conf` is **not** watched by default — after editing it, run
`sudo orbit-net apply` (or `sudo systemctl reload orbit-net`). Enabling
`orbit-net.path` (above) makes systemd re-apply automatically on every save;
either way, a changed SSID/password rebuilds the NetworkManager profile and
drops connected clients until they reconnect.

`config/orbit-rs.env` is read once at `orbit-rs` startup — after editing it,
`sudo systemctl restart orbit-rs` (the standby image covers the blip).

### Boot ordering

Both units are `After=multi-user.target` + `WantedBy=multi-user.target`, i.e.
they start **after** the system has finished booting and never appear in
`systemd-analyze critical-chain`. The AP profile is `autoconnect=yes`, so
NetworkManager raises the link itself during its own startup (already on the
boot path); `orbit-net.service` is then just a fast reconcile of
`network.conf`. `orbit-rs.service` waits on `orbit-net.service`; the renderer's
standby image covers the first seconds until the goggles feed arrives.

Nothing here is ordered against `network-online.target` — `orbit-net`
configures NetworkManager, it does not wait for connectivity.

Trim the rest of the boot (optional, appliance images):

```sh
sudo systemctl disable --now NetworkManager-wait-online.service   # ~3 s, unused here
sudo systemctl disable --now apt-daily.timer apt-daily-upgrade.timer
sudo touch /etc/cloud/cloud-init.disabled                         # if cloud-init is present
sudo systemctl mask e2scrub_reap.service dpkg-db-backup.timer
```

## CLI

```
usage: orbit-rs [OUTPUT|stdout] [options]

  OUTPUT             file for the H.264 elementary stream; 'stdout' pipes it.
                     With no OUTPUT, orbit-rs spawns the renderer and feeds it.

  -h, --help         show help, then exit
  -V, --version      print the version, then exit

Mode (ORBIT_MODE=aoa|goggles-net, default aoa)
  aoa                USB-C gadget/cable link (default) -- this board emulates
                     the DJI AOA accessory; goggles are the USB host.
  goggles-net        regular USB port, "Liveview sharing" -- goggles enumerate
                     as an RNDIS network device; this board is a UDP client.

Controller (aoa mode)
  --udc NAME         device controller to bind (default: the board's only one)
  --list-udc         list the device controllers, then exit

Self-composed gadget (aoa mode, default)
  --gadget NAME      configfs gadget + FunctionFS instance name (default 'goggles')
  --mount PATH       where to mount FunctionFS (default /dev/ffs-<NAME>)
  --cleanup          remove a gadget left behind by a killed run, then exit

Attach to a gadget composed elsewhere (aoa mode)
  --ffs PATH         FunctionFS already mounted here; leave configfs alone
  --gadget-dir DIR   with --ffs: bind/unbind this configfs gadget around the run

RNDIS interface (goggles-net mode)
  --iface NAME       RNDIS interface (env GOGGLES_NET_IFACE; default: auto-detect
                     the USB parent with idVendor:idProduct 2ca3:0020)
  --peer IP:PORT     goggles' UDP endpoint (env GOGGLES_NET_PEER; default
                     192.168.60.2:9003)
  --ip ADDR/CIDR     our address on the link (default 192.168.60.1/24)
  --wait SECS        how long to wait for the interface to enumerate (default 30)

Session (goggles-net mode)
  --probe            stop after the opener ACK, don't read video
  --hex-rx           log every post-handshake datagram as hex on stderr
  --handshake-ms N   opener ACK timeout in milliseconds (default 5000)
  --dump PATH        also tee raw UDP payloads to a file (independent of the
                     reassembled OUTPUT -- for offline analysis)
```

## Layout

| file | role |
|---|---|
| [`src/main.rs`](src/main.rs)      | args, version, `ORBIT_MODE` dispatch, config load, renderer spawn, wiring |
| [`src/envcfg.rs`](src/envcfg.rs)  | load `config/orbit.env` into the environment |
| [`src/capture.rs`](src/capture.rs)| `aoa` mode: CRCs, DUML commands, EP0 events, the per-plug-in session loop |
| [`src/gadget.rs`](src/gadget.rs)  | `aoa` mode: configfs / FunctionFS AOA gadget lifecycle |
| [`src/goggles_net/`](src/goggles_net/mod.rs) | `goggles-net` mode: RNDIS interface bring-up (`iface.rs`), opener/ACK handshake (`handshake.rs`), video reassembly (`video.rs`), the receive + stall-watchdog loop (`capture.rs`) — ported from the standalone [`goggles-net`](../goggles-net) project |
| [`src/common.rs`](src/common.rs)  | logging, `Worker` threads, `Fd`, `55 CC` framing, `write_all` |

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
- `goggles_net::capture`'s stall watchdog tracks two independent clocks on
  purpose, not one: video quiet while control traffic is still alive is a
  weak, non-fatal signal (almost always just no air-unit picture yet); total
  silence — control traffic included — is the strong, fatal one (the link
  itself is gone). Collapsing these into a single "no video" timeout would
  either false-positive on every ordinary "aircraft not powered on yet" wait,
  or make a truly dead link take as long to notice as the weak signal allows.
