//! A session: subscribe to pushes, correlate replies, keep state current.
//!
//! Two rules from the captures drive the shape of this module.
//!
//! **Never trust a set-command's reply.** Setting ANC answers `0x1B04` and
//! setting transparency answers `0x1904`, both with an empty payload. They are
//! delivery acks, not state. Every setter here therefore re-reads the value it
//! just wrote, so what the app shows is what the headset believes.
//!
//! **Fold pushes in while waiting for a reply.** The headset pushes battery and
//! mode changes at any moment, including between our request and its answer.
//! Discarding those would leave the cached state stale until the next query.

use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::link::Link;
use crate::proto::cmd;
use crate::proto::frame::{
    Frame, VENDOR_QUALCOMM, VENDOR_SENNHEISER,
};
use crate::proto::state::{DeviceState, Event};

/// How long to wait for a reply before giving up. The headset answers in
/// single-digit milliseconds over a connected RFCOMM channel, so this only
/// fires when it genuinely does not support a command.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Features subscribed by [`Session::open`]: battery, noise control,
/// transparency and the user equaliser.
pub const DEFAULT_FEATURES: [u8; 4] = [
    cmd::feature::BATTERY,
    cmd::feature::ANC,
    cmd::feature::TRANSPARENCY,
    cmd::feature::USER_EQ,
];

/// Live protocol session over a [`Link`].
pub struct Session<L: Link> {
    link: L,
    pub state: DeviceState,
    /// Non-fatal problems, e.g. a command this firmware does not answer.
    pub warnings: Vec<String>,
}

impl<L: Link> Session<L> {
    pub fn new(link: L) -> Self {
        Self {
            link,
            state: DeviceState::default(),
            warnings: Vec::new(),
        }
    }

    pub fn link(&self) -> &L {
        &self.link
    }

    pub fn into_link(self) -> L {
        self.link
    }

    /// Subscribe to pushes and wait until the headset confirms.
    ///
    /// The `0x0107` ack is the signal that the control channel is really up;
    /// sending commands without it is how you end up talking to a channel that
    /// the headset has half-opened.
    pub fn open(&mut self, features: &[u8]) -> Result<()> {
        for &feature in features {
            self.link
                .write_frame(&Frame::command(cmd::REGISTER_FEATURE, [feature]))?;
        }
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        let mut acks = 0;
        while acks < features.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.link.read_frame(remaining)? {
                Some(frame) if frame.is_reply_to(cmd::REGISTER_FEATURE) => acks += 1,
                Some(frame) => self.state.apply(&Event::decode(&frame)),
                None => break,
            }
        }
        if acks == 0 {
            return Err(Error::Timeout {
                cmd: cmd::REGISTER_FEATURE,
                waited_ms: DEFAULT_TIMEOUT.as_millis(),
            });
        }
        if acks < features.len() {
            self.warnings.push(format!(
                "only {acks} of {} features were acknowledged",
                features.len()
            ));
        }
        Ok(())
    }

    /// Send a command and return its reply.
    ///
    /// The reply *and* any pushes that arrive while we wait are folded into
    /// [`Session::state`], so a query is also a state update and callers never
    /// have to remember to apply it. Empty set-acks decode as `Unknown` and so
    /// cannot corrupt state by being applied.
    pub fn request(
        &mut self,
        command: u16,
        payload: impl Into<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Frame> {
        self.request_vendor(VENDOR_QUALCOMM, command, payload, timeout)
    }

    /// Send a command in an explicit vendor namespace.
    ///
    /// A headset that does not own the vendor id does not silently ignore the
    /// frame — it answers with an [`cmd::ERROR_BIT`] frame whose payload is a
    /// GAIA error code. That becomes [`Error::Refused`] plus a warning, because
    /// treating it as a timeout would send you looking for a dead channel.
    pub fn request_vendor(
        &mut self,
        vendor: u16,
        command: u16,
        payload: impl Into<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Frame> {
        let started = Instant::now();
        self.link
            .write_frame(&Frame::command_vendor(vendor, command, payload))?;
        loop {
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(Error::Timeout {
                    cmd: command,
                    waited_ms: elapsed.as_millis(),
                });
            }
            let remaining = timeout - elapsed;
            match self.link.read_frame(remaining)? {
                Some(frame) if frame.is_reply_to(command) => {
                    self.state.apply(&Event::decode(&frame));
                    return Ok(frame);
                }
                Some(frame) if frame.is_error_for(command) => {
                    let code = frame.payload.first().copied().unwrap_or(0xFF);
                    let detail = format!(
                        "vendor {vendor:#06x} cmd {command:#06x} ({}) is not accepted: {code:#04x} = {}",
                        cmd::name(command),
                        cmd::error_name(code),
                    );
                    self.warnings.push(detail.clone());
                    return Err(Error::Refused(detail));
                }
                Some(frame) => self.state.apply(&Event::decode(&frame)),
                None => {
                    return Err(Error::Timeout {
                        cmd: command,
                        waited_ms: started.elapsed().as_millis(),
                    });
                }
            }
        }
    }

    /// The serial number, which this firmware only reveals in Sennheiser's own
    /// vendor namespace. `None` with a warning if the firmware refuses it.
    ///
    /// [`VENDOR_QUALCOMM`]: crate::VENDOR_QUALCOMM
    pub fn serial(&mut self) -> Result<Option<String>> {
        if self.state.serial.is_none() {
            let outcome = self.request_vendor(
                VENDOR_SENNHEISER,
                cmd::GET_SERIAL_NUMBER,
                [],
                DEFAULT_TIMEOUT,
            );
            if let Err(err) = outcome {
                // Already recorded as a warning; a missing serial is not a
                // reason to fail a status read.
                if !matches!(err, Error::Refused(_)) {
                    return Err(err);
                }
            }
        }
        Ok(self.state.serial.clone())
    }

    /// Write a value, then read it back so callers see confirmed state.
    fn set_and_confirm(&mut self, command: u16, value: u8, confirm: u16) -> Result<Event> {
        self.request(command, [value], DEFAULT_TIMEOUT)?;
        let reply = self.request(confirm, [], DEFAULT_TIMEOUT)?;
        Ok(Event::decode(&reply))
    }

    /// Turn active noise control on or off.
    pub fn set_anc(&mut self, on: bool) -> Result<Event> {
        self.set_and_confirm(cmd::SET_ANC, u8::from(on), cmd::GET_ANC)
    }

    /// Set the ambient / transparent-hearing level, 0..=100. `0` turns
    /// transparent hearing off.
    pub fn set_transparency(&mut self, level: u8) -> Result<Event> {
        self.set_and_confirm(
            cmd::SET_TRANSPARENCY_LEVEL,
            level.min(100),
            cmd::GET_TRANSPARENCY_LEVEL,
        )
    }

    /// Turn bass boost on or off.
    pub fn set_bass_boost(&mut self, on: bool) -> Result<Event> {
        self.set_and_confirm(cmd::SET_BASS_BOOST, u8::from(on), cmd::GET_BASS_BOOST)
    }

    /// Query everything we display. Commands this firmware does not answer
    /// become warnings instead of failing the whole refresh, because the
    /// command set differs between MOMENTUM models.
    pub fn refresh(&mut self) -> Result<()> {
        let queries = [
            ("noise control", cmd::GET_ANC),
            ("ambient level", cmd::GET_TRANSPARENCY_LEVEL),
            ("bass boost", cmd::GET_BASS_BOOST),
            ("battery", cmd::GET_BATTERY),
        ];
        for (label, command) in queries {
            match self.request(command, [], DEFAULT_TIMEOUT) {
                // request already folded the reply into state.
                Ok(_) => {}
                Err(Error::Timeout { cmd, .. }) => {
                    self.warnings
                        .push(format!("{label} ({cmd:#06x}) was not answered"));
                }
                Err(other) => return Err(other),
            }
        }
        Ok(())
    }

    /// Wait for one push from the headset. `None` means the timeout passed
    /// with nothing happening. Drives `momentum watch`.
    pub fn pump(&mut self, timeout: Duration) -> Result<Option<Event>> {
        match self.link.read_frame(timeout)? {
            Some(frame) => {
                let event = Event::decode(&frame);
                self.state.apply(&event);
                Ok(Some(event))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::MockLink;
    use crate::proto::frame::FrameKind;
    use crate::proto::state::{Battery, Mode};

    fn reply(c: u16, payload: &[u8]) -> Frame {
        Frame {
            kind: FrameKind::Response,
            vendor: crate::VENDOR_QUALCOMM,
            cmd: c,
            payload: payload.to_vec(),
        }
    }

    fn ack() -> Frame {
        reply(cmd::reply_to(cmd::REGISTER_FEATURE), &[])
    }

    #[test]
    fn open_registers_features_and_waits_for_acks() {
        let mut session = Session::new(MockLink::replaying([ack(), ack()]));
        session
            .open(&[cmd::feature::BATTERY, cmd::feature::ANC])
            .expect("session opens once acked");
        assert_eq!(
            session.link().sent_cmds(),
            vec![cmd::REGISTER_FEATURE, cmd::REGISTER_FEATURE]
        );
    }

    #[test]
    fn open_fails_without_any_ack() {
        let mut session = Session::new(MockLink::unresponsive());
        let err = session
            .open(&[cmd::feature::ANC])
            .expect_err("no ack means no session");
        assert!(matches!(err, Error::Timeout { cmd: 0x0007, .. }));
    }

    #[test]
    fn set_anc_reads_the_value_back_instead_of_trusting_the_ack() {
        // The script: ack of the set (empty, carries no state), then the reply
        // to the follow-up query.
        let link = MockLink::replaying([reply(cmd::reply_to(cmd::SET_ANC), &[]), reply(0x1B05, &[0x01])]);
        let mut session = Session::new(link);
        let event = session.set_anc(true).expect("set succeeds");
        assert_eq!(event, Event::Anc(true));
        assert_eq!(session.state.anc, Some(true));
        assert_eq!(
            session.link().sent_cmds(),
            vec![cmd::SET_ANC, cmd::GET_ANC],
            "a setter must verify what it wrote"
        );
    }

    #[test]
    fn pushes_arriving_mid_request_are_not_dropped() {
        // Battery push lands between our query and its reply.
        let link = MockLink::replaying([reply(0x0683, &[0x50, 0x50, 0x3C]), reply(0x1B05, &[0x00])]);
        let mut session = Session::new(link);
        session
            .request(cmd::GET_ANC, [], DEFAULT_TIMEOUT)
            .expect("reply found after the push");
        assert_eq!(
            session.state.battery,
            Some(Battery {
                slots: [Some(80), Some(80), Some(60)]
            }),
            "the interleaved push should have been folded into state"
        );
        assert_eq!(session.state.anc, Some(false));
    }

    #[test]
    fn unresponsive_command_times_out() {
        let mut session = Session::new(MockLink::unresponsive());
        let err = session
            .request(cmd::GET_BATTERY, [], Duration::from_millis(5))
            .expect_err("expected a timeout");
        assert!(matches!(err, Error::Timeout { cmd: 0x0603, .. }));
    }

    /// Live: `0x0007` sent under the Sennheiser vendor id comes back as
    /// `0x0187 [05]`, the command with both bits set and a GAIA error code.
    #[test]
    fn a_refusal_is_reported_as_refused_not_as_a_timeout() {
        let link = MockLink::replaying([reply(cmd::error_for(cmd::REGISTER_FEATURE), &[0x05])]);
        let mut session = Session::new(link);
        let err = session
            .request_vendor(
                crate::proto::frame::VENDOR_SENNHEISER,
                cmd::REGISTER_FEATURE,
                [cmd::feature::ANC],
                DEFAULT_TIMEOUT,
            )
            .expect_err("a refusal is an answer, not a silence");
        assert!(matches!(err, Error::Refused(_)), "got {err:?}");
        assert!(
            session.warnings.iter().any(|w| w.contains("not supported")),
            "the decoded reason should be recorded, got {:?}",
            session.warnings
        );
    }

    #[test]
    fn refresh_degrades_to_warnings_for_unknown_commands() {
        // Only noise control is answered; the rest of the command set is
        // assumed absent on this firmware.
        let link = MockLink::replaying([reply(0x1B05, &[0x01])]);
        let mut session = Session::new(link);
        session.refresh().expect("refresh survives missing commands");
        assert_eq!(
            session.state.anc,
            Some(true),
            "the one command that answered must still update state"
        );
        assert_eq!(session.state.mode(), Mode::Anc);
        assert_eq!(
            session.warnings.len(),
            3,
            "ambient, bass and battery should warn: {:?}",
            session.warnings
        );
    }

    #[test]
    fn refresh_propagates_a_dead_link() {
        let mut session = Session::new(MockLink::closed());
        assert!(matches!(
            session.refresh(),
            Err(Error::Disconnected) | Err(Error::Timeout { .. })
        ));
    }

    #[test]
    fn pump_reports_pushes_and_idle_separately() {
        let mut session = Session::new(MockLink::replaying([reply(0x1A85, &[0x01])]));
        assert_eq!(session.pump(DEFAULT_TIMEOUT).unwrap(), Some(Event::Anc(true)));
        assert_eq!(session.pump(DEFAULT_TIMEOUT).unwrap(), None);
        assert_eq!(session.state.anc, Some(true));
    }

    #[test]
    fn transparency_is_clamped_to_a_sane_level() {
        let link = MockLink::replaying([
            reply(cmd::reply_to(cmd::SET_TRANSPARENCY_LEVEL), &[]),
            reply(0x1B03, &[100]),
        ]);
        let mut session = Session::new(link);
        session.set_transparency(250).expect("set");
        assert_eq!(
            session.link().sent.first().unwrap().payload,
            vec![100],
            "the headset takes 0..=100; clamp rather than send nonsense"
        );
        assert_eq!(session.state.mode(), Mode::Transparency);
    }
}
