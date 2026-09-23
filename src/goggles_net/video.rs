//! Video-fragment framing and reassembly for `type=0x02` packets. Ported
//! verbatim from `../../goggles-net/src/video.rs` -- see `handshake.rs`'s
//! doc comment for why this port needed no logic changes.
//!
//! Confirmed against a real capture (1592 datagrams, 92 of them video, 18
//! complete units reassembled and `ffprobe`-validated as H.264 High Profile
//! 1440x1080@60, BT.709 -- no dropped fragments):
//!
//! ```text
//! off 0..8    generic envelope (see handshake.rs's packet_type())
//! off 8..16   packet-type-specific (unexamined -- not needed for reassembly)
//! off 16      unit_id
//! off 17      bits 0..6 = total_parts (mask 0x7f); bit 7 = part_index bit 0
//! off 18      bits 0..4 = part_index bits 1..5 (mask 0x1f)
//! off 20..    fragment payload (payload_size = packet_len - 20)
//! ```
//!
//! `part_index` is a 6-bit field split across a bit boundary:
//! `(byte17 >> 7 & 1) | ((byte18 & 0x1f) << 1)`. Concatenating every
//! fragment 0..total_parts-1 in `part_index` order yields the complete
//! Annex-B unit unchanged -- confirmed by finding `00 00 00 01 67` (SPS),
//! `00 00 00 01 68` (PPS) and `00 00 00 01 61` (slice) NAL start codes
//! right where they should be after reassembly.

const HEADER_LEN: usize = 20;

/// Parsed video-fragment header, borrowing its payload from the source packet.
pub struct Fragment<'a> {
    pub unit_id: u8,
    pub total_parts: usize,
    pub part_index: usize,
    pub payload: &'a [u8],
}

/// Parse a `type=0x02` packet's fragment header. `None` if it's too short
/// to even hold one (implausible/truncated, not a real fragment).
pub fn parse_fragment(packet: &[u8]) -> Option<Fragment<'_>> {
    if packet.len() < HEADER_LEN {
        return None;
    }
    let unit_id = packet[16];
    let total_parts = (packet[17] & 0x7f) as usize;
    let part_index = (((packet[17] >> 7) & 1) | ((packet[18] & 0x1f) << 1)) as usize;
    Some(Fragment { unit_id, total_parts, part_index, payload: &packet[HEADER_LEN..] })
}

/// Buffers fragments for the video unit currently being assembled. A
/// `unit_id` change before the previous unit completed means a fragment
/// was lost somewhere -- the partial unit is silently dropped (logged once)
/// rather than emitted corrupt, and assembly restarts on the new unit.
pub struct Assembler {
    unit_id: Option<u8>,
    total_parts: usize,
    parts: Vec<Option<Vec<u8>>>,
    have: usize,
    dropped_partial: u64,
}

impl Assembler {
    pub fn new() -> Self {
        Self { unit_id: None, total_parts: 0, parts: Vec::new(), have: 0, dropped_partial: 0 }
    }

    /// Feed one fragment. Returns the complete, ordered unit bytes once
    /// every part for its `unit_id` has arrived.
    pub fn feed(&mut self, f: &Fragment) -> Option<Vec<u8>> {
        if self.unit_id != Some(f.unit_id) {
            if self.unit_id.is_some() && self.have < self.total_parts {
                self.dropped_partial += 1;
                if self.dropped_partial == 1 {
                    crate::common::log(
                        "goggles-net: dropped a partial video unit (unit_id changed \
                         before every fragment arrived) -- further ones stay quiet",
                    );
                }
            }
            self.unit_id = Some(f.unit_id);
            self.total_parts = f.total_parts;
            self.parts = vec![None; f.total_parts];
            self.have = 0;
        }
        if f.total_parts == 0 || f.part_index >= self.parts.len() {
            return None; // implausible header -- ignore this fragment
        }
        if self.parts[f.part_index].is_none() {
            self.parts[f.part_index] = Some(f.payload.to_vec());
            self.have += 1;
        }
        if self.have < self.total_parts {
            return None;
        }
        let mut out = Vec::new();
        for part in self.parts.iter().flatten() {
            out.extend_from_slice(part);
        }
        self.unit_id = None; // ready for the next unit
        Some(out)
    }
}
