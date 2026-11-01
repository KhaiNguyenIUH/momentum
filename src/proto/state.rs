//! Decoding frames into device state.
//!
//! Two firmware behaviours found in the captures shape this module and are
//! worth repeating because they are easy to get wrong:
//!
//! 1. **Set-commands answer with an empty ack that carries no state.** The
//!    ack for setting ANC (`0x1B04`) and for setting transparency (`0x1904`)
//!    says only "received". Never infer a mode from them.
//! 2. **ANC and transparency are physically exclusive, but the headset does
//!    not always push both.** Turning transparency on does *not* push
//!    `anc_push(false)`. So the two states are tracked independently and never
//!    overwrite each other; a third mode ("off") is simply both being false.

use crate::proto::cmd;
use crate::proto::frame::Frame;

/// A slot that reports no value right now.
const UNKNOWN: u8 = 0xFF;

/// Battery levels for the reported slots.
///
/// On true-wireless models the three slots are left, right and case. On the
/// over-ear MOMENTUM 5 the headset is a single unit, so slot 0 holds the
/// headset level and the rest are usually [`UNKNOWN`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Battery {
    pub slots: [Option<u8>; 3],
}

impl Battery {
    pub fn from_payload(payload: &[u8]) -> Self {
        let mut slots = [None; 3];
        for (slot, &byte) in slots.iter_mut().zip(payload) {
            *slot = if byte == UNKNOWN { None } else { Some(byte) };
        }
        Self { slots }
    }

    /// The level a headphone with one battery would report.
    pub fn primary(&self) -> Option<u8> {
        self.slots[0]
    }
}

/// In-case / charging flags per unit. `Some(true)` means charging in the case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Charging {
    pub slots: [Option<bool>; 2],
}

impl Charging {
    pub fn from_payload(payload: &[u8]) -> Self {
        let mut slots = [None; 2];
        for (slot, &byte) in slots.iter_mut().zip(payload) {
            *slot = match byte {
                UNKNOWN => None,
                0x00 => Some(false),
                _ => Some(true),
            };
        }
        Self { slots }
    }
}

/// The equaliser tables, as the headset reports them.
///
/// [`Equaliser::gains_db`] is the calibrated field: ten raw units per decibel,
/// established against the app's own slider labels. The other three tables describe a
/// **bank of filters**, not the seven sliders: a centre frequency, a Q and a filter type
/// per band, all three writable. They are named for what they are, which is recent — an
/// earlier version of this struct called the Q `q` and the filter type `limit`,
/// and the document that recorded ±14 dB as a range was reading a filter type as a
/// decibel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Equaliser {
    /// One signed byte per band, the compact form. All zero means flat.
    pub gains: Vec<i8>,
    /// Centre frequency per band, hertz, from `0x100b`: 90, 325, 1500, 2500, 4500,
    /// 8000, 13000, 16000 on a MOMENTUM 5. **Not** the app's slider labels, which are
    /// 50 Hz to 12 kHz — see [`APP_BANDS_HZ`]. This is the bank the DSP describes, and
    /// `0x100a` writes it.
    pub hz: Vec<Option<u16>>,
    /// Q per band from `0x100d`, in **Q12**: divide by 4096. `0x0800` is 0.5 and
    /// `0x0b5c` is 0.71, which is 1/√2 — the Butterworth value. Sennheiser's own
    /// encoder multiplies Q by 4096, which is what settled the scale.
    pub q12: Vec<Option<u16>>,
    /// Filter type per band from `0x100f`. `14` on every band of a MOMENTUM 5. An
    /// enumeration nobody has published, so it stays an integer.
    pub filter_types: Vec<Option<u16>>,
    /// Per-band gain in the wide form, `0x1011`.
    pub gains_wide: Vec<Option<u16>>,
    /// The two-byte row of `0x1013`, kept verbatim.
    pub state: Vec<u8>,
}

/// The centre frequencies Smart Control's graphic EQ offers, ascending.
///
/// Observed in the app, not read from the wire, and matched to band order — the
/// app lists eight bands and the headset stores eight gains. That is why
/// [`Equaliser::hz`] is not this: the device's own bank is 90, 325, 1500, 2500, 4500,
/// 8000, 13000, 16000. Whether the app's labels are a naming of the same eight
/// filters or a separate UI fiction has never been established.
pub const APP_BANDS_HZ: [u16; 8] = [50, 100, 250, 600, 1400, 3000, 7000, 12000];

impl Equaliser {
    /// How many bands the headset has described so far.
    pub fn bands(&self) -> usize {
        [
            self.gains.len(),
            self.hz.len(),
            self.q12.len(),
            self.filter_types.len(),
            self.gains_wide.len(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }

    /// The stored gains in **decibels**: ten raw units per dB.
    ///
    /// Not an inference. Setting every band to −6 dB in Smart Control and reading
    /// the table gave −60 on all eight bands, and flat reads 0. That also fixes
    /// the granularity of a slider drag at 0.1 dB, which is what the single-unit
    /// steps in a capture looked like.
    ///
    /// This applies to the signed-byte store only. The u16 table bounds at ±180
    /// rather than the ±60 this scale implies for a ±6 dB slider, so it is not
    /// covered by it and is deliberately not converted.
    pub fn gains_db(&self) -> Vec<f32> {
        self.gains
            .iter()
            .map(|&gain| f32::from(gain) / 10.0)
            .collect()
    }

    /// True when no table has arrived, which is how a caller tells "flat" from
    /// "never asked".
    pub fn is_empty(&self) -> bool {
        self.bands() == 0 && self.state.is_empty()
    }
}

/// Parse indexed rows: a band number followed by `width` big-endian bytes.
///
/// Rows land at the index the headset names rather than in arrival order, so a
/// firmware that reports three of eight bands leaves the other five as `None`
/// instead of shifting everything left.
fn rows(payload: &[u8], width: usize) -> Vec<Option<u16>> {
    let stride = width + 1;
    let mut out = Vec::new();
    for row in payload.chunks(stride) {
        if row.len() < stride {
            break; // a partial row describes no band
        }
        let band = usize::from(row[0]);
        let mut value: u16 = 0;
        for &byte in &row[1..] {
            value = (value << 8) | u16::from(byte);
        }
        if out.len() <= band {
            out.resize(band + 1, None);
        }
        out[band] = Some(value);
    }
    out
}

/// A state change reported by the headset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Battery(Battery),
    Charging(Charging),
    Anc(bool),
    /// Ambient sound level, 0..=100. Non-zero means transparency is active.
    TransparencyLevel(u8),
    TransparencyOn(bool),
    BassBoost(bool),
    /// Ack that pushes for a feature were subscribed.
    FeatureRegistered(u8),
    /// The serial number, ASCII digits, as reported under the Sennheiser vendor
    /// namespace. (live on MOMENTUM 5)
    Serial(String),
    /// The product name, e.g. `MOMENTUM 5`. Same namespace. (live)
    Product(String),
    /// Firmware version and build stamp, e.g.
    /// `6.21.1 / 2026-05-20 06:46:42+00:00`. Same namespace. (live)
    Firmware(String),
    /// One of the equaliser tables, pushed when feature `0x08` is registered or
    /// read back per band. The variant names the table because the ids do not
    /// share a payload shape. (live)
    EqGains(Vec<i8>),
    /// See [`Event::EqGains`]. Values are hertz.
    EqRow100b(Vec<Option<u16>>),
    /// See [`Event::EqGains`]. Unit unidentified.
    EqBandValues(Vec<Option<u16>>),
    /// See [`Event::EqGains`].
    EqLimits(Vec<Option<u16>>),
    /// See [`Event::EqGains`].
    EqGainsWide(Vec<Option<u16>>),
    /// See [`Event::EqGains`]. Kept raw.
    EqState(Vec<u8>),
    /// Anything not yet understood. Kept verbatim so captures can be studied.
    Unknown {
        cmd: u16,
        payload: Vec<u8>,
    },
}

impl Event {
    /// Interpret one frame received from the headset.
    pub fn decode(frame: &Frame) -> Event {
        let flag = |payload: &[u8]| payload.first().copied().unwrap_or(0) != 0;
        match frame.cmd {
            c if c == cmd::reply_to(cmd::REGISTER_FEATURE) => {
                Event::FeatureRegistered(frame.payload.first().copied().unwrap_or(0))
            }
            c if c == cmd::reply_to(cmd::GET_BATTERY)
                || c == cmd::notification_of(cmd::GET_BATTERY) =>
            {
                Event::Battery(Battery::from_payload(&frame.payload))
            }
            c if c == cmd::reply_to(cmd::GET_CHARGING)
                || c == cmd::notification_of(cmd::GET_CHARGING) =>
            {
                Event::Charging(Charging::from_payload(&frame.payload))
            }
            c if c == cmd::reply_to(cmd::GET_ANC) || c == cmd::notification_of(cmd::GET_ANC) => {
                Event::Anc(flag(&frame.payload))
            }
            cmd::PUSH_TRANSPARENCY_LEVEL => {
                Event::TransparencyLevel(frame.payload.first().copied().unwrap_or(0))
            }
            cmd::PUSH_TRANSPARENCY_ON => Event::TransparencyOn(flag(&frame.payload)),
            c if c == cmd::reply_to(cmd::GET_TRANSPARENCY_LEVEL) => {
                Event::TransparencyLevel(frame.payload.first().copied().unwrap_or(0))
            }
            c if c == cmd::reply_to(cmd::GET_TRANSPARENCY) => {
                // Confirmed quirk: this reply is a level, not an on/off flag.
                Event::TransparencyLevel(frame.payload.first().copied().unwrap_or(0))
            }
            c if c == cmd::reply_to(cmd::GET_BASS_BOOST)
                || c == cmd::notification_of(cmd::GET_BASS_BOOST) =>
            {
                Event::BassBoost(flag(&frame.payload))
            }
            c if c == cmd::SERIAL_NUMBER_REPLY
                || c == cmd::PRODUCT_NAME_REPLY
                || c == cmd::FIRMWARE_VERSION_REPLY =>
            {
                match ascii_text(&frame.payload) {
                    Some(text) => match c {
                        cmd::PRODUCT_NAME_REPLY => Event::Product(text),
                        cmd::FIRMWARE_VERSION_REPLY => Event::Firmware(text),
                        _ => Event::Serial(text),
                    },
                    // A short numeric payload under these ids is a flag, not a
                    // label, so keep it verbatim rather than inventing one.
                    None => Event::Unknown {
                        cmd: frame.cmd,
                        payload: frame.payload.clone(),
                    },
                }
            }
            // Equaliser: each table answers under its own reply id *and* arrives
            // as a push, and the two carry the same bytes.
            c if c == cmd::PUSH_EQ_GAINS || c == cmd::reply_to(cmd::GET_EQ_GAINS) => {
                Event::EqGains(frame.payload.iter().map(|&b| b as i8).collect())
            }
            c if c == cmd::PUSH_EQ_FREQUENCY || c == cmd::reply_to(cmd::GET_EQ_FREQUENCY) => {
                Event::EqRow100b(rows(&frame.payload, 2))
            }
            c if c == cmd::PUSH_EQ_Q || c == cmd::reply_to(cmd::GET_EQ_Q) => {
                Event::EqBandValues(rows(&frame.payload, 2))
            }
            c if c == cmd::PUSH_EQ_FILTER_TYPE || c == cmd::reply_to(cmd::GET_EQ_FILTER_TYPE) => {
                Event::EqLimits(rows(&frame.payload, 1))
            }
            c if c == cmd::PUSH_EQ_GAIN_WIDE || c == cmd::reply_to(cmd::GET_EQ_GAIN_WIDE) => {
                Event::EqGainsWide(rows(&frame.payload, 2))
            }
            c if c == cmd::PUSH_EQ_STATE || c == cmd::reply_to(cmd::GET_EQ_STATE) => {
                Event::EqState(frame.payload.clone())
            }
            _ => Event::Unknown {
                cmd: frame.cmd,
                payload: frame.payload.clone(),
            },
        }
    }

    /// Short description for logs and `watch` output.
    pub fn describe(&self) -> String {
        match self {
            Event::Serial(text) => format!("serial {text}"),
            Event::Product(text) => format!("product {text}"),
            Event::Firmware(text) => format!("firmware {text}"),
            Event::EqGains(gains) => format!("eq gains {}", signed(gains)),
            Event::EqRow100b(rows) => format!("eq 0x100b rows {}", values(rows)),
            Event::EqBandValues(rows) => format!("eq band values {}", values(rows)),
            Event::EqLimits(rows) => format!("eq limits {}", values(rows)),
            Event::EqGainsWide(rows) => format!("eq gains wide {}", values(rows)),
            Event::EqState(state) => format!("eq state {}", hex(state)),
            Event::Battery(b) => format!("battery {}", describe_slots(&b.slots)),
            Event::Charging(c) => format!(
                "charging {}",
                c.slots
                    .iter()
                    .map(|s| match s {
                        Some(true) => "in-case",
                        Some(false) => "out",
                        None => "-",
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Event::Anc(on) => format!("anc {}", on_off(*on)),
            Event::TransparencyLevel(l) => format!("transparency level {l}"),
            Event::TransparencyOn(on) => format!("transparency {}", on_off(*on)),
            Event::BassBoost(on) => format!("bass boost {}", on_off(*on)),
            Event::FeatureRegistered(f) => format!("registered feature {f:#04x}"),
            Event::Unknown { cmd, payload } => {
                format!("unknown cmd {cmd:#06x} payload {}", hex(payload))
            }
        }
    }
}

/// Everything the app knows about the headset, kept current by pushes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceState {
    pub anc: Option<bool>,
    pub transparency_level: Option<u8>,
    pub transparency_on: Option<bool>,
    pub battery: Option<Battery>,
    pub charging: Option<Charging>,
    pub bass_boost: Option<bool>,
    /// Read on demand; see [`crate::proto::cmd::GET_SERIAL_NUMBER`].
    pub serial: Option<String>,
    /// Product name, e.g. `MOMENTUM 5`. Read on demand.
    pub product: Option<String>,
    /// Firmware version and build stamp. Read on demand.
    pub firmware: Option<String>,
    /// The equaliser tables. Populated by registering feature `0x08`, which
    /// [`crate::Session::open`] already does.
    pub eq: Equaliser,
}

/// Render parsed equaliser rows for log output; a band the headset did not
/// report shows as `-` rather than as zero.
fn values(rows: &[Option<u16>]) -> String {
    rows.iter()
        .map(|row| row.map_or("-".to_string(), |v| v.to_string()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Render signed gain bytes.
fn signed(values: &[i8]) -> String {
    values
        .iter()
        .map(i8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// A payload that reads as a label rather than a flag. The serial number, the
/// product name (`MOMENTUM 5`) and the firmware string
/// (`6.21.1 / 2026-05-20 06:46:42+00:00`) all arrive as plain ASCII.
///
/// Spaces are allowed because two of the three contain one, so the guard that
/// keeps a padded flag byte from becoming text is the length and the requirement
/// for at least one alphanumeric character.
fn ascii_text(payload: &[u8]) -> Option<String> {
    if payload.len() < 3 || !payload.iter().all(|&b| b.is_ascii_graphic() || b == b' ') {
        return None;
    }
    if !payload.iter().any(u8::is_ascii_alphanumeric) {
        return None;
    }
    let text: String = payload.iter().map(|&b| b as char).collect();
    let text = text.trim_end();
    (!text.is_empty()).then(|| text.to_string())
}

impl DeviceState {
    /// Fold one push or reply into the cached state.
    pub fn apply(&mut self, event: &Event) {
        match event {
            // Deliberately independent: transparency traffic does not push an
            // ANC change, so touching `anc` here would desync the UI.
            Event::Anc(on) => self.anc = Some(*on),
            Event::TransparencyLevel(level) => {
                self.transparency_level = Some(*level);
                self.transparency_on = Some(*level > 0);
            }
            Event::TransparencyOn(on) => self.transparency_on = Some(*on),
            Event::Battery(b) => self.battery = Some(*b),
            Event::Charging(c) => self.charging = Some(*c),
            Event::BassBoost(on) => self.bass_boost = Some(*on),
            Event::FeatureRegistered(_) | Event::Unknown { .. } => {}
            Event::Serial(text) => self.serial = Some(text.clone()),
            Event::Product(text) => self.product = Some(text.clone()),
            Event::Firmware(text) => self.firmware = Some(text.clone()),
            Event::EqGains(gains) => self.eq.gains = gains.clone(),
            Event::EqRow100b(rows) => self.eq.hz = rows.clone(),
            Event::EqBandValues(rows) => self.eq.q12 = rows.clone(),
            Event::EqLimits(rows) => self.eq.filter_types = rows.clone(),
            Event::EqGainsWide(rows) => self.eq.gains_wide = rows.clone(),
            Event::EqState(state) => self.eq.state = state.clone(),
        }
    }

    /// Effective noise-control mode.
    pub fn mode(&self) -> Mode {
        match (self.transparency_on, self.anc) {
            (Some(true), _) => Mode::Transparency,
            (_, Some(true)) => Mode::Anc,
            (Some(false), Some(false)) => Mode::Off,
            _ => Mode::Unknown,
        }
    }

    /// What identifies this particular unit, on its own line: product, firmware
    /// with its build stamp, and serial. `None` until at least one has been read.
    ///
    /// Kept apart from [`DeviceState::summary`] because these are fixed labels,
    /// not state — printing them on every push would be noise.
    pub fn identity(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(product) = &self.product {
            parts.push(product.clone());
        }
        if let Some(firmware) = &self.firmware {
            // `6.21.1 / 2026-05-20 06:46:42+00:00`: the version is what people
            // quote, the stamp is what tells two builds of it apart.
            let (version, built) = firmware.split_once(" / ").unwrap_or((firmware, ""));
            if built.is_empty() {
                parts.push(format!("firmware {version}"));
            } else {
                parts.push(format!("firmware {version} (built {built})"));
            }
        }
        if let Some(serial) = &self.serial {
            parts.push(format!("serial {serial}"));
        }
        (!parts.is_empty()).then(|| parts.join(" | "))
    }

    /// One-line status for `momentum status`.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("noise control: {}", self.mode().label())];
        if let Some(level) = self.transparency_level {
            parts.push(format!("ambient level {level}"));
        }
        if let Some(battery) = self.battery {
            match battery.primary() {
                Some(level) => parts.push(format!("battery {level}%")),
                None => parts.push("battery: not reported".into()),
            }
        }
        if let Some(bass) = self.bass_boost {
            parts.push(format!("bass boost {}", on_off(bass)));
        }
        parts.join(" | ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Anc,
    Transparency,
    Off,
    Unknown,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Anc => "noise cancelling",
            Mode::Transparency => "transparent hearing",
            Mode::Off => "off",
            Mode::Unknown => "unknown",
        }
    }
}

fn describe_slots(slots: &[Option<u8>; 3]) -> String {
    slots
        .iter()
        .map(|s| match s {
            Some(v) => format!("{v}%"),
            None => "-".into(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// Lowercase hex with spaces, e.g. `50 ff 3c`.
pub fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "<empty>".into();
    }
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::frame::FrameKind;

    fn frame(cmd: u16, payload: &[u8]) -> Frame {
        Frame {
            kind: FrameKind::Response,
            vendor: crate::VENDOR_QUALCOMM,
            cmd,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn decodes_the_captured_battery_push() {
        // 50 50 3C from the captures means left 80 %, right 80 %, case 60 %.
        let event = Event::decode(&frame(0x0683, &[0x50, 0x50, 0x3C]));
        assert_eq!(
            event,
            Event::Battery(Battery {
                slots: [Some(80), Some(80), Some(60)]
            })
        );
        assert_eq!(event.describe(), "battery 80%/80%/60%");
    }

    #[test]
    fn unknown_slot_is_not_zero() {
        let battery = Battery::from_payload(&[0x50, 0xFF, 0x3C]);
        assert_eq!(battery.slots, [Some(80), None, Some(60)]);
        assert_eq!(battery.primary(), Some(80));
    }

    #[test]
    fn decodes_reply_and_push_for_battery_identically() {
        for cmd in [0x0703, 0x0683] {
            assert_eq!(
                Event::decode(&frame(cmd, &[0x50, 0x50, 0x50])),
                Event::Battery(Battery {
                    slots: [Some(80), Some(80), Some(80)]
                })
            );
        }
    }

    #[test]
    fn decodes_charging_flags() {
        let event = Event::decode(&frame(0x0682, &[0x01, 0x00]));
        assert_eq!(
            event,
            Event::Charging(Charging {
                slots: [Some(true), Some(false)]
            })
        );
    }

    #[test]
    fn decodes_anc_reply_and_push() {
        assert_eq!(Event::decode(&frame(0x1B05, &[0x01])), Event::Anc(true));
        assert_eq!(Event::decode(&frame(0x1A85, &[0x00])), Event::Anc(false));
    }

    #[test]
    fn set_anc_ack_carries_no_state() {
        // 0x1B04 must not be mistaken for an ANC state event.
        let event = Event::decode(&frame(0x1B04, &[]));
        assert!(matches!(event, Event::Unknown { cmd: 0x1B04, .. }));
    }

    #[test]
    fn transparency_query_reply_is_a_level() {
        assert_eq!(
            Event::decode(&frame(0x1903, &[0x32])),
            Event::TransparencyLevel(50)
        );
    }

    #[test]
    fn transparency_push_does_not_disturb_anc_state() {
        // The documented firmware behaviour: enabling transparency through the
        // headset only pushes the level, never anc(false).
        let mut state = DeviceState::default();
        state.apply(&Event::Anc(true));
        assert_eq!(state.mode(), Mode::Anc);

        state.apply(&Event::TransparencyLevel(100));
        assert_eq!(
            state.anc,
            Some(true),
            "anc must survive transparency pushes"
        );
        assert_eq!(state.mode(), Mode::Transparency);

        state.apply(&Event::TransparencyLevel(0));
        assert_eq!(state.mode(), Mode::Anc, "back to anc without a new push");
    }

    #[test]
    fn mode_is_off_only_when_both_flags_are_false() {
        let mut state = DeviceState::default();
        assert_eq!(state.mode(), Mode::Unknown);
        state.apply(&Event::Anc(false));
        assert_eq!(state.mode(), Mode::Unknown);
        state.apply(&Event::TransparencyOn(false));
        assert_eq!(state.mode(), Mode::Off);
    }

    #[test]
    fn summary_reads_as_one_line() {
        let mut state = DeviceState::default();
        state.apply(&Event::Anc(true));
        state.apply(&Event::Battery(Battery {
            slots: [Some(80), None, None],
        }));
        state.apply(&Event::BassBoost(true));
        assert_eq!(
            state.summary(),
            "noise control: noise cancelling | battery 80% | bass boost on"
        );
    }

    #[test]
    fn unknown_frames_keep_their_bytes() {
        let event = Event::decode(&frame(0x168A, &[0x01, 0x01]));
        assert_eq!(
            event.describe(),
            "unknown cmd 0x168a payload 01 01",
            "unmapped frames must stay inspectable"
        );
    }

    /// Live capture from a MOMENTUM 5: `0x0003` under the Sennheiser vendor id
    /// answers `0x0103` with the serial number as ASCII digits.
    #[test]
    fn decodes_a_serial_number_reply() {
        let event = Event::decode(&frame(cmd::SERIAL_NUMBER_REPLY, b"8000800326034155"));
        assert_eq!(event, Event::Serial("8000800326034155".into()));

        let mut state = DeviceState::default();
        state.apply(&event);
        assert_eq!(state.serial.as_deref(), Some("8000800326034155"));
    }

    #[test]
    fn a_short_numeric_payload_is_not_a_serial() {
        // The same command id under Qualcomm's vendor id is refused with a
        // single flag byte. That must stay an unknown frame, not a serial.
        let event = Event::decode(&frame(cmd::SERIAL_NUMBER_REPLY, &[0x01]));
        assert!(
            matches!(event, Event::Unknown { .. }),
            "a flag byte must not become an identifier"
        );
    }

    /// Live capture: `0x0004` under vendor `0x001D` answers `0x0104` with the
    /// product name, and `0x0005` answers `0x0105` with version and build stamp.
    /// Both contain a space, which the serial number does not.
    #[test]
    fn decodes_product_and_firmware_replies() {
        let product = Event::decode(&frame(cmd::PRODUCT_NAME_REPLY, b"MOMENTUM 5"));
        assert_eq!(product, Event::Product("MOMENTUM 5".into()));

        let firmware = Event::decode(&frame(
            cmd::FIRMWARE_VERSION_REPLY,
            b"6.21.1 / 2026-05-20 06:46:42+00:00",
        ));
        assert_eq!(
            firmware,
            Event::Firmware("6.21.1 / 2026-05-20 06:46:42+00:00".into())
        );

        let mut state = DeviceState::default();
        state.apply(&product);
        state.apply(&firmware);
        state.serial = Some("8000800326034155".into());
        assert_eq!(
            state.identity().as_deref(),
            Some(
                "MOMENTUM 5 | firmware 6.21.1 (built 2026-05-20 06:46:42+00:00) \
                 | serial 8000800326034155"
            ),
            "identity is the line under the status line, not part of it"
        );
        assert!(
            !state.summary().contains("6.21.1"),
            "fixed labels must not crowd out the state in the summary"
        );
    }

    #[test]
    fn a_padding_of_spaces_is_not_a_label() {
        // Three printable bytes with no alphanumeric is still not text worth
        // reporting: `0x0106` answers empty and other ids answer with padding.
        for payload in [&b"   "[..], &b"--"[..], &[0x01, 0x02][..]] {
            let event = Event::decode(&frame(cmd::PRODUCT_NAME_REPLY, payload));
            assert!(
                matches!(event, Event::Unknown { .. }),
                "{payload:?} must stay an unknown frame"
            );
        }
    }

    /// Live capture from a MOMENTUM 5 after registering feature `0x08`: the
    /// `0x100b` rows as (band, u16 BE), and the flat gain row.
    #[test]
    fn decodes_the_equaliser_pushes() {
        const FREQUENCIES: [u8; 24] = [
            0x00, 0x00, 0x5a, 0x01, 0x01, 0x45, 0x02, 0x05, 0xdc, 0x03, 0x09, 0xc4, 0x04, 0x11,
            0x94, 0x05, 0x1f, 0x40, 0x06, 0x32, 0xc8, 0x07, 0x3e, 0x80,
        ];
        let event = Event::decode(&frame(cmd::PUSH_EQ_FREQUENCY, &FREQUENCIES));
        let Event::EqRow100b(rows) = &event else {
            panic!("expected frequency rows, got {event:?}");
        };
        assert_eq!(
            rows.iter().map(|row| row.unwrap()).collect::<Vec<_>>(),
            vec![90, 325, 1500, 2500, 4500, 8000, 13000, 16000],
            "eight ascending values; NOT the app's bands, which are 50 Hz to 12 kHz"
        );

        let mut state = DeviceState::default();
        state.apply(&event);
        state.apply(&Event::EqGains(vec![0; 8]));
        state.apply(&Event::decode(&frame(
            cmd::PUSH_EQ_FILTER_TYPE,
            &[0x00, 0x0e, 0x01, 0x0e],
        )));
        assert_eq!(state.eq.bands(), 8);
        assert!(!state.eq.is_empty());
        assert_eq!(
            state.eq.filter_types[1],
            Some(14),
            "rows land at the index the headset named"
        );
        assert_eq!(
            state.eq.filter_types.len(),
            2,
            "only two limit rows were sent"
        );
        assert_eq!(
            state.eq.filter_types.get(7),
            None,
            "a band nobody described stays absent, not zero"
        );
        assert_eq!(state.eq.gains.iter().sum::<i8>(), 0, "flat EQ");
    }

    #[test]
    fn an_incomplete_equaliser_row_describes_no_band() {
        let event = Event::decode(&frame(cmd::PUSH_EQ_FREQUENCY, &[0x00, 0x00, 0x5a, 0x01]));
        let Event::EqRow100b(rows) = &event else {
            panic!("expected frequency rows, got {event:?}");
        };
        assert_eq!(rows.as_slice(), &[Some(90)], "a stray byte is not a band");
    }

    #[test]
    fn ten_raw_units_are_one_decibel() {
        // Live: every band set to -6 dB in Smart Control, the table read back as
        // -60 on all eight.
        let eq = Equaliser {
            gains: vec![-60; 8],
            ..Equaliser::default()
        };
        assert_eq!(eq.gains_db(), vec![-6.0_f32; 8]);
        let eq = Equaliser {
            gains: vec![0; 8],
            ..Equaliser::default()
        };
        assert_eq!(eq.gains_db(), vec![0.0_f32; 8]);
        let eq = Equaliser {
            gains: vec![5],
            ..Equaliser::default()
        };
        assert_eq!(eq.gains_db(), vec![0.5], "a slider step is 0.1 dB");
    }

    #[test]
    fn error_frames_are_distinct_from_replies() {
        // Live: a vendor id this firmware does not own answers `0x0007` with
        // `0x0187 [05]`, i.e. the command with both bits set and a GAIA code.
        let refused = frame(cmd::error_for(cmd::REGISTER_FEATURE), &[0x05]);
        assert!(refused.is_error_for(cmd::REGISTER_FEATURE));
        assert!(
            !refused.is_reply_to(cmd::REGISTER_FEATURE),
            "a refusal must not be mistaken for the ack"
        );
        assert_eq!(cmd::error_name(0x05), "not supported");
        assert_eq!(
            cmd::base_of(cmd::error_for(cmd::GET_BATTERY)),
            cmd::GET_BATTERY
        );
    }
}
