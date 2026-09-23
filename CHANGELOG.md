# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

The project is in git but has no release tags yet, so everything so far sits under
`[Unreleased]`. Once releases start, move entries under a version heading (e.g. `## [1.2.0]`).

## [Unreleased]

### Added

- **`goggles-net` capture mode** (`ORBIT_MODE=goggles-net`, alongside the existing default `aoa`
  mode) — captures the goggles' "Liveview sharing" link (regular USB port, RNDIS/UDP, this board
  as a plain network client) instead of the USB-C gadget/AOA cable link `aoa` mode speaks. The two
  modes are mutually exclusive USB roles on typical single-OTG-port hardware (host vs. device), so
  this is a config choice, not auto-detected — mirrors how the reference appliance (`dji-rs`) picks
  its own `op_mode` from a plain config key rather than juggling processes; see README's new
  [Modes](README.md#modes) section. New module `src/goggles_net/` (`iface.rs`, `handshake.rs`,
  `video.rs`, `capture.rs`), ported from the standalone `../goggles-net` project (left as-is, not
  removed). New CLI flags: `--iface`, `--peer`, `--ip`, `--wait`, `--probe`, `--hex-rx`,
  `--handshake-ms`, `--dump`; new env vars `GOGGLES_NET_IFACE`/`GOGGLES_NET_PEER`.
- **`goggles-net` mode's stall watchdog** — the standalone project this was ported from blocks on
  `sock.recv()` with no read timeout and no liveness tracking at all, so a fully dead link (goggles
  unplugged, Liveview sharing turned off) hangs forever with no log line. The ported version adds
  two independent liveness clocks: video quiet for 2.5s while control-ACK traffic is still flowing
  is logged once as a non-fatal hint (almost always "no signal from the air unit yet", the same
  HANDSHAKE-STALL/FEED-STALL distinction `dji-rs` itself makes — corroborated independently by
  `../goggles-net-mac/src/capture.rs`'s own FEED-STALL watchdog, though that one's bootstrap/reseed
  logic on top of it is macOS/`ffplay`-decoder-specific and was **not** carried over, since
  `orbit-kms`'s hardware decoder doesn't need it); total silence, control traffic included, for 10s
  is fatal and ends the session for the service supervisor to restart fresh.
- **`orbit-web`'s Renderer tab gained an `ORBIT_MODE` dropdown** (`aoa` / `goggles-net`) plus
  optional `GOGGLES_NET_IFACE`/`GOGGLES_NET_PEER` text fields — same `orbit-rs.env` file, same
  save-and-restart action as every other renderer knob; no backend change needed there, since
  `PUT /api/env` was already a generic `KEY=value` editor.

### Fixed

- **`ORBIT_MODE` (and `GOGGLES_NET_IFACE`/`GOGGLES_NET_PEER`) set only in `orbit-rs.env`, never as
  a real shell/systemd env var, was silently ignored.** `envcfg::load()` (which folds the config
  file into the process environment) ran *after* `ORBIT_MODE` was read and after
  `goggles_net::Opts::default()` had already captured `GOGGLES_NET_IFACE`/`_PEER` from the
  environment — both read a snapshot from before the file's values existed. `envcfg::load()` now
  runs first, before anything reads the environment. Caught before it ever shipped: while wiring up
  `orbit-web`'s new dropdown above, since that's the one path that only ever sets these through the
  file, never a real env var.
