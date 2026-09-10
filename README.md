# goggles_capture - Orbit-RS (Rust port)

A line-for-line Rust port of the C++ `goggles_capture` in the parent directory.
Same behaviour, same CLI, same on-the-wire bytes.

Capture the live **1080p H.264** stream from **DJI Goggles 3 / N3** over USB on
any Linux board with a USB device controller: the board pretends to be the
Android Open Accessory the goggles expect, replays the DUML keep-alive commands,
unwraps the `55 CC` framing and writes the **H.264 elementary stream** on
channel `0x4A` to a file or to stdout.

## Build

```sh
cargo build --release
# -> target/release/goggles_capture
```

Linux only. Depends on `libc` (FFI to `mount`, `configfs`/`FunctionFS`, signals,
`pthread`); no other crates.

Cross-compiling for an SBC:

```sh
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

## Run

```sh
sudo ./goggles_capture /tmp/feed.h264      # Ctrl-C to stop
sudo ./goggles_capture stdout | ffmpeg -i - -c copy -f mpegts udp://…
```

Must run as root (it mounts FunctionFS and writes configfs). See the parent
[`README.md`](../README.md) for the hardware setup, putting the port into device
mode, piping, and troubleshooting — all of it applies unchanged.

## CLI

```
usage: goggles_capture [OUTPUT|stdout] [options]

  OUTPUT            H.264 elementary stream output (default ./goggles_feed.h264)

Controller
  --udc NAME        device controller to bind (default: the board's only one)
  --list-udc        list the device controllers, then exit

Self-composed gadget (default)
  --gadget NAME     configfs gadget + FunctionFS instance name (default 'goggles')
  --mount PATH      where to mount FunctionFS (default /dev/ffs-<NAME>)
  --cleanup         remove a gadget left behind by a killed run, then exit

Attach to a gadget composed elsewhere
  --ffs PATH        FunctionFS already mounted here; leave configfs alone
  --gadget-dir DIR  with --ffs: bind/unbind this configfs gadget around the run
```

## Layout

| file | mirrors |
|---|---|
| [`src/common.rs`](src/common.rs)  | `common.{h,cpp}` — logging, `Worker`, `Fd`, `55 CC` framing |
| [`src/gadget.rs`](src/gadget.rs)  | `usb_gadget.{h,cpp}` — configfs/FunctionFS gadget lifecycle |
| [`src/capture.rs`](src/capture.rs)| `goggles_capture.cpp` — CRCs, DUML commands, EP0 events, the session loop |
| [`src/main.rs`](src/main.rs)      | `main()` / argument parsing |

### Port notes

- `Worker` keeps the C++ design exactly: the thread blocks `SIGINT`/`SIGTERM`,
  and `stop()` sets a flag then loops `pthread_kill(tid, SIGUSR1)` until the
  thread confirms it has left a blocking endpoint transfer. The `pthread_t` comes
  from `std::os::unix::thread::JoinHandleExt::as_pthread_t`.
- Shared state that C++ passed by reference into thread lambdas (`Session`, the
  `enabled` flag) is `Arc<…>` of atomics here; raw endpoint fds are passed by
  value, and the owning `Fd` outlives the workers exactly as before.
- Signal handlers are installed with `sigaction` and **no** `SA_RESTART`, so a
  signal makes the blocked syscall return `EINTR`.
- The FunctionFS descriptor/string blocks and the EP0 event struct are written
  and parsed as raw little-endian bytes, so no `linux/usb/functionfs.h` is
  needed at build time.
