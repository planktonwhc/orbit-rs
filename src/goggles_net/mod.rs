//! DJI Goggles 3 "Liveview sharing" (RNDIS/UDP) capture -- the second mode
//! `main.rs` can select via `ORBIT_MODE=goggles-net`, alongside the default
//! AOA/gadget mode (`../capture.rs` + `../gadget.rs`). Ported from the
//! standalone `../../goggles-net` project, which stays as-is (this is a
//! copy, not a move) -- see `references/dji-rs-flow.md`'s "Porting
//! goggles-net into orbit-rs as a mode" section for the full writeup this
//! summarizes.
//!
//! Compiled into the one `orbit-rs` binary and picked at startup by a
//! config key, rather than spawned as a second process piped into this
//! one -- mirrors how the reference appliance (`dji-rs`) switches its own
//! `op_mode` from its web panel: a plain config value, re-derived on load,
//! not a process juggling act.
//!
//! Differs from the AOA path in USB *role*, not just wire protocol: there,
//! the goggles are the USB host and this board is the emulated accessory
//! (FunctionFS gadget). Here, the goggles enumerate as a plain RNDIS
//! network device on a regular (non-OTG) USB port, and this board is an
//! ordinary network client -- no gadget, no configfs, just a UDP socket
//! opened after the kernel's own RNDIS driver brings the interface up.

mod capture;
mod handshake;
mod iface;
mod video;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::common::{log, Fd};

pub const DJI_VID: u16 = 0x2ca3;
pub const RNDIS_PID: u16 = 0x0020;

pub struct Opts {
    pub iface: Option<String>,
    pub peer: SocketAddr,
    pub ip: String,
    pub wait_iface: Duration,
    pub handshake_timeout: Duration,
    pub probe_only: bool,
    pub hex_rx: bool,
    pub dump: Option<PathBuf>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            iface: std::env::var("GOGGLES_NET_IFACE").ok(),
            peer: std::env::var("GOGGLES_NET_PEER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| "192.168.60.2:9003".parse().unwrap()),
            ip: "192.168.60.1/24".to_string(),
            wait_iface: Duration::from_secs(30),
            handshake_timeout: Duration::from_millis(5000),
            probe_only: false,
            hex_rx: false,
            dump: None,
        }
    }
}

/// Finds (or takes `opts.iface` as given) and brings up the RNDIS
/// interface, then hands off to the handshake/capture loop. `sink` is
/// consumed and dropped when this returns -- same contract as
/// `capture::run_capture`'s own sink -- so a spawned renderer sees EOF on
/// its stdin either way, whichever mode produced the bytes.
pub fn run(sink: Fd, sink_label: &str, opts: &Opts) -> io::Result<()> {
    let iface = match &opts.iface {
        Some(name) => name.clone(),
        None => match iface::wait_for(DJI_VID, RNDIS_PID, opts.wait_iface) {
            Some(name) => name,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no {DJI_VID:04x}:{RNDIS_PID:04x} RNDIS interface after {}s -- is the \
                         goggles plugged into a REGULAR USB port (not the gadget/OTG one) with \
                         Liveview sharing turned on? (or pass --iface / GOGGLES_NET_IFACE to \
                         skip auto-detect)",
                        opts.wait_iface.as_secs()
                    ),
                ));
            }
        },
    };
    log(format!("goggles-net: using interface {iface}"));

    iface::bring_up(&iface, &opts.ip)?;

    capture::run(
        sink,
        sink_label,
        capture::Opts {
            bind_addr: "0.0.0.0",
            peer: opts.peer,
            handshake_timeout: opts.handshake_timeout,
            probe_only: opts.probe_only,
            hex_rx: opts.hex_rx,
            dump_path: opts.dump.as_deref(),
        },
    )
}
