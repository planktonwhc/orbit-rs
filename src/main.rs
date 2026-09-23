//! orbit-rs -- capture the live 1080p H.264 stream from DJI Goggles 3 / N3 / 2
//! over USB, and hand it to a renderer. Two capture modes, picked by
//! `ORBIT_MODE` (see [`Mode`]): the default `aoa` (USB-C gadget/cable link,
//! this board as the emulated accessory) and `goggles-net` (regular USB
//! port, "Liveview sharing", this board as a plain RNDIS network client --
//! see `goggles_net/mod.rs`). Both feed the same sink-selection code below.
//!
//! A bare `orbit-rs` run captures and spawns the renderer itself (orbit-kms by
//! default), piping the elementary stream into its stdin -- the renderer
//! inherits the environment, so config/orbit.env applies. Passing an explicit
//! OUTPUT (a file, or `stdout`) keeps the old behaviour and spawns nothing.

mod capture;
mod common;
mod envcfg;
mod gadget;
mod goggles_net;

use std::os::unix::io::IntoRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use capture::{on_stop, run_capture, Options};
use common::{catch_signal, errno, install_worker_kick_handler, log, strerror, Fd};
use gadget::{open_sink, FunctionFsGadget};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which transport captures the video -- `ORBIT_MODE`, mirroring the
/// reference appliance's own `op_mode` config key (see
/// `references/dji-rs-flow.md`'s "Porting goggles-net into orbit-rs as a
/// mode" section): a plain config value picked once at startup, not a
/// second process spawned and piped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// USB-C gadget/cable link: this board emulates the DJI AOA accessory,
    /// the goggles are the USB host. `capture.rs` + `gadget.rs`.
    Aoa,
    /// Regular (non-OTG) USB port, "Liveview sharing" turned on: the
    /// goggles enumerate as an RNDIS network device, this board is a plain
    /// UDP client. `goggles_net/`.
    GogglesNet,
}

fn mode_from_env() -> Result<Mode, String> {
    match std::env::var("ORBIT_MODE") {
        Err(_) => Ok(Mode::Aoa),
        Ok(v) if v.trim().is_empty() || v.trim() == "aoa" => Ok(Mode::Aoa),
        Ok(v) if v.trim() == "goggles-net" => Ok(Mode::GogglesNet),
        Ok(v) => Err(format!("ORBIT_MODE={v:?} not understood (expected 'aoa' or 'goggles-net')")),
    }
}

fn usage() {
    eprint!(
        r#"orbit-rs {VERSION} - DJI Goggles 3 / N3 / 2 USB H.264 capture + renderer supervisor

usage: orbit-rs [OUTPUT|stdout] [options]

  OUTPUT             file to write the H.264 elementary stream to.
                     'stdout' pipes it. With no OUTPUT, orbit-rs spawns a
                     renderer (ORBIT_RENDERER, default /usr/local/bin/orbit-kms
                     when /dev/dri exists) and feeds its stdin.

  -h, --help         show this help, then exit
  -V, --version      print the version, then exit

Mode (ORBIT_MODE=aoa|goggles-net, default aoa)
  aoa                USB-C gadget/cable link (default) -- this board emulates
                     the DJI AOA accessory; goggles are the USB host.
  goggles-net        regular USB port, "Liveview sharing" -- goggles enumerate
                     as an RNDIS network device; this board is a UDP client.

Controller (aoa mode)
  --udc NAME         device controller to bind; default is the board's
                     only one. --list-udc shows what this board has.
  --list-udc         list the device controllers, then exit

Self-composed gadget (aoa mode, default)
  --gadget NAME      configfs gadget + FunctionFS instance name (default 'goggles')
  --mount PATH       where to mount FunctionFS (default /dev/ffs-<NAME>)
  --cleanup          remove a gadget left behind by a killed run, then exit

Attach to a gadget composed elsewhere (aoa mode)
  --ffs PATH         FunctionFS is already mounted here; leave configfs alone
  --gadget-dir DIR   with --ffs: bind and unbind this configfs gadget
                     around the run. Omit it to leave the UDC alone too.

RNDIS interface (goggles-net mode)
  --iface NAME       RNDIS interface (env GOGGLES_NET_IFACE; default:
                     auto-detect the USB parent with idVendor:idProduct
                     2ca3:0020)
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

Environment
  ORBIT_MODE         aoa (default) | goggles-net -- see Mode above.
  ORBIT_RENDERER     renderer command (whitespace-split). 'none'/'off'/'' -> just
                     write the default file. Unset -> /usr/local/bin/orbit-kms
                     if /dev/dri/card0|card1 exists, else the default file.
  ORBIT_CODEC/CODEC  h264 (default) | h265/hevc -> passed to an auto-picked
                     orbit-kms as --h264 / --h265.
  ORBIT_ENV_FILE     path to the KEY=value config to fold into the environment
                     ('none' to skip). See config/orbit-rs.env.
"#
    );
}

/// The renderer argv, or None to fall back to writing `out_path`.
fn renderer_argv() -> Option<Vec<String>> {
    match std::env::var("ORBIT_RENDERER") {
        Ok(v) => {
            let v = v.trim();
            if v.is_empty() || v == "none" || v == "off" {
                return None;
            }
            let argv: Vec<String> = v.split_whitespace().map(String::from).collect();
            if argv.is_empty() {
                None
            } else {
                Some(argv)
            }
        }
        Err(_) => {
            // Auto: orbit-kms, but only if a KMS device is present.
            if !Path::new("/dev/dri/card0").exists() && !Path::new("/dev/dri/card1").exists() {
                return None;
            }
            let mut argv = vec!["/usr/local/bin/orbit-kms".to_string()];
            let codec = std::env::var("ORBIT_CODEC")
                .or_else(|_| std::env::var("CODEC"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            match codec.as_str() {
                "h265" | "hevc" => argv.push("--h265".to_string()),
                "h264" | "" => {}
                other => log(format!("ORBIT_CODEC={} not understood, assuming h264", other)),
            }
            Some(argv)
        }
    }
}

fn open_file_sink(out_path: &str) -> Result<Fd, ()> {
    let s = open_sink(out_path);
    if s.is_valid() {
        Ok(s)
    } else {
        log(format!("open {}: {}", out_path, strerror(errno())));
        Err(())
    }
}

fn real_main() -> i32 {
    let mode = match mode_from_env() {
        Ok(m) => m,
        Err(e) => {
            log(e);
            return 1;
        }
    };

    let args: Vec<String> = std::env::args().collect();
    let mut out_path = "./goggles_feed.h264".to_string();
    let mut out_path_given = false;
    let mut opt = Options::default();
    let mut gn_opt = goggles_net::Opts::default();
    let mut cleanup = false;
    let mut list_udc = false;

    let mut i = 1;
    while i < args.len() {
        let a = args[i].clone();
        let has_value = i + 1 < args.len();
        if a == "--cleanup" {
            cleanup = true;
        } else if a == "--list-udc" {
            list_udc = true;
        } else if a == "--udc" && has_value {
            i += 1;
            opt.udc = args[i].clone();
        } else if a == "--gadget" && has_value {
            i += 1;
            opt.gadget_name = args[i].clone();
        } else if a == "--mount" && has_value {
            i += 1;
            opt.mount_point = args[i].clone();
        } else if a == "--ffs" && has_value {
            i += 1;
            opt.ffs_path = args[i].clone();
        } else if a == "--gadget-dir" && has_value {
            i += 1;
            opt.gadget_dir = args[i].clone();
        } else if a == "--iface" && has_value {
            i += 1;
            gn_opt.iface = Some(args[i].clone());
        } else if a == "--peer" && has_value {
            i += 1;
            match args[i].parse() {
                Ok(peer) => gn_opt.peer = peer,
                Err(e) => {
                    log(format!("bad --peer '{}': {e}", args[i]));
                    return 1;
                }
            }
        } else if a == "--ip" && has_value {
            i += 1;
            gn_opt.ip = args[i].clone();
        } else if a == "--wait" && has_value {
            i += 1;
            match args[i].parse::<u64>() {
                Ok(secs) => gn_opt.wait_iface = std::time::Duration::from_secs(secs),
                Err(_) => {
                    log(format!("bad --wait '{}' (expected a number of seconds)", args[i]));
                    return 1;
                }
            }
        } else if a == "--probe" {
            gn_opt.probe_only = true;
        } else if a == "--hex-rx" {
            gn_opt.hex_rx = true;
        } else if a == "--handshake-ms" && has_value {
            i += 1;
            match args[i].parse::<u64>() {
                Ok(ms) => gn_opt.handshake_timeout = std::time::Duration::from_millis(ms),
                Err(_) => {
                    log(format!("bad --handshake-ms '{}'", args[i]));
                    return 1;
                }
            }
        } else if a == "--dump" && has_value {
            i += 1;
            gn_opt.dump = Some(std::path::PathBuf::from(args[i].clone()));
        } else if a == "-h" || a == "--help" {
            usage();
            return 0;
        } else if a == "-V" || a == "--version" {
            println!("orbit-rs {}", VERSION);
            return 0;
        } else if !a.is_empty() && a.starts_with('-') {
            usage();
            return 1;
        } else {
            out_path = a;
            out_path_given = true;
        }
        i += 1;
    }

    let mode_label = match mode {
        Mode::Aoa => "AOA capture",
        Mode::GogglesNet => "goggles-net capture",
    };
    log(format!("orbit-rs v{} (DJI goggles {})", VERSION, mode_label));

    if list_udc {
        let udcs = FunctionFsGadget::list_udcs();
        if udcs.is_empty() {
            log("no UDC under /sys/class/udc - is the port in device/peripheral mode?");
            return 1;
        }
        for u in &udcs {
            println!("{}", u);
        }
        return 0;
    }

    if !opt.ffs_path.is_empty() && (!opt.mount_point.is_empty() || cleanup) {
        log("--ffs attaches to an existing gadget; --mount and --cleanup do not apply to it");
        return 1;
    }
    if !opt.gadget_dir.is_empty() && opt.ffs_path.is_empty() {
        log("--gadget-dir only means something together with --ffs");
        return 1;
    }
    if unsafe { libc::geteuid() } != 0 {
        log("must run as root");
        return 1;
    }

    if cleanup {
        // The stale gadget is removed either way; the controller is only
        // needed to hand it back to its previous owner.
        let udc = capture::pick_udc(&opt.udc).unwrap_or_default();
        if udc.is_empty() && !opt.udc.is_empty() {
            return 1;
        }
        return if FunctionFsGadget::cleanup_stale(&opt.gadget_name, &opt.mount_point, &udc) {
            0
        } else {
            1
        };
    }

    // Fold config/orbit.env into the environment before we spawn anything, so a
    // renderer child inherits it. Anything already set is left untouched.
    envcfg::load();

    // Decide where the elementary stream goes.
    let mut renderer_child: Option<Child> = None;
    let (sink, sink_label) = if out_path_given {
        match open_file_sink(&out_path) {
            Ok(s) => (s, out_path.clone()),
            Err(()) => return 1,
        }
    } else {
        match renderer_argv() {
            None => {
                log(format!(
                    "no renderer (ORBIT_RENDERER unset and no /dev/dri); writing {}",
                    out_path
                ));
                match open_file_sink(&out_path) {
                    Ok(s) => (s, out_path.clone()),
                    Err(()) => return 1,
                }
            }
            Some(argv) => {
                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]).stdin(Stdio::piped());
                match cmd.spawn() {
                    Ok(mut c) => {
                        let child_stdin = c.stdin.take().expect("stdin was piped");
                        let fd = child_stdin.into_raw_fd(); // our Fd owns it now
                        log(format!("renderer: {} (pid {})", argv.join(" "), c.id()));
                        renderer_child = Some(c);
                        (Fd::new(fd), argv.join(" "))
                    }
                    Err(e) => {
                        log(format!("renderer: cannot exec {}: {}", argv[0], e));
                        return 1;
                    }
                }
            }
        }
    };

    catch_signal(libc::SIGINT, on_stop);
    catch_signal(libc::SIGTERM, on_stop);
    install_worker_kick_handler();
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) }; // downstream pipe may close first

    // Both modes consume `sink` and drop it on return, so the renderer sees
    // EOF on its stdin and shuts itself down either way; then we reap it.
    let rc = match mode {
        Mode::Aoa => run_capture(sink, &sink_label, &opt),
        Mode::GogglesNet => match goggles_net::run(sink, &sink_label, &gn_opt) {
            Ok(()) => 0,
            Err(e) => {
                log(format!("goggles-net: {e}"));
                1
            }
        },
    };

    if let Some(mut c) = renderer_child {
        match c.wait() {
            Ok(status) => log(format!("renderer exited: {}", status)),
            Err(e) => log(format!("renderer wait failed: {}", e)),
        }
    }
    rc
}

fn main() {
    std::process::exit(real_main());
}
