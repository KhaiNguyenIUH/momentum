//! Frame encoding and stream reassembly.
//!
//! ```text
//! offset  size  field
//! 0       1     0xFF sync
//! 1       1     kind: 0x03 command, 0x04 response/notification
//! 2       1     0x00 reserved
//! 3       1     payload length (payload only, header excluded)
//! 4       2     vendor id, big endian (0x0495 Qualcomm)
//! 6       2     command id, big endian
//! 8       n     payload
//! ```
//!
//! There is no application-layer checksum: the trailing FCS-8 belongs to the
//! RFCOMM layer and is added/verified by the operating system's Bluetooth
//! stack, not by us.
//!
//! Replies and notifications are *not* distinguished by a flag. They are the
//! originating command with bits folded in:
//!
//! * reply to `cmd` is `cmd | 0x0100`
//! * notification for `cmd` is `cmd | 0x0080`
//!
//! e.g. `0x0603` (query battery) is answered by `0x0703` and pushed as
//! `0x0683`.

use crate::proto::cmd;

/// Start-of-frame marker.
pub const SYNC: u8 = 0xFF;
/// Qualcomm vendor id carried in every frame.
pub const VENDOR_QUALCOMM: u16 = 0x0495;
/// Sennheiser's own vendor id (Bluetooth SIG company id 29). Most commands are
/// Qualcomm's, but a few — the serial number among them, live-confirmed — only
/// answer under this one, and everything else sent this way is refused with an
/// [`crate::proto::cmd::ERROR_BIT`] frame.
pub const VENDOR_SENNHEISER: u16 = 0x001D;
/// Fixed header size in bytes, payload excluded.
pub const HEADER_LEN: usize = 8;
/// Longest payload a single frame can describe (the length field is one byte).
pub const MAX_PAYLOAD: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Written by the host.
    Command = 0x03,
    /// Sent by the headset, either as a reply or as an unsolicited push.
    Response = 0x04,
}

impl FrameKind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x03 => Some(FrameKind::Command),
            0x04 => Some(FrameKind::Response),
            _ => None,
        }
    }
}

/// One decoded protocol frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub vendor: u16,
    /// Raw command id as seen on the wire, reply/notification bits included.
    pub cmd: u16,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Build a host-to-headset command.
    pub fn command(cmd: u16, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: FrameKind::Command,
            vendor: VENDOR_QUALCOMM,
            cmd,
            payload: payload.into(),
        }
    }

    /// Build a command in another vendor namespace, e.g. [`VENDOR_SENNHEISER`].
    pub fn command_vendor(vendor: u16, cmd: u16, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: FrameKind::Command,
            vendor,
            cmd,
            payload: payload.into(),
        }
    }

    /// True when the headset used this frame to refuse a command; the payload's
    /// first byte is then a [`crate::proto::cmd::error_name`] code.
    pub fn is_error_for(&self, request: u16) -> bool {
        self.cmd == cmd::error_for(request)
    }

    /// True when this frame is the reply the headset sends for `request`.
    pub fn is_reply_to(&self, request: u16) -> bool {
        self.cmd == cmd::reply_to(request)
    }

    /// True when this frame is the unsolicited push belonging to `request`.
    pub fn is_notification_for(&self, request: u16) -> bool {
        self.cmd == cmd::notification_of(request)
    }

    /// The command id this frame refers to with reply/notification bits
    /// removed, which is how the tables in [`crate::proto::cmd`] are indexed.
    pub fn base_cmd(&self) -> u16 {
        cmd::base_of(self.cmd)
    }

    /// Serialise back to wire form.
    ///
    /// # Panics
    /// If `payload.len()` exceeds [`MAX_PAYLOAD`]; the one-byte length field
    /// cannot express more. Callers should segment instead.
    pub fn encode(&self) -> Vec<u8> {
        assert!(
            self.payload.len() <= MAX_PAYLOAD,
            "payload of {} bytes does not fit one frame (max {MAX_PAYLOAD}); segment it",
            self.payload.len()
        );
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(SYNC);
        out.push(self.kind as u8);
        out.push(0x00);
        out.push(self.payload.len() as u8);
        out.extend_from_slice(&self.vendor.to_be_bytes());
        out.extend_from_slice(&self.cmd.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Reassembles frames out of the RFCOMM byte stream.
///
/// A single `recv()` can deliver several frames glued together, half a frame,
/// or a frame with stray bytes in front of it. Bytes that cannot belong to a
/// frame are counted in [`Framer::dropped_bytes`] and skipped, which keeps the
/// decoder self-healing after a desync instead of wedged on bad data.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
    dropped: usize,
}

impl Framer {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            dropped: 0,
        }
    }

    /// Queue bytes received from the socket.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Number of bytes discarded while searching for a frame boundary.
    pub fn dropped_bytes(&self) -> usize {
        self.dropped
    }

    /// Bytes currently buffered but not yet part of a complete frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Pull the next complete frame, or `None` when more bytes are needed.
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            // Discard everything ahead of the next sync byte.
            match self.buf.iter().position(|&b| b == SYNC) {
                Some(0) => {}
                Some(offset) => {
                    self.dropped += offset;
                    self.buf.drain(..offset);
                }
                None => {
                    self.dropped += self.buf.len();
                    self.buf.clear();
                    return None;
                }
            }

            if self.buf.len() < 4 {
                return None;
            }
            let Some(kind) = FrameKind::from_u8(self.buf[1]) else {
                // A 0xFF followed by something that is not a kind byte is
                // payload or noise: skip just the marker and resync. Waiting
                // for more bytes here would wedge the decoder.
                self.dropped += 1;
                self.buf.drain(..1);
                continue;
            };
            if self.buf[2] != 0x00 {
                // Same for a non-zero reserved byte.
                self.dropped += 1;
                self.buf.drain(..1);
                continue;
            }

            let payload_len = self.buf[3] as usize;
            let total = HEADER_LEN + payload_len;
            if self.buf.len() < total {
                return None;
            }

            let vendor = u16::from_be_bytes([self.buf[4], self.buf[5]]);
            let command = u16::from_be_bytes([self.buf[6], self.buf[7]]);
            let payload = self.buf[HEADER_LEN..total].to_vec();
            self.buf.drain(..total);

            return Some(Frame {
                kind,
                vendor,
                cmd: command,
                payload,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::cmd;

    // Verbatim frames from the MOMENTUM True Wireless 4 btsnoop captures.
    const TX_QUERY_BATTERY: &[u8] = &[0xFF, 0x03, 0x00, 0x00, 0x04, 0x95, 0x06, 0x03];
    const TX_REGISTER_BATTERY: &[u8] = &[0xFF, 0x03, 0x00, 0x01, 0x04, 0x95, 0x00, 0x07, 0x03];
    const RX_BATTERY_PUSH: &[u8] = &[
        0xFF, 0x04, 0x00, 0x03, 0x04, 0x95, 0x06, 0x83, 0x50, 0x50, 0x3C,
    ];
    const RX_REGISTER_ACK: &[u8] = &[0xFF, 0x04, 0x00, 0x00, 0x04, 0x95, 0x01, 0x07];

    fn decode_one(bytes: &[u8]) -> Frame {
        let mut f = Framer::new();
        f.push(bytes);
        let frame = f.next_frame().expect("frame");
        assert_eq!(f.next_frame(), None, "should have consumed everything");
        assert_eq!(f.dropped_bytes(), 0);
        frame
    }

    #[test]
    fn encodes_query_battery_exactly_as_captured() {
        assert_eq!(
            Frame::command(cmd::GET_BATTERY, []).encode(),
            TX_QUERY_BATTERY
        );
    }

    #[test]
    fn encodes_feature_registration_exactly_as_captured() {
        assert_eq!(
            Frame::command(cmd::REGISTER_FEATURE, [cmd::feature::BATTERY]).encode(),
            TX_REGISTER_BATTERY
        );
    }

    #[test]
    fn decodes_battery_push_with_three_slots() {
        let f = decode_one(RX_BATTERY_PUSH);
        assert_eq!(f.kind, FrameKind::Response);
        assert_eq!(f.vendor, VENDOR_QUALCOMM);
        assert_eq!(f.cmd, cmd::notification_of(cmd::GET_BATTERY));
        assert_eq!(f.base_cmd(), cmd::GET_BATTERY);
        assert!(f.is_notification_for(cmd::GET_BATTERY));
        assert_eq!(f.payload, vec![0x50, 0x50, 0x3C]);
    }

    #[test]
    fn decodes_register_ack() {
        let f = decode_one(RX_REGISTER_ACK);
        assert_eq!(f.cmd, cmd::reply_to(cmd::REGISTER_FEATURE));
        assert!(f.is_reply_to(cmd::REGISTER_FEATURE));
        assert!(f.payload.is_empty());
    }

    #[test]
    fn reply_and_notification_bits_do_not_collide() {
        for c in [
            cmd::GET_BATTERY,
            cmd::GET_ANC,
            cmd::SET_ANC,
            cmd::GET_TRANSPARENCY_LEVEL,
        ] {
            assert_ne!(cmd::reply_to(c), cmd::notification_of(c));
            assert_eq!(cmd::base_of(cmd::reply_to(c)), c);
            assert_eq!(cmd::base_of(cmd::notification_of(c)), c);
        }
        // The concrete pairs named in the reverse-engineering notes.
        assert_eq!(cmd::reply_to(0x0603), 0x0703);
        assert_eq!(cmd::notification_of(0x0603), 0x0683);
        assert_eq!(cmd::reply_to(0x1A05), 0x1B05);
        assert_eq!(cmd::notification_of(0x1A05), 0x1A85);
    }

    #[test]
    fn reassembles_frames_split_across_reads() {
        let mut f = Framer::new();
        let (head, last) = RX_BATTERY_PUSH.split_at(RX_BATTERY_PUSH.len() - 1);
        for byte in head {
            f.push(std::slice::from_ref(byte));
            assert_eq!(f.next_frame(), None, "an incomplete frame must not surface");
        }
        f.push(last);
        assert_eq!(f.next_frame().unwrap().cmd, 0x0683);
        assert_eq!(f.dropped_bytes(), 0);
    }

    #[test]
    fn reassembles_frames_arriving_in_three_byte_chunks() {
        // RFCOMM delivers whatever fits the MTU, so a frame routinely spans
        // several reads in awkward pieces.
        let mut f = Framer::new();
        let mut seen = 0;
        for chunk in TX_REGISTER_BATTERY.chunks(3) {
            f.push(chunk);
            while f.next_frame().is_some() {
                seen += 1;
            }
        }
        assert_eq!(seen, 1);
        assert_eq!(f.dropped_bytes(), 0);
    }

    #[test]
    fn splits_frames_glued_into_one_read() {
        let mut f = Framer::new();
        f.push(TX_QUERY_BATTERY);
        f.push(RX_REGISTER_ACK);
        f.push(RX_BATTERY_PUSH);
        assert_eq!(f.next_frame().unwrap().cmd, cmd::GET_BATTERY);
        assert_eq!(f.next_frame().unwrap().cmd, 0x0107);
        assert_eq!(f.next_frame().unwrap().cmd, 0x0683);
        assert_eq!(f.next_frame(), None);
        assert_eq!(f.dropped_bytes(), 0);
    }

    #[test]
    fn resynchronises_after_stray_bytes_and_recovers() {
        let mut f = Framer::new();
        f.push(&[0x11, 0x22, 0xFF]); // noise, then a 0xFF that is not a frame
        f.push(&[0x77, 0x88]); // makes the previous 0xFF bogus
        f.push(RX_REGISTER_ACK);
        let frame = f.next_frame().unwrap();
        assert_eq!(frame.cmd, 0x0107);
        assert!(f.dropped_bytes() > 0);
    }

    #[test]
    fn treats_zero_ff_inside_payload_as_payload() {
        // 0xFF inside a payload must not start a new frame.
        let raw = Frame::command(0x1A81, [0x01, 0x00, 0xFF, 0x00, 0x03, 0x00]).encode();
        let mut f = Framer::new();
        f.push(&raw);
        let frame = f.next_frame().unwrap();
        assert_eq!(frame.payload, vec![0x01, 0x00, 0xFF, 0x00, 0x03, 0x00]);
        assert_eq!(f.next_frame(), None);
        assert_eq!(f.dropped_bytes(), 0);
    }

    #[test]
    fn handles_maximum_payload() {
        let payload = vec![0xA5; MAX_PAYLOAD];
        let raw = Frame::command(0x2C83, payload.clone()).encode();
        assert_eq!(raw.len(), HEADER_LEN + MAX_PAYLOAD);
        let f = decode_one(&raw);
        assert_eq!(f.payload, payload);
    }

    #[test]
    fn round_trips_every_frame_kind() {
        for c in [cmd::GET_ANC, cmd::SET_ANC, cmd::REGISTER_FEATURE] {
            for payload in [vec![], vec![0x01], vec![0x00; 17]] {
                let frame = Frame {
                    kind: FrameKind::Command,
                    vendor: VENDOR_QUALCOMM,
                    cmd: c,
                    payload,
                };
                assert_eq!(decode_one(&frame.encode()), frame);
            }
        }
    }

    #[test]
    fn buffers_are_reported_for_diagnostics() {
        let mut f = Framer::new();
        f.push(&RX_BATTERY_PUSH[..5]);
        assert_eq!(f.next_frame(), None);
        assert_eq!(f.buffered(), 5);
    }
}
