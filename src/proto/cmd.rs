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
        0x01 => "unexpected code", // live: 0x0003 under vendor 0x0495
        0x02 => "invalid message size",
        0x03 => "invalid parameters",
        0x04 => "not ready",
        0x05 => "not supported", // live: 0x0007 under vendor 0x001D
        0x06 => "not initialised",
        0x07 => "internal error",
        0x08 => "hardware failure",
        // The 0x80 class is the device saying something is a valid operation in a state
        // where it cannot happen, rather than a command it has never heard of. Both of
        // these were seen from a MOMENTUM 5, and they read as information rather than as
        // failure: `0x82` was the u16 gain store rejecting 182 of a range ending at 180,
        // and `0x80` was the audio mode being set to the mode it was already in.
        0x80 => "already", // live: 0x0803 with the mode already active
        0x81 => "unknown",
        0x82 => "out of range", // live: 0x1010 with a gain of −182
        0x83 => "delete entry failed",
        _ => "unknown error",
    }
}

/// Features passed to [`REGISTER_FEATURE`] to subscribe to pushes.
pub mod feature {
    /// Battery level and charging state. (confirmed)
    pub const BATTERY: u8 = 0x03;
    /// Unit state: worn, not worn, in case. Registering `02` is what produces the
    /// `0x0482` transitions, which is the frame that separates an ear from a table.
    /// (confirmed, by a scripted worn/not-worn test)
    pub const UNIT_STATE: u8 = 0x02;
    /// The audio stream: this is the feature whose tables arrive as the `0x08xx`
    /// block, including the sample rate the codec actually negotiated. It was named
    /// for unit state before that block was understood, which is why the number here
    /// looks one feature off from the frame ids it produces. (confirmed by streaming
    /// from a phone and watching `0x089a` follow the codec)
    pub const AUDIO: u8 = 0x04;
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

/// `Core_GetSupportedFeatures`: the firmware's own list of what it implements, answered
/// **per vendor namespace** — the same command sent to `0x0495` and to the core `0x001d`
/// returns two different lists. The reply is a `moreData` byte followed by
/// `(feature, version)` pairs. (live on MOMENTUM 5, both namespaces answer; see
/// `momentum features`)
pub const GET_SUPPORTED_FEATURES: u16 = 0x0001;

/// Query charging / in-case state. (live on MOMENTUM 5, seen as the `0x0682
/// [01]` push that follows registration)
pub const GET_CHARGING: u16 = 0x0602;
/// Query battery level, one byte 0..=100 (`0xFF` = not reported).
/// (live on MOMENTUM 5: `0x0603 [] -> 0x0703 [64]` = 100 %)
pub const GET_BATTERY: u16 = 0x0603;

/// The noise-control master switch: `ANC_Status` in Sennheiser's model, `01`/`00`.
/// Separate from the `(mode, state)` pairs at `0x1A00`, where mode 1 is anti-wind, 2
/// comfort and 3 adaptive — a distinction that explains why ANC and transparency looked
/// mutually exclusive to a tool that only ever touched one of the two switches.
/// (live on MOMENTUM 5: writing this changed what the headset did)
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

/// The sub-mode word, or bass boost — the naming is not settled and this tool has
/// hedged it the wrong way round. Sennheiser's own device model calls `0x1008` / `0x09`
/// `User_EQ_SetSubMode` / `GetSubMode`, a **bitmask** notified as `0x1089`. We write `01`
/// and read `01` back, and that bit has always agreed with what the app calls bass boost,
/// but no test has yet toggled the app's bass boost and watched which bit moves.
/// (live on MOMENTUM 5, name uncertain)
pub const SET_BASS_BOOST: u16 = 0x1008;
/// Query bass boost. (live on MOMENTUM 5)
pub const GET_BASS_BOOST: u16 = 0x1009;

/// Push carrying the transparency level; `> 0` means transparency is on.
/// (confirmed)
pub const PUSH_TRANSPARENCY_LEVEL: u16 = 0x1A83;
/// Push carrying the transparency on/off flag. (confirmed)
pub const PUSH_TRANSPARENCY_ON: u16 = 0x1885;

/// How many bands the user equaliser has, and the range each accepts.
///
/// The reply is `bands, min, max, selectable_presets, user_presets`, all one byte
/// except the two gains, which are signed tenths of a dB. Measured on MOMENTUM 5:
/// `08 c4 3c 00 00` — **eight bands, −6.0 dB to +6.0 dB**. This is where a client
/// learns the limits instead of assuming them. (live)
///
/// The last two bytes are worth more than they look: **this unit answers `0` and `0`, so
/// the headset has no preset of its own to select and no user preset slots at all.** The
/// `Rock / Pop / Jazz / Neutral` list and the owner's own stored curves live in the app,
/// which applies one by writing the changed bands — `UserEQ_SetSliderGain`, the same command
/// this program sends. So there is nothing here to select, and an `eq preset` command would
/// be a fiction: the only thing a preset *is*, on this device, is a set of gains somebody
/// wrote. (field layout from the vendor's device model; the two zeros measured)
pub const GET_EQ_CONFIG: u16 = 0x1000;
/// Write one band: `[band, gain]`, gain signed in tenths of a dB. Answers an empty
/// ack and then pushes the whole table. (live: `[00, 3c]` → `0x1101 []`, then
/// `0x1082`)
///
/// The unit was settled by Sennheiser's own device model, which names this command
/// `UserEQ_SetSliderGain` with `UINT8 band, INT8 gain(dB × 10)`, and by the read-back
/// agreeing band for band. Note that the user equaliser is only *heard* in audio mode
/// `userEq`; a write that lands while another mode is active is real but inaudible.
pub const SET_EQ_GAIN: u16 = 0x1001;
/// Write one band's **filter frequency**: `[band, u16 Hz big-endian]`, answered with
/// an empty ack. The read side is [`GET_EQ_FREQUENCY`]. (live: `[00, 00, 5a]` → `0x110a []`)
///
/// The equaliser publishes a frequency, a Q and a filter type per band, and all three
/// accept writes, which is a parametric filter bank — the Momentum 5's own app exposes
/// only the gains. Whether the DSP reads what is written here has not yet been heard.
pub const SET_EQ_FREQUENCY: u16 = 0x100A;
/// Write one band's **Q factor**, `[band, u16 Q12]`. (live: `[00, 08, 00]` → `0x110c []`)
pub const SET_EQ_Q: u16 = 0x100C;
/// Write one band's **filter type**, `[band, u8]`; every band reads `14` here.
/// (live: `[00, 0e]` → `0x110e []`)
pub const SET_EQ_FILTER_TYPE: u16 = 0x100E;
/// Read one band: `[band]` in, one signed byte out. (live: `0x1002 [00] → 0x1102 [3c]`)
pub const GET_EQ_GAIN: u16 = 0x1002;
/// Unresolved neighbour of the two above. Every payload tried — empty, one byte,
/// eight, sixteen, twenty-four — was refused, so it is not the gains getter it looks
/// like. Kept because a named unknown is easier to find again than a bare number.
pub const GET_EQ_GAINS: u16 = 0x1003;
/// A per-band u16 from `0x100b`, ascending, in what is probably hertz.
///
/// **This is not the graphic equaliser's band set.** Smart Control offers 50, 100,
/// 250, 600, 1400, 3000, 7000 and 12000 Hz, while this table reads 90, 325, 1500,
/// 2500, 4500, 8000, 13000 and 16000 — eight values, ascending, a different filter
/// bank. The id keeps the word "frequency" because that is the likeliest unit, not
/// because the band set is known.
pub const GET_EQ_FREQUENCY: u16 = 0x100b;
/// A per-band u16 whose unit is unidentified — `0x0800` on band 0 and `0x0b5c`
/// on the others here. Named for its id because naming it anything else would
/// be a guess.
pub const GET_EQ_Q: u16 = 0x100d;
/// Per-band limit, `14` on every band of this unit, unit unknown. Read-only:
/// the even neighbour `0x100e` refuses the write.
pub const GET_EQ_FILTER_TYPE: u16 = 0x100f;
/// The user gain of one band as a u16, where the eight-byte form of
/// [`GET_EQ_GAINS`] reports the same thing in one frame.
///
/// **Nothing written here has been heard to change anything, but it is not proven
/// inert either.** Its even neighbour `0x1010` is range-checked — a MOMENTUM 5
/// accepted `-180` on a band and refused `-182` with error `0x82` — stores what it
/// accepts, and echoes it back. None of it was audible, at 90 Hz or at 2.5 kHz, and
/// the app's own slider moves never touch this table. **Settled by ear rather than
/// by inference:** the whole table was written as a −6 … +6 dB tilt and then its
/// mirror image, switched repeatedly with music playing, and the sound did not
/// change — so the earlier escape that level normalisation had been masking small
/// changes no longer holds. Whatever this table is, it is not the user equaliser.
pub const GET_EQ_GAIN_WIDE: u16 = 0x1011;
/// `0x1010` writes the store [`GET_EQ_GAIN_WIDE`] reads, and unlike its odd
/// neighbour it takes the **whole table in one frame**: eight `(band, u16 BE)` rows,
/// 24 bytes. It acknowledges with an empty reply and then pushes
/// [`PUSH_EQ_GAIN_WIDE`] back. Carries the same caution as the id it writes — no
/// audible effect has ever been confirmed from it.
pub const SET_EQ_GAIN_WIDE: u16 = 0x1010;
/// A two-byte row whose meaning is unknown. Read live, kept raw.
pub const GET_EQ_STATE: u16 = 0x1013;

/// Pushes of each table above, sent together when [`feature::EQ`] is registered.
pub const PUSH_EQ_GAINS: u16 = 0x1082;
/// See [`PUSH_EQ_GAINS`]: `(band, UINT16 Hz)` per row, three bytes a band. The vendor's M4
/// traces carry `90, 325, 1500, 6500, 6500` and a MOMENTUM 5 answers `90, 325, 1500` for its
/// first three bands and then five more of its own — **the same tables, the same decode,
/// confirmed on two products that never saw this program.**
pub const PUSH_EQ_FREQUENCY: u16 = 0x108b;
/// See [`PUSH_EQ_GAINS`]: `(band, UINT16 Q)` per row, **Q12 over 4096**. The vendor's M4
/// traces carry `0x0800` and `0x0b5c` — 0.500 and 0.710 — and those are exactly the two
/// numbers a MOMENTUM 5 reports. It was called `band_value` here until the vendor's model
/// said otherwise. (name from the vendor's device model, values measured on both units)
pub const PUSH_EQ_Q: u16 = 0x108d;
/// See [`PUSH_EQ_GAINS`]: `(band, UINT8 filter type)` per row. Every band of a MOMENTUM 5
/// answers `14` and so does every band of the M4 in the vendor's traces. **It is a filter
/// type, not a limit** — which is what this id was first called here, on the strength of its
/// value alone and against the fact that a limit of 14 would disagree with the ±6.0 dB the
/// device states for itself at [`GET_EQ_CONFIG`].
pub const PUSH_EQ_FILTER_TYPE: u16 = 0x108f;
/// See [`PUSH_EQ_GAINS`]. The vendor's model has this one too and calls it "probably the DSP
/// gain", unverified; on a MOMENTUM 5 it has never been seen at all.
pub const PUSH_EQ_GAIN_WIDE: u16 = 0x1091;
/// See [`PUSH_EQ_GAINS`]. `[00, 00]` on a MOMENTUM 5 and `[00, 00]` in the vendor's M4
/// traces, unknown on both — the only member of this block that neither product explains.
pub const PUSH_EQ_STATE: u16 = 0x1093;

/// The sample rate of the live audio link, four bytes big-endian.
///
/// Measured by playing from a phone and pausing again: `48000` while Qobuz streamed,
/// `44100` at rest, `0` for the few hundred milliseconds the link spends being
/// renegotiated. The at-rest `44100` is not silence — it is the idle A2DP link to the
/// PC, which is Windows' default output. Live on MOMENTUM 5, firmware 6.21.1.
pub const PUSH_SAMPLE_RATE: u16 = 0x089A;

/// Which codec the headset actually settled on, in one byte.
///
/// Measured here: `ff` with nothing streaming, `02` and `08` during real playback.
/// Sennheiser's own device model calls the same command `Sound_CodecUsed` and names the
/// values `02` aptX, `08` aptX Adaptive, `ff` none — which is why this stopped being a
/// mystery byte. An earlier name here promised a "link state", a guess wearing a label.
pub const PUSH_CODEC: u16 = 0x0880;

/// Which equaliser path the DSP is applying. `UINT16`, unlike every other setter in this
/// block. (live: `[00, 01]` on a unit running its user equaliser)
pub const GET_AUDIO_MODE: u16 = 0x0804;

/// Select the audio mode. Payload is the same big-endian `UINT16`.
///
/// Live on MOMENTUM 5: `[00, 01]` accepted; `[00, 04]` answered `0x0983 [80]`, already,
/// when the device was already in mode 4, and `[00, 01]` was then accepted to leave it.
/// The names for the values come from Smart Control Plus's enumeration, which is shared
/// across the product family, so a mode number existing here is not evidence that the
/// feature it names exists in this firmware.
pub const SET_AUDIO_MODE: u16 = 0x0803;

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
        // An error frame naming command 0x0000 in the core feature. Seen from a MOMENTUM 5
        // in place of the reply to `0x080d` — twice, both times when other getters were
        // queried in the same run, never when it was the only command sent, and the same
        // command answered `0x090d [01]` correctly every time it was sent alone. So this is
        // the device being uncertain, not a statement about the command: an error frame seen
        // once, in a batch, is not evidence. Re-send the command on its own before writing
        // down a refusal.
        0x0180 => "core_error",
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
        GET_EQ_Q => "GET_EQ_Q",
        PUSH_EQ_Q => "eq_q_push",
        GET_EQ_FILTER_TYPE => "GET_EQ_FILTER_TYPE",
        PUSH_EQ_FILTER_TYPE => "eq_filter_type_push",
        GET_EQ_GAIN_WIDE => "get_eq_gain_wide",
        PUSH_EQ_GAIN_WIDE => "eq_gain_wide_push",
        GET_EQ_STATE => "get_eq_state",
        PUSH_EQ_STATE => "eq_state_push",
        0x1102 => "eq_gain_band_reply",
        GET_EQ_CONFIG => "get_eq_config",
        SET_EQ_GAIN => "set_eq_gain",
        GET_EQ_GAIN => "get_eq_gain",
        0x1100 => "eq_config_reply",
        0x1101 => "eq_gain_set_ack",
        SET_EQ_FREQUENCY => "set_eq_frequency",
        SET_EQ_Q => "set_eq_q",
        SET_EQ_FILTER_TYPE => "set_eq_filter_type",
        0x110a => "eq_frequency_ack",
        0x110c => "eq_q_ack",
        0x110e => "eq_filter_type_ack",
        // The codec actually in use, and the audio mode that decides whether the user
        // equaliser is audible at all. Both read via their `+0x100` reply.
        0x0900 => "codec_reply",
        0x0904 => "audio_mode_reply",
        0x0903 => "audio_mode_set_ack",
        0x0983 => "audio_mode_error",
        0x1301 => "firmware_three_part_reply",
        0x1306 => "model_reply",
        // Getters named from Sennheiser's device model and answered on this MOMENTUM 5,
        // listed as literals because nothing in this crate sends them yet — the point of
        // naming them is that `probe` stops printing `unknown` for a answered frame.
        0x0001 => "get_supported_features",
        0x0101 => "supported_features_reply",
        0x0002 => "get_supported_features_next",
        0x0102 => "supported_features_next_reply",
        0x0401 => "get_on_head_detection",
        0x0501 => "on_head_detection_reply",
        0x0406 => "get_bluetooth_compatibility_mode",
        0x0506 => "bluetooth_compatibility_mode_reply",
        0x0601 => "get_timer",
        0x0701 => "timer_reply",
        0x0800 => "get_codec",
        0x0802 => "get_audio_prompt_mode",
        0x0902 => "audio_prompt_mode_reply",
        0x0806 => "get_sidetone",
        0x0906 => "sidetone_reply",
        0x0818 => "get_low_latency_mode",
        0x0918 => "low_latency_mode_reply",
        0x081a => "get_sample_rate",
        0x091a => "sample_rate_reply",
        0x1200 => "get_hw_revision",
        0x1300 => "hw_revision_reply",
        0x1202 => "get_firmware_versions",
        0x1302 => "firmware_versions_reply",
        0x1400 => "get_paired_devices_size",
        0x1500 => "paired_devices_size_reply",
        0x1401 => "get_paired_device_info",
        0x1501 => "paired_device_info_reply",
        0x1407 => "get_own_device_index",
        0x1507 => "own_device_index_reply",
        0x1409 => "get_max_bt_connections",
        0x1509 => "max_bt_connections_reply",
        0x1605 => "is_default_mmi_config",
        0x1705 => "default_mmi_config_reply",
        0x1607 => "get_auto_lock_mode",
        0x1707 => "auto_lock_mode_reply",
        0x110b => "eq_frequency_reply",
        0x110d => "eq_q_reply",
        0x110f => "eq_filter_type_reply",
        0x1110 => "eq_gain_wide_ack",
        0x1111 => "eq_gain_wide_reply",
        0x1113 => "eq_state_reply",
        // Measured on a Momentum 5, firmware 6.21.1, by scripted physical steps:
        // `[02]` every time they came off the head, `[03]` every time they went back
        // on, and nothing at all from the app. Whether that is wear or simply the
        // unit being in use is not settled — see docs/PROTOCOL.md.
        0x0482 => "unit_state_push",
        // The getter for the same value: `01` in case, `02` not on head, `03` on head.
        // Payload byte 0 is the state; a second byte is used for the right side on earbuds
        // and is absent here. (live)
        0x0502 => "physical_state_reply",
        // Not stability. The name came from the published numbering and was never
        // checked; it moves with the audio stream instead — `02` at rest on a 44.1 kHz
        // link, `ff` for the moment the link is renegotiated, `08` while a phone
        // streamed at 48 kHz. What the values mean is still unknown, so the name says
        // only that it belongs to the link.
        PUSH_CODEC => "codec_push",
        PUSH_SAMPLE_RATE => "sample_rate_push",
        0x1A81 => "mode_list_push",
        0x2C83 => "wear_push",
        // The audio mode's own notification, `0x0804 | 0x80`. Named from the app's device
        // model, which lists `Ntf 0x0884` against `AudioMode`, and seen arriving with
        // `[00, 03]` and `[00, 01]` as the mode moved on a MOMENTUM 5 — the M4 tool's
        // hardware check saw the same frame carry `[00, 02]` after a podcast-mode set.
        // A client that registers feature 4 never has to poll the mode. (live)
        0x0884 => "audio_mode_push",
        // Sound personalization, feature 16: mode at `0x2001`, six hearing thresholds at
        // `0x2003`, six ratios at `0x2005`, three gains and two preset indices at `0x2007`.
        // Ids and schema are Sennheiser's own, published in the M4 device model shipped
        // inside Smart Control; the confirmation is that a MOMENTUM 5 answers all four
        // getters with payloads of exactly the documented shapes. (live)
        0x2101 => "personal_mode_reply",
        0x2103 => "personal_threshold_reply",
        0x2105 => "personal_ratio_reply",
        0x2107 => "personal_gains_reply",
        0x2081 => "personal_mode_push",
        // Given names by the vendor's model that this program had left as hex. The first is
        // the touch-lock push — its getter `0x1607` was read here and answered `[00]` — the
        // second is a low-latency mode the app hides, whose getter `0x0818` answers `[00]`
        // on this unit, and the third is the per-host connection status of the paired list.
        0x1687 => "auto_lock_push",
        0x0898 => "low_latency_push",
        0x1484 => "connection_status_push",
        _ => "unknown",
    }
}
