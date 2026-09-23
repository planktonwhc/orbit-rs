//! Handshakes with the goggles, ACKs their upstream control packets, and
//! reassembles video fragments into complete Annex-B units, writing them to
//! `sink` (the same file/renderer-stdin `orbit-rs`'s own sink-selection
//! code already opened for the AOA path) -- not stdout, the way the
//! standalone `../../goggles-net/src/capture.rs` this was ported from does
//! (it's meant to run piped: `goggles-net | orbit-rs`).
//!
//! Two real differences from that standalone version, both from
//! `references/dji-rs-flow.md`'s "Porting goggles-net into orbit-rs as a
//! mode" section:
//!
//! 1. Output goes through `common::write_all`/`Fd` (a raw fd, `libc::write`
//!    underneath) instead of `io::Write`/`io::stdout()`, so this can share
//!    `main.rs`'s existing sink-selection code with the AOA path instead of
//!    needing its own.
//! 2. A stall watchdog -- the standalone version's main loop is a bare
//!    `sock.recv()` with no read timeout and no liveness tracking at all,
//!    so a fully dead link (goggles unplugged, Liveview sharing turned
//!    off) hangs this forever with no log line. `dji-rs`'s own strings
//!    describe exactly this distinction ("video payload quiet ... while
//!    transport control is alive" vs a whole-session "quiet for <n>"), and
//!    `../../goggles-net-mac/src/capture.rs` (this same protocol's macOS
//!    port, further along in real-hardware use) independently grew the
//!    same two-clock shape as a FEED-STALL watchdog -- corroborated twice,
//!    not a guess. Its bootstrap/reseed-cache logic built on top of that
//!    watchdog is NOT carried over here: that's specifically compensating
//!    for `ffplay`'s strict software H.264 decoder wanting a seeded IDR at
//!    cold start, and `orbit-kms`'s hardware decoder is already lenient
//!    about a missing IDR (see `goggles-net-mac/README.md`), so there's
//!    nothing here for it to fix.

use std::fs::File;
use std::io::{self, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::time::{Duration, Instant};

use super::{handshake, video};
use crate::common::{errno, log, write_all, Fd};

pub struct Opts<'a> {
    pub bind_addr: &'a str,
    pub peer: SocketAddr,
    pub handshake_timeout: Duration,
    pub probe_only: bool,
    pub hex_rx: bool,
    pub dump_path: Option<&'a Path>,
}

/// How often the receive loop wakes up even with nothing on the wire, so
/// the staleness checks below run close to their own timeout instead of
/// however long `sock.recv()` happens to block for. Same cadence
/// `iface::wait_for`'s own poll loop already uses.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Video quiet, but control ACKs (the actual per-packet keepalive -- see
/// `handshake.rs`) still flowing: almost always "no signal from the air
/// unit yet", not a dead link -- `goggles-net-mac/src/capture.rs`'s own
/// FEED-STALL doc comment makes the identical case for its 2.5s threshold
/// (25-30fps video and even-more-frequent control traffic make a real stall
/// unambiguous well under a second, while FPV links routinely see brief RF
/// hiccups that aren't worth reacting to). Diagnostic only: logged once on
/// each transition, never ends the session by itself -- matches
/// `goggles-net-mac/README.md`'s own warning that a wall of control ACKs
/// with zero video is normal (no aircraft battery in yet), not a bug.
const VIDEO_QUIET_TIMEOUT: Duration = Duration::from_millis(2500);

/// Nothing at all -- not even a control packet -- for this long means the
/// wired session itself is gone (goggles unplugged, Liveview sharing turned
/// off, cable fault), not just a quiet camera. Deliberately longer than
/// `VIDEO_QUIET_TIMEOUT`: control traffic doesn't depend on whether the air
/// unit has a picture, so its own silence is the stronger signal and gets
/// the more conservative (harder to false-positive) timeout. Fatal: ends
/// the session so the caller (and, on a real appliance, the service
/// supervisor) can restart fresh, same severity as any other unrecoverable
/// transport error here.
const SESSION_QUIET_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(sink: Fd, sink_label: &str, opts: Opts) -> io::Result<()> {
    let bind: SocketAddr = format!("{}:{}", opts.bind_addr, opts.peer.port())
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad bind address: {e}")))?;
    let sock = UdpSocket::bind(bind)?;
    sock.connect(opts.peer)?;
    log(format!(
        "goggles-net: listening on {bind}, expecting the goggles at {}, writing to {sink_label}",
        opts.peer
    ));

    let session = handshake::open(&sock, opts.handshake_timeout)?;
    log(format!(
        "goggles-net: active session=0x{:04x} initial-seq=0x{:04x}",
        session.id, session.sequence
    ));
    if opts.probe_only {
        log("goggles-net: probe successful; exiting before video receive loop");
        return Ok(());
    }

    // Blocking-with-a-timeout from here, not the fully-blocking mode
    // `handshake::open` leaves the socket in on success: the loop below
    // needs to wake up on its own even with nothing arriving, to run the
    // staleness checks.
    sock.set_read_timeout(Some(POLL_INTERVAL))?;

    let mut dump = match opts.dump_path {
        Some(p) => Some(File::create(p)?),
        None => None,
    };

    let mut buf = [0u8; 65536];
    let mut assembler = video::Assembler::new();
    let mut seen_video = false;
    let mut control_acks: u64 = 0;
    let mut units_written: u64 = 0;

    // Liveness clocks -- see this file's own doc comment and the constants
    // above for why there are two of them, not one.
    let mut last_any_at = Instant::now();
    let mut last_control_at: Option<Instant> = None;
    let mut last_video_at: Option<Instant> = None;
    let mut video_stalled = false; // one-shot latch, so the log line doesn't repeat every poll

    loop {
        match sock.recv(&mut buf) {
            Ok(n) => {
                last_any_at = Instant::now();
                let data = &buf[..n];

                if opts.hex_rx {
                    log(format!("goggles-net: RX {n} B: {}", handshake::hex(data)));
                }
                if let Some(f) = dump.as_mut() {
                    f.write_all(data)?;
                }

                // Upstream control packets must be ACK'd or the goggles stall.
                if let Some(ack) = handshake::build_control_ack(data, session.id) {
                    sock.send(&ack)?;
                    control_acks += 1;
                    last_control_at = Some(last_any_at);
                    if opts.hex_rx || control_acks == 1 {
                        log(format!("goggles-net: TX control ACK #{control_acks}"));
                    }
                } else {
                    match handshake::packet_type(data, session.id) {
                        Some(0x02) => {
                            if !seen_video {
                                log("goggles-net: first video packet -- streaming");
                                seen_video = true;
                            }
                            if let Some(frag) = video::parse_fragment(data) {
                                if let Some(unit) = assembler.feed(&frag) {
                                    last_video_at = Some(last_any_at);
                                    if video_stalled {
                                        video_stalled = false;
                                        log("goggles-net: video resumed");
                                    }
                                    units_written += 1;
                                    if opts.hex_rx && units_written <= 3 {
                                        log(format!(
                                            "goggles-net: unit #{units_written}: {} bytes, starts {}",
                                            unit.len(),
                                            handshake::hex(&unit[..unit.len().min(8)])
                                        ));
                                    }
                                    // SIGPIPE is ignored (main.rs), so a downstream
                                    // consumer that went away shows up here as EPIPE.
                                    if !write_all(sink.raw(), &unit) {
                                        return Err(io::Error::from_raw_os_error(errno()));
                                    }
                                }
                            }
                        }
                        Some(kind) if opts.hex_rx => {
                            log(format!("goggles-net: transport type=0x{kind:02x} ({n} B, not video)"));
                        }
                        _ => {}
                    }
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }

        // Run on every iteration -- whether a packet just arrived or the
        // poll interval simply elapsed -- so detection latency tracks
        // POLL_INTERVAL, not whatever `sock.recv()` happened to do.
        let now = Instant::now();
        let quiet = now.duration_since(last_any_at);
        if quiet >= SESSION_QUIET_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "wired session quiet for {}ms (video={units_written} units, \
                     control_acks={control_acks}) -- treating the link as dead",
                    quiet.as_millis()
                ),
            ));
        }
        if seen_video && !video_stalled {
            let video_quiet = last_video_at.map_or(true, |t| now.duration_since(t) >= VIDEO_QUIET_TIMEOUT);
            let control_alive = last_control_at.is_some_and(|t| now.duration_since(t) < VIDEO_QUIET_TIMEOUT);
            if video_quiet && control_alive {
                video_stalled = true;
                log("goggles-net: video payload quiet while transport control is alive -- \
                     likely no signal from the air unit yet, not a dead link");
            }
        }
    }
}
