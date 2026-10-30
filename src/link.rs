//! Byte-level link to the headset, and a mock implementation for tests.
//!
//! Everything above this module (protocol, session, CLI) is transport-agnostic
//! and testable without hardware. Implementations own their [`Framer`], because
//! frame boundaries are a property of the stream they sit on.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::proto::frame::Frame;

/// A framed, bidirectional link to a headset.
pub trait Link {
    /// Write one complete frame.
    fn write_frame(&mut self, frame: &Frame) -> Result<()>;

    /// Wait for the next frame.
    ///
    /// `Ok(None)` means the timeout expired with the link still open;
    /// [`Error::Disconnected`] means the peer closed it.
    fn read_frame(&mut self, timeout: Duration) -> Result<Option<Frame>>;

    /// Human-readable description of the peer, for status output.
    fn describe(&self) -> String {
        "link".into()
    }

    /// Transport counters, for when a command goes unanswered: "0 bytes back in
    /// 5 s" and "40 bytes back, none of them frames" are different problems.
    /// Empty for transports with nothing to report.
    fn diagnostics(&self) -> String {
        String::new()
    }
}

/// Scripted link: replays canned frames and records what was sent.
///
/// Used to test session logic without a radio, and by `momentum replay` to
/// exercise the whole stack against bytes captured from a real headset.
#[derive(Debug, Default)]
pub struct MockLink {
    incoming: Vec<Frame>,
    cursor: usize,
    /// Frames written by the session, newest last.
    pub sent: Vec<Frame>,
    /// When true, reads report end-of-stream instead of buffering.
    pub closed: bool,
    /// When true, reads time out instead of returning frames.
    pub black_hole: bool,
}

impl MockLink {
    /// A link that will answer with `frames`, in order.
    pub fn replaying(frames: impl IntoIterator<Item = Frame>) -> Self {
        Self {
            incoming: frames.into_iter().collect(),
            cursor: 0,
            sent: Vec::new(),
            closed: false,
            black_hole: false,
        }
    }

    /// A link that accepts writes but never answers.
    pub fn unresponsive() -> Self {
        Self {
            black_hole: true,
            ..Default::default()
        }
    }

    /// A link whose peer has hung up.
    pub fn closed() -> Self {
        Self {
            closed: true,
            ..Default::default()
        }
    }

    /// The command ids written so far, in order.
    pub fn sent_cmds(&self) -> Vec<u16> {
        self.sent.iter().map(|f| f.cmd).collect()
    }
}

impl Link for MockLink {
    fn write_frame(&mut self, frame: &Frame) -> Result<()> {
        if self.closed {
            return Err(Error::Disconnected);
        }
        self.sent.push(frame.clone());
        Ok(())
    }

    fn read_frame(&mut self, _timeout: Duration) -> Result<Option<Frame>> {
        if self.black_hole {
            return Ok(None);
        }
        if self.closed {
            return Err(Error::Disconnected);
        }
        let frame = self.incoming.get(self.cursor).cloned();
        self.cursor += 1;
        Ok(frame)
    }

    fn describe(&self) -> String {
        "mock link".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{cmd, frame::FrameKind};

    fn reply(cmd: u16, payload: &[u8]) -> Frame {
        Frame {
            kind: FrameKind::Response,
            vendor: crate::VENDOR_QUALCOMM,
            cmd,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn records_writes_and_replays_reads() {
        let mut link = MockLink::replaying([reply(0x1B05, &[0x01])]);
        link.write_frame(&Frame::command(cmd::GET_ANC, [])).unwrap();
        let frame = link
            .read_frame(Duration::from_secs(1))
            .unwrap()
            .expect("a frame");
        assert_eq!(frame.cmd, 0x1B05);
        assert_eq!(link.sent_cmds(), vec![cmd::GET_ANC]);
        assert_eq!(
            link.read_frame(Duration::from_secs(1)).unwrap(),
            None,
            "an exhausted script reads like a timeout"
        );
    }

    #[test]
    fn closed_link_reports_disconnected() {
        let mut link = MockLink::closed();
        assert!(matches!(
            link.read_frame(Duration::from_secs(1)),
            Err(Error::Disconnected)
        ));
        assert!(matches!(
            link.write_frame(&Frame::command(cmd::GET_ANC, [])),
            Err(Error::Disconnected)
        ));
    }
}
