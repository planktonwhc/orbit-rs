//! The capture runtime: compose (or attach to) the AOA gadget, replay the DUML
//! keep-alive commands, unwrap the goggles' 55 CC frames and write the H.264
//! elementary stream on channel 0x4A to the sink.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::common::{
    comma_separated, errno, log, next_frame, strerror, write_all, Fd, Worker,
};
use crate::gadget::{
    FunctionFsGadget, GadgetIdentity, FFS_EVENT_SIZE, FUNCTIONFS_DISABLE, FUNCTIONFS_ENABLE,
    FUNCTIONFS_SETUP, FUNCTIONFS_UNBIND,
};

// --- USB identity ----------------------------------------------------------
pub const VID: u16 = 0x18D1; // Google
pub const PID: u16 = 0x2D01; // AOA accessory
pub const VIDEO_CHANNEL: u8 = 0x4A;

// --- Android Open Accessory identity strings --------------------------------
// The goggles check these before they agree to open the video path.
fn aoa_identity() -> GadgetIdentity {
    GadgetIdentity {
        vid: VID,
        pid: PID,
        manufacturer: "Google Inc.".to_string(),
        product: "Android-powered device in accessory mode".to_string(),
        serial: "da64sxd1".to_string(),
        configuration: "High speed configuration".to_string(),
        interface_name: "Android Accessory Interface".to_string(),
    }
}

// --- signals -----------------------------------------------------------------
// The stop signals are the main thread's alone; Worker blocks them in every
// thread it starts, so they always land here.
static G_STOP: AtomicBool = AtomicBool::new(false); // SIGINT / SIGTERM

pub extern "C" fn on_stop(_: libc::c_int) {
    G_STOP.store(true, Ordering::SeqCst);
}

// --- DJI DUML checksums (CRC-8/Maxim seed 0x77, CRC-16/X-25 seed 0x3692) -----
struct CrcTables {
    crc8: [u8; 256],
    crc16: [u16; 256],
}

fn crc_tables() -> &'static CrcTables {
    static TABLES: OnceLock<CrcTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut t = CrcTables {
            crc8: [0; 256],
            crc16: [0; 256],
        };
        for i in 0..256usize {
            let mut c8: u32 = i as u32;
            let mut c16: u32 = i as u32;
            for _ in 0..8 {
                c8 = if c8 & 1 != 0 { (c8 >> 1) ^ 0x8C } else { c8 >> 1 };
                c16 = if c16 & 1 != 0 { (c16 >> 1) ^ 0x8408 } else { c16 >> 1 };
            }
            t.crc8[i] = c8 as u8;
            t.crc16[i] = c16 as u16;
        }
        t
    })
}

pub fn crc8(d: &[u8]) -> u8 {
    let t = crc_tables();
    let mut c: u8 = 0x77;
    for &b in d {
        c = t.crc8[(c ^ b) as usize];
    }
    c
}

pub fn crc16(d: &[u8]) -> u16 {
    let t = crc_tables();
    let mut c: u16 = 0x3692;
    for &b in d {
        c = t.crc16[((c ^ b as u16) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

// --- DUML keep-alive commands ---------------------------------------------
// Pre-recorded inner packets from the official receiver. COMMANDS[5] ("APP")
// tells the goggles to start streaming; the rest are normal keep-alive chatter.
static COMMANDS: &[&[u8]] = &[
    &[
        0x55, 0x10, 0x04, 0x56, 0x02, 0x88, 0x54, 0xad, 0x40, 0x00, 0xe5, 0x04, 0x04, 0x01, 0x68,
        0x9c,
    ],
    &[
        0x55, 0x0e, 0x04, 0x66, 0x02, 0x28, 0x55, 0xad, 0x40, 0x00, 0x51, 0x06, 0x7c, 0x9c,
    ],
    &[
        0x55, 0x16, 0x04, 0xfc, 0x02, 0x48, 0x56, 0xad, 0x40, 0x00, 0x4f, 0x01, 0x00, 0x16, 0x00,
        0x00, 0xff, 0xff, 0xff, 0xff, 0x5c, 0xe9,
    ],
    &[
        0x55, 0x1e, 0x04, 0x8a, 0x02, 0x01, 0x63, 0xad, 0x40, 0x02, 0xeb, 0x00, 0xff, 0x03, 0x11,
        0x27, 0x00, 0x00, 0x0a, 0x00, 0x03, 0x00, 0x08, 0x00, 0xd1, 0x07, 0x75, 0x17, 0x6f, 0x3d,
    ],
    &[
        0x55, 0x0e, 0x04, 0x66, 0x02, 0x2d, 0x7e, 0x01, 0x80, 0x00, 0x82, 0x00, 0xd2, 0x72,
    ],
    &[
        0x55, 0x1b, 0x04, 0x75, 0x02, 0x3c, 0x68, 0xad, 0x40, 0x00, 0x88, 0x17, 0x00, 0x00, 0x23,
        0x00, 0x41, 0x50, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x98, 0xf0,
    ], // APP
    &[
        0x55, 0x0e, 0x04, 0x66, 0x02, 0x01, 0x6a, 0xad, 0x40, 0x02, 0xd0, 0x04, 0x20, 0xa8,
    ],
    &[
        0x55, 0x0e, 0x04, 0x66, 0x02, 0x01, 0x42, 0x86, 0x40, 0x08, 0x41, 0x02, 0xad, 0xd2,
    ],
    &[
        0x55, 0x0d, 0x04, 0x33, 0x02, 0x0e, 0x4b, 0x86, 0x00, 0x00, 0x00, 0xc1, 0x2a,
    ],
];

// Fill in the sequence number + CRCs, then add the outer USB wrapper.
pub fn build_command(inner: &[u8], seq: u16) -> Vec<u8> {
    let mut p = inner.to_vec();
    p[6] = (seq & 0xFF) as u8;
    p[7] = (seq >> 8) as u8;
    p[3] = crc8(&p[..3]);
    let c = crc16(&p[..p.len() - 2]);
    let n = p.len();
    p[n - 2] = (c & 0xFF) as u8;
    p[n - 1] = (c >> 8) as u8;

    let wrapped_len = p.len() as u16;
    let mut out: Vec<u8> = Vec::with_capacity(8 + p.len());
    out.extend_from_slice(&[
        0x55,
        0xCC,
        0x49,
        0x57,
        (wrapped_len & 0xFF) as u8,
        (wrapped_len >> 8) as u8,
        (seq & 0xFF) as u8,
        (seq >> 8) as u8,
    ]);
    out.extend_from_slice(&p);
    out
}

// --- EP0 events --------------------------------------------------------------
// FunctionFS answers every standard control request itself (the descriptors and
// identity strings come from configfs), so ep0 only tells us when the host has
// enabled the interface -- and hands us the odd vendor request addressed to it,
// which we stall.
//
// A no-data request is confirmed with a zero-length transfer in the data-stage
// direction; the same transfer in the opposite direction stalls it instead.
fn stall(fd: libc::c_int, host_to_device: bool) {
    let mut x: u8 = 0;
    unsafe {
        if host_to_device {
            let _ = libc::write(fd, &x as *const u8 as *const libc::c_void, 0);
        } else {
            let _ = libc::read(fd, &mut x as *mut u8 as *mut libc::c_void, 0);
        }
    }
}

fn event_loop(fd: libc::c_int, enabled: &AtomicBool, quit: &AtomicBool) {
    let mut ev = [0u8; FFS_EVENT_SIZE * 8];
    while !quit.load(Ordering::SeqCst) {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, 200) } <= 0 {
            continue;
        }

        let n = unsafe { libc::read(fd, ev.as_mut_ptr() as *mut libc::c_void, ev.len()) };
        if n < 0 {
            let e = errno();
            if e == libc::EINTR || e == libc::EAGAIN {
                continue;
            }
            log(format!("ep0 read failed: {}", strerror(e)));
            return;
        }

        let count = n as usize / FFS_EVENT_SIZE;
        for i in 0..count {
            let base = i * FFS_EVENT_SIZE;
            let etype = ev[base + 8];
            match etype {
                FUNCTIONFS_ENABLE => {
                    if !enabled.swap(true, Ordering::SeqCst) {
                        log("USB: enabled (host configured us)");
                    }
                }
                FUNCTIONFS_DISABLE => {
                    if enabled.swap(false, Ordering::SeqCst) {
                        log("USB: disabled");
                    }
                }
                FUNCTIONFS_UNBIND => {
                    log("USB: unbind");
                    enabled.store(false, Ordering::SeqCst);
                }
                FUNCTIONFS_SETUP => {
                    // setup.bRequestType is the first byte of the event.
                    let b_request_type = ev[base];
                    stall(fd, (b_request_type & 0x80) == 0);
                }
                _ => {} // BIND / SUSPEND / RESUME
            }
        }
    }
}

// --- one session = one plug-in ---------------------------------------------
// What the two data-flow workers report back to the main thread.
pub struct Session {
    pub over: AtomicBool,        // a worker ended it; main tears down and starts over
    pub failed: AtomicBool,      // ...because of an unexpected I/O error
    pub sink_closed: AtomicBool, // ...because nobody reads the video any more
}

impl Session {
    fn new() -> Session {
        Session {
            over: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            sink_closed: AtomicBool::new(false),
        }
    }

    fn end(&self) {
        self.over.store(true, Ordering::SeqCst);
    }

    fn fail(&self, what: &str, err: libc::c_int) {
        log(format!("{}: {}", what, strerror(err)));
        self.failed.store(true, Ordering::SeqCst);
        self.over.store(true, Ordering::SeqCst);
    }

    // An endpoint that went away is the normal end of a session -- the host
    // disabled the interface and a DISABLE event is on its way. Anything else
    // is a real error.
    fn endpoint_error(&self, what: &str, err: libc::c_int) {
        if err == libc::ESHUTDOWN || err == libc::ENODEV {
            self.end();
        } else {
            self.fail(what, err);
        }
    }
}

// Keep-alive: replay the DUML commands ~once a second for as long as the host
// keeps the interface enabled. Stop, and the goggles cut the video after ~11 s.
fn send_commands(fd: libc::c_int, s: &Session, quit: &AtomicBool) {
    let mut seq: u16 = 0;
    while !quit.load(Ordering::SeqCst) {
        for inner in COMMANDS {
            if quit.load(Ordering::SeqCst) {
                return;
            }
            let pkt = build_command(inner, seq);
            seq = seq.wrapping_add(1);
            // Blocks until the goggles drain the endpoint; Worker::stop() is
            // what ends it when they never do.
            let w = unsafe { libc::write(fd, pkt.as_ptr() as *const libc::c_void, pkt.len()) };
            if w < 0 {
                let e = errno();
                if e != libc::EINTR && e != libc::EAGAIN {
                    s.endpoint_error("command write", e);
                    return;
                }
            }
            unsafe { libc::usleep(20_000) };
        }
        unsafe { libc::usleep(1_000_000) };
    }
}

// Offset of the first H.264 SPS (NAL type 7) at a start code, or None.
// We start the stream here so a downstream decoder gets a clean first buffer.
pub fn find_sps(p: &[u8]) -> Option<usize> {
    if p.len() < 5 {
        return None;
    }
    for i in 0..p.len() - 4 {
        if p[i] == 0 && p[i + 1] == 0 && p[i + 2] == 0 && p[i + 3] == 1 && (p[i + 4] & 0x1F) == 7 {
            return Some(i);
        }
    }
    None
}

// Read the goggles' stream, unwrap the frames, write channel 0x4A to sink.
// The read size is a multiple of the 512-byte high-speed max packet, which
// FunctionFS requires for OUT endpoints.
fn receive_video(fd: libc::c_int, sink: libc::c_int, s: &Session, quit: &AtomicBool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    let mut total: usize = 0;
    let mut started = false; // only emit once we've seen the first SPS

    while !quit.load(Ordering::SeqCst) {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n < 0 {
            let e = errno();
            if e == libc::EINTR || e == libc::EAGAIN {
                continue;
            }
            s.endpoint_error("video read", e);
            return;
        }
        buf.extend_from_slice(&chunk[..n as usize]);

        let mut pos = 0usize;
        while let Some(frame) = next_frame(&buf, &mut pos) {
            if frame.channel != VIDEO_CHANNEL || frame.payload.is_empty() {
                continue;
            }

            let mut payload: &[u8] = frame.payload;
            if !started {
                // skip the partial leading NAL
                match find_sps(payload) {
                    None => continue,
                    Some(sps) => {
                        payload = &payload[sps..];
                        started = true;
                        log(">>> first SPS seen - clean H.264 stream starts here");
                    }
                }
            }

            // SIGPIPE is ignored, so a consumer that went away shows up here as
            // EPIPE. EINTR only means Worker::stop() is asking us to leave.
            if !write_all(sink, payload) {
                let e = errno();
                if e != libc::EINTR {
                    log(format!("output closed ({})", strerror(e)));
                    s.sink_closed.store(true, Ordering::SeqCst);
                }
                s.end();
                return;
            }
            total += payload.len();
            if total % (1 << 20) < payload.len() {
                log(format!("  wrote {} KiB", total / 1024));
            }
        }
        buf.drain(..pos);
    }
}

// --- runtime -----------------------------------------------------------------
// Pick the controller to run on. Boards can have more than one -- an SoC with
// two OTG cores, or a dwc3 and a dwc2 on different ports -- and only the one
// wired to the port the goggles plug into will do, so say what we found.
pub fn pick_udc(wanted: &str) -> Option<String> {
    let udcs = FunctionFsGadget::list_udcs();
    if udcs.is_empty() {
        log("no UDC under /sys/class/udc - is the port in device/peripheral mode?");
        return None;
    }
    if !wanted.is_empty() {
        if udcs.iter().any(|u| u == wanted) {
            return Some(wanted.to_string());
        }
        log(format!("no such UDC: {}", wanted));
        log(format!("this board has: {}", comma_separated(&udcs)));
        return None;
    }
    if udcs.len() > 1 {
        log(format!(
            "this board has several UDCs ({}); using {} - pick another with --udc",
            comma_separated(&udcs),
            udcs[0]
        ));
    }
    Some(udcs[0].clone())
}

// --- how the gadget should be brought up ------------------------------------
// An empty ffs_path means "compose it ourselves"; otherwise we attach to a
// FunctionFS instance someone else set up (an init script, an existing
// multi-function gadget).
pub struct Options {
    pub udc: String,         // empty => the board's only UDC
    pub gadget_name: String, // configfs gadget + ffs instance name
    pub mount_point: String, // empty => /dev/ffs-<gadget_name>
    pub ffs_path: String,    // attach mode
    pub gadget_dir: String,  // attach mode: gadget to bind, optional
}

impl Default for Options {
    fn default() -> Options {
        Options {
            udc: String::new(),
            gadget_name: "goggles".to_string(),
            mount_point: String::new(),
            ffs_path: String::new(),
            gadget_dir: String::new(),
        }
    }
}

// Sessions that fail back to back without ever streaming: give up and let the
// supervisor restart us, rather than logging the same failure forever.
const MAX_CONSECUTIVE_FAILURES: i32 = 5;

// `sink` is the already-open output: a file, a dup of stdout, or the stdin of a
// spawned renderer. `sink_label` is used only in log lines. Returns the exit code.
pub fn run_capture(sink: Fd, sink_label: &str, opt: &Options) -> i32 {
    // Attaching to a FunctionFS that somebody else binds needs no controller.
    let needs_udc = opt.ffs_path.is_empty() || !opt.gadget_dir.is_empty();
    let mut udc = String::new();
    if needs_udc {
        match pick_udc(&opt.udc) {
            Some(u) => udc = u,
            None => return 1,
        }
    }

    let mut gadget = FunctionFsGadget::new(opt.gadget_name.clone(), udc);
    if !opt.mount_point.is_empty() {
        gadget.set_mount_point(opt.mount_point.clone());
    }

    let id = aoa_identity();
    let up = if opt.ffs_path.is_empty() {
        gadget.setup(&id)
    } else {
        gadget.attach(&opt.ffs_path, &opt.gadget_dir, &id)
    };
    if !up {
        return 1;
    }

    let sink_fd = sink.raw();

    let enabled = Arc::new(AtomicBool::new(false));
    let ep0_fd = gadget.ep0();
    let _events = {
        let enabled = Arc::clone(&enabled);
        Worker::new(move |quit: &AtomicBool| event_loop(ep0_fd, &enabled, quit))
    };
    log("descriptors written; plug in the goggles");

    // Each pass is one plug-in: FunctionFS invalidates the endpoint files when
    // the host disables the interface, so they are reopened per session. The
    // main thread never touches an endpoint itself; it only watches the flags
    // and stops the workers, so a stop signal always has somewhere to land.
    let mut failures = 0;
    while !G_STOP.load(Ordering::SeqCst) {
        if !enabled.load(Ordering::SeqCst) {
            unsafe { libc::usleep(100_000) };
            continue;
        }
        unsafe { libc::usleep(200_000) }; // the real receiver waits ~200 ms

        let s = Arc::new(Session::new());
        let in_fd = Fd::new(gadget.open_endpoint("ep1")); // commands out to the goggles
        let out_fd = Fd::new(gadget.open_endpoint("ep2")); // video in from the goggles
        if !in_fd.is_valid() || !out_fd.is_valid() {
            let e = errno();
            s.fail("open endpoints", e);
        } else {
            log(format!("streaming video -> {}", sink_label));
            let rin = in_fd.raw();
            let rout = out_fd.raw();
            {
                let _sender = {
                    let s = Arc::clone(&s);
                    Worker::new(move |quit: &AtomicBool| send_commands(rin, &s, quit))
                };
                let _receiver = {
                    let s = Arc::clone(&s);
                    Worker::new(move |quit: &AtomicBool| receive_video(rout, sink_fd, &s, quit))
                };
                while !G_STOP.load(Ordering::SeqCst)
                    && enabled.load(Ordering::SeqCst)
                    && !s.over.load(Ordering::SeqCst)
                {
                    unsafe { libc::usleep(50_000) };
                }
            } // workers stopped here, before the fds close
        }
        drop(in_fd);
        drop(out_fd);

        if s.sink_closed.load(Ordering::SeqCst) {
            return 1;
        }
        failures = if s.failed.load(Ordering::SeqCst) {
            failures + 1
        } else {
            0
        };
        if failures >= MAX_CONSECUTIVE_FAILURES {
            log(format!("giving up after {} failed sessions", failures));
            return 1;
        }
        if s.failed.load(Ordering::SeqCst) {
            unsafe { libc::usleep(500_000) };
        }
    }

    log("stopped");
    0
}
