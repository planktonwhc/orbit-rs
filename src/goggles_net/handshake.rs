//! The real goggles-net wire protocol -- **confirmed against real hardware**
//! (a captured, `ffprobe`-validated H.264 stream: 1440x1080, High Profile,
//! 60fps, BT.709). Nothing here is a guess. Ported near-verbatim from
//! `../../goggles-net/src/handshake.rs` (the standalone project this mode
//! was folded in from -- see `goggles_net/mod.rs`'s doc comment); this file
//! is untouched byte-for-byte logic, only the `log` import target changed
//! (orbit-rs's own `common::log` has the identical signature).
//!
//! It has nothing to do with DUML or `logiclink` (both explored and ruled
//! out for this transport -- see `../../goggles-net/README.md` and
//! `references/dji-rs-flow.md`). It's a small, self-contained session
//! protocol:
//!
//! ```text
//! generic packet envelope (every packet on the wire, both directions):
//!   off 0..2   declared length, u16 LE, top bit reserved (mask 0x7fff)
//!   off 2..4   session id, u16 LE
//!   off 4..6   reserved / packet-type-specific
//!   off 6      packet type (0x01 = upstream control, 0x02 = video, ...)
//!   off 7      checksum: XOR of bytes 0..7
//! ```
//!
//! **Opener** (48 bytes, us -> goggles): fixed type marker `0x8030` LE at
//! offset 0 (not a "declared length" for this one), our session id, a
//! sequence (masked to a multiple of 8), then a fixed 38-byte capability
//! payload copied verbatim from a real capture. Resent every 250ms until
//! **ACK'd** (exactly 9 bytes back, echoing our session id) -- that's
//! "opener ACK ... wired session is live".
//!
//! **Control ACK** (34 bytes, us -> goggles): every upstream `type=0x01`
//! control packet from the goggles must be echoed back as a `type=0x04`
//! packet (type marker `0x8022` LE), bytes 8..33 copied verbatim from what
//! was received (they carry the goggles' own sequence/state), checksum
//! recomputed. Without this ACK the goggles apparently stop advancing --
//! this is the actual "keepalive", not the AOA-transport trigger frames an
//! earlier attempt (wrongly) reused.
//!
//! **Video** (`type=0x02`): see `video.rs` for the fragment header at
//! offset 8..20 and reassembly.

use std::fs::File;
use std::io::{self, Read};
use std::net::UdpSocket;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::common::log;

pub const OPENER_LEN: usize = 48;

const OPENER_PAYLOAD: [u8; 38] = [
    0x64, 0x00, 0x64, 0x00, 0xc0, 0x05, 0x14, 0x00,
    0x00, 0x64, 0x00, 0x00, 0x01, 0x90, 0x01, 0xc0,
    0x05, 0x14, 0x00, 0x00, 0x64, 0x00, 0x14, 0x00,
    0x64, 0x00, 0xc0, 0x05, 0x14, 0x00, 0x00, 0x64,
    0x00, 0x01, 0x01, 0x04, 0x01, 0x02,
];

pub struct Session {
    pub id: u16,
    pub sequence: u16,
}

pub fn build_opener(session: u16, sequence: u16) -> [u8; OPENER_LEN] {
    let mut packet = [0u8; OPENER_LEN];
    let sid = session.to_le_bytes();
    let seq = (sequence & 0xfff8).to_le_bytes();

    packet[0..2].copy_from_slice(&0x8030u16.to_le_bytes());
    packet[2..4].copy_from_slice(&sid);
    packet[7] = sid[0] ^ sid[1] ^ 0xb0;
    packet[8..10].copy_from_slice(&seq);
    packet[10..].copy_from_slice(&OPENER_PAYLOAD);
    packet
}

pub fn is_opener_ack(packet: &[u8], session: u16) -> bool {
    packet.len() == 9 && packet[2..4] == session.to_le_bytes()
}

/// Validate a packet's generic envelope (declared length + checksum +
/// session match) and return its type byte if it checks out.
pub fn packet_type(packet: &[u8], session: u16) -> Option<u8> {
    if packet.len() < 8 || packet[2..4] != session.to_le_bytes() {
        return None;
    }

    let declared = u16::from_le_bytes([packet[0], packet[1]]) & 0x7fff;
    if usize::from(declared) != packet.len() || checksum7(packet) != packet[7] {
        return None;
    }
    Some(packet[6])
}

/// Build the fixed 34-byte transport ACK used for upstream control packets.
/// Bytes 8..33 carry the sequence/state fields from the received header.
pub fn build_control_ack(packet: &[u8], session: u16) -> Option<[u8; 34]> {
    if packet_type(packet, session) != Some(0x01) || packet.len() < 34 {
        return None;
    }

    let mut ack = [0u8; 34];
    ack.copy_from_slice(&packet[..34]);
    ack[0..2].copy_from_slice(&0x8022u16.to_le_bytes());
    ack[4] = 0;
    ack[5] = 0;
    ack[6] = 0x04;
    ack[7] = checksum7(&ack);
    Some(ack)
}

fn checksum7(packet: &[u8]) -> u8 {
    packet[..7].iter().fold(0u8, |sum, byte| sum ^ byte)
}

/// Send the opener every 250ms until a matching ACK arrives, or `timeout`
/// runs out.
pub fn open(sock: &UdpSocket, timeout: Duration) -> io::Result<Session> {
    let (session, sequence) = random_session_and_sequence()?;
    let opener = build_opener(session, sequence);
    let deadline = Instant::now() + timeout;
    let retry = Duration::from_millis(250);
    let mut buf = [0u8; 65536];
    let mut attempts = 0u32;

    sock.set_read_timeout(Some(retry))?;
    log(format!(
        "goggles-net: opener session=0x{session:04x} seq=0x{sequence:04x} ({} bytes)",
        opener.len()
    ));

    while Instant::now() < deadline {
        attempts += 1;
        sock.send(&opener)?;

        loop {
            match sock.recv(&mut buf) {
                Ok(n) => {
                    let packet = &buf[..n];
                    if is_opener_ack(packet, session) {
                        sock.set_read_timeout(None)?;
                        log(format!(
                            "goggles-net: opener ACK after {attempts} attempt(s); wired session is live"
                        ));
                        return Ok(Session { id: session, sequence });
                    }
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no opener ACK within {} ms ({attempts} attempts)", timeout.as_millis()),
    ))
}

pub fn hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(data.len() * 3);
    for (i, byte) in data.iter().enumerate() {
        if i != 0 {
            result.push(' ');
        }
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn random_session_and_sequence() -> io::Result<(u16, u16)> {
    let mut bytes = [0u8; 4];
    match File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)) {
        Ok(()) => {}
        Err(_) => {
            let n = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
                ^ u64::from(std::process::id());
            bytes.copy_from_slice(&(n as u32).to_le_bytes());
        }
    }

    let mut session = u16::from_le_bytes([bytes[0], bytes[1]]);
    if session == 0 {
        session = 1;
    }
    let sequence = u16::from_le_bytes([bytes[2], bytes[3]]) & 0xfff8;
    Ok((session, sequence))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_opener() {
        let packet = build_opener(0x1234, 0x1000);
        assert_eq!(packet.len(), 48);
        assert_eq!(
            hex(&packet),
            "30 80 34 12 00 00 00 96 00 10 64 00 64 00 c0 05 14 00 00 64 00 00 01 90 01 c0 05 14 00 00 64 00 14 00 64 00 c0 05 14 00 00 64 00 01 01 04 01 02"
        );
    }

    #[test]
    fn ack_matches_length_and_session() {
        assert!(is_opener_ack(&[0x09, 0x80, 0x34, 0x12, 0, 0, 0, 0, 0], 0x1234));
        assert!(!is_opener_ack(&[0x09, 0x80, 0x35, 0x12, 0, 0, 0, 0, 0], 0x1234));
    }

    #[test]
    fn control_ack_from_real_capture() {
        let rx = hex_decode(
            "22 80 f5 71 00 00 01 27 00 66 00 66 00 00 00 00 00 66 00 66 00 00 00 00 00 66 00 66 00 00 00 00 00 00",
        );
        let ack = build_control_ack(&rx, 0x71f5).unwrap();
        assert_eq!(
            hex(&ack),
            "22 80 f5 71 00 00 04 22 00 66 00 66 00 00 00 00 00 66 00 66 00 00 00 00 00 66 00 66 00 00 00 00 00 00"
        );
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|part| u8::from_str_radix(part, 16).unwrap())
            .collect()
    }
}
