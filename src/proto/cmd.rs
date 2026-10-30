//! Command and feature identifiers.
//!
//! Confidence levels used throughout this module:
//!
//! * **live** — sent to, or received from, a MOMENTUM 5 (BD_ADDR
//!   `80:C3:BA:C5:BB:97`) driven by this program, and observed to change the
//!   headset's behaviour. This is the strongest claim this crate makes.
//! * **confirmed** — seen in btsnoop captures of the official Smart Control
//!   app driving a MOMENTUM True Wireless 4 *and* in an independent tool for
//!   the MOMENTUM 4, or byte-identical in both.
//! * **probable** — documented for one MOMENTUM model only. Same Qualcomm GAIA
//!   family, so very likely the same, but not yet observed here.
//! * **probe** — candidate id worth trying; behaviour unknown. Responses are
//!   printed raw by `momentum probe` so a new finding can be added here.

/// Setting this bit turns a command id into its reply id.
pub const REPLY_BIT: u16 = 0x0100;
/// Setting this bit turns a command id into its notification id.
pub const NOTIFICATION_BIT: u16 = 0x0080;
/// Both bits: the headset could not carry out the command, and the payload is a
/// [`error_name`] code. Observed live: `0x0007` under a vendor id this firmware
/// does not own is answered `0x0187 [05]`, i.e. NOT_SUPPORTED.
pub const ERROR_BIT: u16 = REPLY_BIT | NOTIFICATION_BIT;

/// The id the headset uses to answer `cmd`.
pub const fn reply_to(cmd: u16) -> u16 {
    cmd | REPLY_BIT
}

/// The id the headset uses to push state changes for `cmd`.
pub const fn notification_of(cmd: u16) -> u16 {
    cmd | NOTIFICATION_BIT
}

/// The id the headset uses to refuse `cmd`.
pub const fn error_for(cmd: u16) -> u16 {
    cmd | ERROR_BIT
}

/// True for a frame id that carries an error code rather than state.
pub const fn is_error(raw: u16) -> bool {
    raw & ERROR_BIT == ERROR_BIT
}

/// Strip reply/notification bits to get the command a frame belongs to.
pub const fn base_of(cmd: u16) -> u16 {
    cmd & !(REPLY_BIT | NOTIFICATION_BIT)
}

/// Names for the error code in the payload of an [`ERROR_BIT`] frame. These are
/// the Qualcomm GAIA error codes; the ones marked *live* were seen from a
/// MOMENTUM 5.
pub fn error_name(code: u8) -> &'static str {
    match code {
        0x00 => "no error",
        0x01 => "unexpected code",           // live: 0x0003 under vendor 0x0495
        0x02 => "invalid message size",
        0x03 => "invalid parameters",
        0x04 => "not ready",
        0x05 => "not supported",             // live: 0x0007 under vendor 0x001D
        0x06 => "not initialised",
        _ => "unknown error",
    }
}

/// Features passed to [`REGISTER_FEATURE`] to subscribe to pushes.
pub mod feature {
    /// Battery level and charging state. (confirmed)
    pub const BATTERY: u8 = 0x03;
    /// Per-unit state transitions, e.g. in-case / out-of-case. (confirmed)
    pub const UNIT_STATE: u8 = 0x04;
    /// User equaliser. **live on MOMENTUM 5**: registering it answers the ack
    /// and then pushes the whole band configuration.
    pub const EQ: u8 = 0x08;
    /// Transparency / ambient sound. (confirmed)
    pub const TRANSPARENCY: u8 = 0x0C;
    /// Active noise control. (confirmed)
    pub const ANC: u8 = 0x0D;
}

/// Subscribe to pushes for one feature. Answered with `0x0107` and an empty
/// payload; that ack is the signal that the session is live. (live on
/// MOMENTUM 5, and the battery/charging pushes follow it unprompted)
pub const REGISTER_FEATURE: u16 = 0x0007;

/// Query charging / in-case state. (live on MOMENTUM 5, seen as the `0x0682
/// [01]` push that follows registration)
pub const GET_CHARGING: u16 = 0x0602;
/// Query battery level, one byte 0..=100 (`0xFF` = not reported).
/// (live on MOMENTUM 5: `0x0603 [] -> 0x0703 [64]` = 100 %)
pub const GET_BATTERY: u16 = 0x0603;

/// Set active noise control on/off. Payload `01`/`00`. (live on MOMENTUM 5)
pub const SET_ANC: u16 = 0x1A04;
/// Query active noise control state. (live: `0x1A05 [] -> 0x1B05 [01]`)
pub const GET_ANC: u16 = 0x1A05;

/// Turn transparency on/off. Payload `01`/`00`. (confirmed on TW4)
pub const SET_TRANSPARENCY: u16 = 0x1804;
/// Query transparency. The reply carries a *level*, not a boolean. (confirmed)
pub const GET_TRANSPARENCY: u16 = 0x1803;

/// Set the ambient/transparent-hearing level, 0..=100. (live on MOMENTUM 5:
/// `25` put the headset in transparent hearing, `0` returned it to ANC)
pub const SET_TRANSPARENCY_LEVEL: u16 = 0x1A02;
/// Query the ambient/transparent-hearing level. (live on MOMENTUM 5)
pub const GET_TRANSPARENCY_LEVEL: u16 = 0x1A03;

/// Set bass boost on/off. (live on MOMENTUM 5)
pub const SET_BASS_BOOST: u16 = 0x1008;
/// Query bass boost. (live on MOMENTUM 5)
pub const GET_BASS_BOOST: u16 = 0x1009;

/// Push carrying the transparency level; `> 0` means transparency is on.
/// (confirmed)
pub const PUSH_TRANSPARENCY_LEVEL: u16 = 0x1A83;
/// Push carrying the transparency on/off flag. (confirmed)
pub const PUSH_TRANSPARENCY_ON: u16 = 0x1885;

/// The equaliser, in its own command range. Read live from a MOMENTUM 5.
///
/// Two things about this block are unlike the rest of the protocol and cost
/// measurement to learn. First, the base ids **refuse a bare read** with `05`:
/// they take a band index, and the complete tables arrive unprompted when
/// [`REGISTER_FEATURE`] is sent with [`feature::EQ`]. Second, the even/odd rule
/// seen elsewhere holds — the even id writes, the odd one reads — which is how
/// `0x1010` was found to accept a gain, answering an empty ack and then pushing
/// the table again.
///
/// The gains are one signed byte per band with flat reported as zero, but the
/// **scale is unmeasured**: no write other than the value already in effect has
/// been sent, so this crate does not translate bytes into decibels.
pub const GET_EQ_GAINS: u16 = 0x1003;
/// Centre frequency of one band, in Hz, big-endian u16. The eight bands of a
/// MOMENTUM 5 are 90, 325, 1500, 2500, 4500, 8000, 13000 and 16000 Hz.
pub const GET_EQ_FREQUENCY: u16 = 0x100b;
/// A per-band u16 whose unit is unidentified — `0x0800` on band 0 and `0x0b5c`
/// on the others here. Named for its id because naming it anything else would
/// be a guess.
pub const GET_EQ_BAND_VALUE: u16 = 0x100d;
/// Per-band limit, `14` on every band of this unit, unit unknown. Read-only:
/// the even neighbour `0x100e` refuses the write.
pub const GET_EQ_LIMIT: u16 = 0x100f;
/// The user gain of one band as a u16, where the eight-byte form of
/// [`GET_EQ_GAINS`] reports the same thing in one frame.
///
/// **Writing this changes nothing you can hear, but it is not inert.** Its even
/// neighbour `0x1010` is range-checked — a MOMENTUM 5 accepted `-180` on a band and
/// refused `-182` with error `0x82` — stores what it accepts, and echoed it back.
/// None of it was audible, at 90 Hz or at 2.5 kHz, and the app's own slider moves
/// never touch this table at all. The likeliest explanation is that the equaliser
/// is switched off by some other field, [`GET_EQ_STATE`] being the candidate. Not
/// measured, so nothing in this crate writes it.
pub const GET_EQ_GAIN_WIDE: u16 = 0x1011;
/// A two-byte row whose meaning is unknown. Read live, kept raw.
pub const GET_EQ_STATE: u16 = 0x1013;

/// Pushes of each table above, sent together when [`feature::EQ`] is registered.
pub const PUSH_EQ_GAINS: u16 = 0x1082;
/// See [`PUSH_EQ_GAINS`].
pub const PUSH_EQ_FREQUENCY: u16 = 0x108b;
/// See [`PUSH_EQ_GAINS`].
pub const PUSH_EQ_BAND_VALUE: u16 = 0x108d;
/// See [`PUSH_EQ_GAINS`].
pub const PUSH_EQ_LIMIT: u16 = 0x108f;
/// See [`PUSH_EQ_GAINS`].
pub const PUSH_EQ_GAIN_WIDE: u16 = 0x1091;
/// See [`PUSH_EQ_GAINS`].
pub const PUSH_EQ_STATE: u16 = 0x1093;

/// Read the serial number. The payload of the reply is ASCII digits.
///
/// This one belongs to the **Sennheiser** vendor namespace: under Qualcomm's
/// `0x0495` the headset refuses it (`0x0183 [01]`), under `0x001D` it answers
/// `0x0103` with e.g. `"8000800326034155"`. Live on MOMENTUM 5.
pub const GET_SERIAL_NUMBER: u16 = 0x0003;
/// Reply id of [`GET_SERIAL_NUMBER`] under the Sennheiser vendor namespace.
pub const SERIAL_NUMBER_REPLY: u16 = 0x0103;

/// Read the product name as ASCII — `"MOMENTUM 5"` on this unit.
///
/// Same Sennheiser-only namespace as [`GET_SERIAL_NUMBER`]: under `0x0495` this
/// id answers `0x0184 [01]` (unexpected code), under `0x001D` it answers
/// `0x0104`. Live on MOMENTUM 5.
pub const GET_PRODUCT_NAME: u16 = 0x0004;
/// Reply id of [`GET_PRODUCT_NAME`] under the Sennheiser vendor namespace.
pub const PRODUCT_NAME_REPLY: u16 = 0x0104;

/// Read the firmware version and build stamp as one ASCII string, separated by
/// `" / "`: `"6.21.1 / 2026-05-20 06:46:42+00:00"`.
///
/// This is the version the Sennheiser app shows, and the reason it lives here
/// rather than in a Windows-specific module: it is nine bytes out and forty-two
/// back on the ordinary control channel. Live on MOMENTUM 5, Sennheiser
/// namespace only.
pub const GET_FIRMWARE_VERSION: u16 = 0x0005;
/// Reply id of [`GET_FIRMWARE_VERSION`] under the Sennheiser vendor namespace.
pub const FIRMWARE_VERSION_REPLY: u16 = 0x0105;

/// Human-readable name for a raw command id, used by `momentum watch` and
/// `momentum raw` so a capture is readable while scrolling.
pub fn name(cmd: u16) -> &'static str {
    match cmd {
        REGISTER_FEATURE => "register_feature",
        0x0107 => "register_ack",
        GET_CHARGING => "get_charging",
        GET_BATTERY => "get_battery",
        0x0702 => "charging_reply",
        0x0682 => "charging_push",
        0x0703 => "battery_reply",
        0x0683 => "battery_push",
        SET_ANC => "set_anc",
        GET_ANC => "get_anc",
        0x1B04 => "set_anc_ack",
        0x1B05 => "anc_reply",
        0x1A85 => "anc_push",
        SET_TRANSPARENCY => "set_transparency",
        GET_TRANSPARENCY => "get_transparency",
        0x1904 => "set_transparency_ack",
        0x1903 => "transparency_level_reply",
        SET_TRANSPARENCY_LEVEL => "set_transparency_level",
        GET_TRANSPARENCY_LEVEL => "get_transparency_level",
        PUSH_TRANSPARENCY_LEVEL => "transparency_level_push",
        PUSH_TRANSPARENCY_ON => "transparency_on_push",
        SET_BASS_BOOST => "set_bass_boost",
        GET_BASS_BOOST => "get_bass_boost",
        0x1109 => "bass_boost_reply",
        GET_SERIAL_NUMBER => "get_serial_number",
        SERIAL_NUMBER_REPLY => "serial_reply",
        GET_PRODUCT_NAME => "get_product_name",
        PRODUCT_NAME_REPLY => "product_reply",
        GET_FIRMWARE_VERSION => "get_firmware_version",
        FIRMWARE_VERSION_REPLY => "firmware_reply",
        GET_EQ_GAINS => "get_eq_gains",
        PUSH_EQ_GAINS => "eq_gains_push",
        GET_EQ_FREQUENCY => "get_eq_frequency",
        PUSH_EQ_FREQUENCY => "eq_frequency_push",
        GET_EQ_BAND_VALUE => "get_eq_band_value",
        PUSH_EQ_BAND_VALUE => "eq_band_value_push",
        GET_EQ_LIMIT => "get_eq_limit",
        PUSH_EQ_LIMIT => "eq_limit_push",
        GET_EQ_GAIN_WIDE => "get_eq_gain_wide",
        PUSH_EQ_GAIN_WIDE => "eq_gain_wide_push",
        GET_EQ_STATE => "get_eq_state",
        PUSH_EQ_STATE => "eq_state_push",
        0x1102 => "eq_gain_ack",
        0x110b => "eq_frequency_reply",
        0x110d => "eq_band_value_reply",
        0x110f => "eq_limit_reply",
        0x1111 => "eq_gain_wide_reply",
        0x1113 => "eq_state_reply",
        0x0482 => "unit_state_push",
        0x0880 => "stability_push",
        0x1A81 => "mode_list_push",
        0x2C83 => "wear_push",
        _ => "unknown",
    }
}
