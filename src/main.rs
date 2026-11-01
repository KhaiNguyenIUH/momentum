//! `momentum` command line.
//!
//! The two hardware-facing commands here answer an open question: does the
//! MOMENTUM 5 expose its controls over Bluetooth Low Energy GATT, or only over
//! the BR/EDR RFCOMM channel the Sennheiser app is known to use? `scan` finds
//! the headset, `gatt` dumps everything it publishes.

use std::error::Error as StdError;
use std::time::{Duration, Instant};

use btleplug::api::{BDAddr, Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use clap::{Parser, Subcommand};
use futures::StreamExt;

use momentum::proto::cmd;
use momentum::proto::frame::{Framer, HEADER_LEN};
use momentum::proto::state::hex;

type Boxed = Box<dyn StdError + Send + Sync>;

#[derive(Parser)]
#[command(
    name = "momentum",
    version,
    about = "Inspect and control a Sennheiser MOMENTUM headset"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List nearby Bluetooth LE devices.
    Scan {
        /// Seconds to listen for advertisements.
        #[arg(long, default_value_t = 6)]
        seconds: u64,
        /// Only show devices whose name contains this text.
        #[arg(long, default_value = "momentum")]
        filter: String,
    },
    /// Dump a device's GATT table, read every readable characteristic and
    /// stream notifications. Use this to see whether noise control is reachable
    /// over BLE.
    Gatt {
        /// Connect by address instead of name, e.g. 46:1E:05:C7:9D:30.
        #[arg(long)]
        address: Option<String>,
        /// Device name substring used when no address is given.
        #[arg(long, default_value = "momentum")]
        filter: String,
        /// Seconds to scan while looking for the device.
        #[arg(long, default_value_t = 8)]
        seconds: u64,
        /// Skip reading characteristics (some devices stall on reads).
        #[arg(long)]
        no_read: bool,
        /// Subscribe to everything notify-capable and stream for this many
        /// seconds. Toggle ANC in the Sennheiser app while it runs.
        #[arg(long)]
        notify: Option<u64>,
    },

    /// List paired devices publishing an RFCOMM control service.
    RfScan {
        /// Select by service name or peer address, e.g. "GAIA" or
        /// "80:c3:ba". Empty means every device that publishes it.
        #[arg(long, default_value = "")]
        filter: String,
        /// Service UUID to look up instead of the GAIA control service.
        #[arg(long)]
        uuid: Option<String>,
    },
    /// Read noise control, ambient level and battery over the control channel.
    Status {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Print what identifies this unit: product name, firmware version with its
    /// build stamp, and serial number. All three are read over the control
    /// channel, in Sennheiser's own vendor namespace, so they are the headset's
    /// own answer and not something Windows remembers.
    Info {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// The equaliser: read the filter bank, or move one part of one band and hear it.
    #[command(subcommand)]
    Eq(EqAction),
    /// Show or change the audio mode — which equaliser path the DSP is applying. The user
    /// equaliser is only heard in `usereq`; a curve written under another mode is stored,
    /// correct, and silent.
    ///
    /// The names are Smart Control Plus's enumeration, which is shared across the product
    /// family, so `parametric` means "the value that enum calls parametricEq" and nothing
    /// more. This firmware accepts it and sits in it; no parametric parameter store has
    /// been found on the wire, and no ear has yet attributed any change of sound to a
    /// change of mode on this unit.
    Mode {
        /// `off`, `usereq`, `podcast`, `personalized`, `parametric`, `hearing`, or 0 to 5.
        /// Omit to read the mode without changing it.
        #[arg(value_parser = parse_mode)]
        mode: Option<u8>,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Interactive step-by-step listener, for experiments where a human is the one
    /// doing something physical. Registers the given features, prints what arrives,
    /// then waits: **press Enter to close a step** and start the next one, `q` to
    /// quit. Nothing is timed from here, and nothing runs in the background — the
    /// frames between two presses belong to whatever you did in between. Add
    /// `--out log.txt` to keep the transcript for afterwards.
    Steps {
        /// Feature ids to register, hex: `04`, `01-1f`, `04,08`.
        #[arg(long, default_value = "04")]
        features: String,
        /// RFCOMM server channel, as for `probe`.
        #[arg(long)]
        channel: Option<u32>,
        /// Also write every line to this file.
        #[arg(long)]
        out: Option<String>,
        /// Read the experiment aloud, one step at a time: `wear` or `buttons`.
        /// Each step says exactly what state to put the headphones in and waits for
        /// Enter, so every frame is attributed to a state you know.
        #[arg(long)]
        script: Option<String>,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Ask whether Windows will let this PC host the Sennheiser control service.
    ///
    /// Smart Control dials a headset by service UUID, so anything answering that UUID
    /// is a headset as far as the app knows — which is the only route left to see what
    /// the app *sends* rather than what the headset remembers. This asks Windows the
    /// first question in that chain and stops there: it reserves the SDP record,
    /// advertises nothing, leaves the machine undiscoverable, and does not contact the
    /// headphones.
    Host {
        /// Bind the service, advertise it, make the radio discoverable and wait for a
        /// client — the phone in other words, which has to be paired with this PC
        /// first. Every frame it sends is printed. Stop with Ctrl-C; stopping drops
        /// the provider and the advertising goes with it.
        #[arg(long)]
        serve: bool,
        /// Name published in the service record, so there is something to tap on the
        /// phone. The paired device's own name is this computer's, which we cannot
        /// change from here.
        #[arg(long, default_value = "MOMENTUM 5")]
        name: String,
    },
    /// Temporary listening test: write an ascending −6 … +6 dB tilt into the
    /// writable u16 store (`0x1010`), then read it back from the device and report
    /// whether it stuck. This is the store this host can write, and the one no ear
    /// has ever confirmed — switch between `ramp1` and `ramp2` with music playing.
    ///
    /// The write is **persisted**: the values survive the headset being switched
    /// off. `ramp0` puts it back to zeros.
    Ramp1 {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Temporary: the mirror image, +6 … −6 dB.
    Ramp2 {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Temporary: zeros to the same store, undoing a ramp.
    Ramp0 {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Print every state push the headset sends, for as long as you watch.
    /// Show what the audio codec actually negotiated, live: the sample rate the
    /// headset reports and the link-state frame that moves whenever the stream is
    /// re-established. Play something to watch it change — silence is a value here,
    /// not a missing reading.
    Audio {
        /// Seconds to watch.
        #[arg(long, default_value_t = 12)]
        seconds: u64,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Print every state push the headset sends, for as long as you keep the channel open.
    ///
    /// Pushes arrive when the device decides to speak: mode changes, battery, the codec
    /// renegotiating, the wear sensor changing its mind. The equaliser tables are **not**
    /// pushed when another client writes them, which matters because something does — your
    /// curve was found rewritten one evening with nobody's hands on it. `--changes` polls
    /// those tables and prints only what moved, and when.
    Watch {
        /// Seconds to keep going. `0` runs until you interrupt it.
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// Poll the equaliser and the personalization block, printing only what changed.
        #[arg(long)]
        changes: bool,
        /// With `--changes`, seconds between polls. Not smaller: this stack reconnects for
        /// every poll, and the headset answers exactly one control connection at a time.
        #[arg(long, default_value_t = 5)]
        every: u64,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
        /// Only listen: send nothing and print raw frames as they arrive.
        #[arg(long)]
        raw: bool,
    },
    /// Ask the firmware which features it implements, in both vendor namespaces, and print
    /// each one beside what this program has actually seen that feature do.
    ///
    /// This is the question you would answer by taking the app apart, asked of the device
    /// instead: the firmware returns its own feature list with versions. The raw bytes are
    /// printed too, because the `(feature, version)` pairing is read off Qualcomm's
    /// documented shape and deserves to be re-checked by eye.
    Features {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Turn active noise control on or off.
    Anc {
        /// on | off
        state: String,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Set the transparent-hearing level, 0..=100.
    ///
    /// Its zero is not off. The vendor's own decode of this byte is **0 = maximum noise
    /// cancelling, 50 = neutral, 100 = maximum transparency**, so `transparency 0` asks for
    /// the loudest cancelling the device has, not silence. Two things mute this control
    /// rather than moving it: while adaptive noise control is on the headset ignores the
    /// value, and with the noise-control master switch off, changing it is inaudible.
    Transparency {
        level: u8,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Turn bass boost on or off.
    Bass {
        /// on | off
        state: String,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Read the hearing profile, or switch personalization on and off.
    ///
    /// With no argument this reads the mode and prints the whole profile as numbers: six
    /// thresholds, six ratios, three gains and the two preset indices it was built from.
    /// With `on` or `off` it writes `0x2000` with `enabled` or `disabled`, which is the only
    /// thing any known client writes into this block, and reads it back. The hearing test
    /// that produces the profile is not here — it plays tones through the phone's microphone
    /// and lives entirely in the app.
    Personalization {
        /// on | off — omit to read only
        state: Option<String>,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Read or set playback volume. Not a headset command: volume is negotiated
    /// by Windows over AVRCP, so this drives the audio endpoint — the same path
    /// as the tray slider, which for Bluetooth headphones moves the headset.
    Volume {
        /// 0..=100. Omit to read the current level.
        level: Option<u8>,
        /// Playback device by name or endpoint id; empty means the default one.
        #[arg(long, default_value = "")]
        filter: String,
        /// List playback devices with their levels.
        #[arg(long)]
        list: bool,
    },
    /// Mute or unmute playback, or read the state. An endpoint property, not a
    /// volume of zero: the level survives a mute.
    Mute {
        /// on | off. Omit to read the current state.
        state: Option<String>,
        /// Playback device by name or endpoint id; empty means the default one.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Send read-only queries and print whatever answers, for finding out what
    /// a firmware actually supports.
    Probe {
        /// Seconds to wait for an answer after each probe.
        #[arg(long, default_value_t = 2)]
        seconds: u64,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
        /// Vendor id as hex, e.g. 495. Only with a specific --cmd.
        #[arg(long)]
        vendor: Option<String>,
        /// Command id as hex, e.g. 1A05, or a span like 1000-101f, which sends the
        /// same payload to every id in it. That one frame instead of the standard
        /// matrix.
        #[arg(long)]
        cmd: Option<String>,
        /// Payload bytes as hex pairs, space or comma separated. Only with --cmd.
        #[arg(long, default_value = "")]
        payload: String,
        /// RFCOMM server channel to dial, e.g. 1. This headset publishes the
        /// control service on two channels; without this the first one the OS
        /// lists is used, which is what every measurement here was taken over.
        #[arg(long)]
        channel: Option<u32>,
        /// Sweep feature registrations instead of single commands: a hex byte spec,
        /// `01-1f` or `01,08,0a`. Registration is how this device is read at all —
        /// it starts pushing its tables to whoever asked — and it asks for reports
        /// rather than changing settings, which is why walking the command space
        /// this way is safe and firing unrecognised ids with empty payloads is not.
        #[arg(long)]
        features: Option<String>,
        /// Declare these features — the same byte spec as `--features` — and wait for
        /// their tables, *then* send the commands. A device is entitled to answer a
        /// command only once the client has declared the feature it belongs to, so a
        /// refusal on a link that declared nothing is not yet evidence about the
        /// command.
        #[arg(long)]
        register: Option<String>,
    },
}

/// The subcommands of `momentum eq`. The verbs are the four things a band of this filter
/// bank carries, named as the device names them on the wire.
///
/// Only `gain` has been confirmed audible by ear. `freq`, `q` and `type` are acknowledged
/// and read back changed, which is a weaker claim than "they do something" — the wide gain
/// store is already a table on this device that acknowledges, persists and stays silent.
#[derive(Subcommand)]
enum EqAction {
    /// Print every band: centre frequency, Q, filter type and gain in decibels.
    Show {
        /// Also print the wide gain store and the `0x1013` state — writable, persistent,
        /// and never yet heard to affect anything.
        #[arg(long)]
        raw: bool,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Change one or more parameters of one band in a single connection, and put them
    /// back after a counted hold. This is the form for listening: one line carries the
    /// whole change, `--hold` does the undo, and the values it restores to are printed
    /// first so a hand restore is possible if you interrupt it.
    ///
    /// `momentum eq set 0 --gain 6 --hold 12` is the entire biquad test. Without
    /// `--hold` the change stays, as with the single-parameter commands.
    ///
    /// Of the four, only `--gain` has been heard to change the sound. `--freq`, `--q` and
    /// `--type` are stored, acknowledged and read back exactly as written, and reached
    /// nothing audible when tested against a +6 dB band.
    ///
    /// One caution: run one hold at a time. A second process reads the *live* values as
    /// what to restore to, so two overlapping holds restore each other's changes.
    Set {
        /// Band index, as `momentum eq show` prints it.
        band: u8,
        /// Gain in decibels, signed tenths on the wire: `6`, `-3`, `2.5`.
        #[arg(long, allow_hyphen_values = true)]
        gain: Option<f32>,
        /// Centre frequency in hertz, 10 to 24000.
        #[arg(long)]
        freq: Option<u16>,
        /// Q as a decimal — `0.71` is 1/√2, `12` a narrow peak. Sent as Q12.
        #[arg(long)]
        q: Option<f32>,
        /// Filter type, an opaque small integer; `14` on every band out of the factory.
        #[arg(long, visible_alias = "type")]
        kind: Option<u8>,
        /// Hold the change this many seconds, then restore what was there. Nothing is
        /// restored if you kill the program first — the values it will restore to are
        /// printed before the count begins.
        #[arg(long)]
        hold: Option<u64>,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Move one band's gain, in decibels, and read it back. The one table whose effect on
    /// the sound has been confirmed by ear; the headset keeps the curve after power-off.
    ///
    /// Only heard in the `userEq` audio mode, which is what a headset running its own
    /// equaliser sits in. A write landing while another mode is active is real, stored,
    /// and inaudible.
    Gain {
        /// Band index, as `momentum eq show` prints it.
        band: u8,
        /// Decibels: `-3`, `2.5`, `0`. Stored as signed tenths, so a leading minus is a
        /// value and not an option.
        #[arg(allow_hyphen_values = true)]
        db: f32,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Set one band's frequency, in hertz.
    ///
    /// Stored, acknowledged, and re-read exactly as written — and not heard. With +6 dB on
    /// a band, moving it from 90 Hz to 4 kHz and back made no difference, twice over, on
    /// two different days. The audio path builds its filters from its own table; what this
    /// command writes is what the read-back reports, which is worth knowing before you try
    /// to tune by ear.
    Freq {
        /// Band index.
        band: u8,
        /// Hertz, 10 to 24000. A band with no gain has no sound to move, so raise one
        /// with `eq gain` first if you are listening.
        hz: u16,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Set a band's Q as a decimal: `0.71` is 1/√2, the value every band ships with except
    /// the first, `12` is a narrow peak. Sent as Q12, which is what the app's own encoder
    /// multiplies by.
    ///
    /// Stored and read back like the frequency, and measured the same way: not heard.
    Q {
        /// Band index.
        band: u8,
        /// Q as a decimal, 0.05 to 16.
        #[arg(allow_hyphen_values = true)]
        q: f32,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Set a band's filter type: an opaque small integer, `14` on every band out of the
    /// factory. Nobody has published the enumeration, so this is exploration — `show`
    /// afterwards, and put it back with the number you started from.
    ///
    /// Stored and read back like the frequency and Q. Those two were listened for and were
    /// not there; the type has not been listened for, and there is no reason to expect it
    /// to be read by an audio path that ignores its neighbours.
    #[command(visible_alias = "type")]
    Kind {
        /// Band index.
        band: u8,
        /// The type, 0 to 255.
        kind: u8,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Zero every band's gain — the curve the factory ships, and the fastest way out of a
    /// sound you no longer want.
    Flat {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Interactive tuner: type an edit, `r` restores the snapshot taken at start-up. One
    /// connection, so an edit lands in tens of milliseconds instead of the second a fresh
    /// connection costs.
    ///
    /// What you can tune is one-dimensional. Gains are the parameter measured to reach the
    /// ears; frequency, Q and filter type are accepted, stored and read back exactly as
    /// written, and were not heard when tested against a band carrying +6 dB. So `f`, `q`
    /// and `t` shape what `eq show` reports rather than what you hear — the keys stay
    /// because the store is real, and someone will eventually find out what the type field
    /// is for.
    Tune {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
}

fn main() -> Result<(), Boxed> {
    let cli = Cli::parse();
    // The RFCOMM commands are blocking WinRT calls; they never need a runtime.
    #[cfg(windows)]
    if let Some(result) = run_windows(&cli.command) {
        return result;
    }
    // btleplug's Windows backend is WinRT, whose Bluetooth callbacks arrive in
    // the apartment that registered them, so the runtime has to be single
    // threaded. `enable_time`, not `enable_all`: btleplug needs timers, never
    // the mio IO driver.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(run(cli.command))
}

async fn run(command: Command) -> Result<(), Boxed> {
    match command {
        Command::Scan { seconds, filter } => scan(seconds, &filter).await,
        Command::Gatt {
            address,
            filter,
            seconds,
            no_read,
            notify,
        } => {
            let address = match address.as_deref() {
                Some(text) => Some(
                    text.parse::<BDAddr>()
                        .map_err(|e| format!("bad address {text}: {e}"))?,
                ),
                None => None,
            };
            gatt(address, &filter, seconds, no_read, notify).await
        }
        // The RFCOMM commands are handled in main before the runtime exists.
        _ => Err("this command needs the Windows control channel".into()),
    }
}

/// Run the RFCOMM commands. Returns `None` for the BLE subcommands, which are
/// async and belong in the runtime.
#[cfg(windows)]
fn run_windows(command: &Command) -> Option<Result<(), Boxed>> {
    match command {
        Command::RfScan { filter, uuid } => Some(rf_scan(filter, uuid.as_deref())),
        Command::Status { filter } => Some(status(filter)),
        Command::Info { filter } => Some(info(filter)),
        Command::Eq(action) => Some(eq(action)),
        Command::Mode { mode, filter } => Some(audio_mode(*mode, filter)),
        Command::Ramp1 { filter } => Some(ramp(filter, &RAMP_UP)),
        Command::Ramp2 { filter } => Some(ramp(filter, &RAMP_DOWN)),
        Command::Ramp0 { filter } => Some(ramp(filter, &RAMP_FLAT)),
        Command::Host { serve, name } => Some(host(*serve, name)),
        Command::Watch {
            seconds,
            changes,
            every,
            filter,
            raw,
        } => Some(if *changes {
            eq_watch(*seconds, *every, filter)
        } else {
            watch(*seconds, filter, *raw)
        }),
        Command::Audio { seconds, filter } => Some(audio(*seconds, filter)),
        Command::Features { filter } => Some(features(filter)),
        Command::Anc { state, filter } => Some(anc(state, filter)),
        Command::Transparency { level, filter } => Some(transparency(*level, filter)),
        Command::Bass { state, filter } => Some(bass(state, filter)),
        Command::Personalization { state, filter } => {
            Some(personalization(state.as_deref(), filter))
        }
        Command::Volume {
            level,
            filter,
            list,
        } => Some(volume(*level, filter, *list)),
        Command::Mute { state, filter } => Some(mute(state.as_deref(), filter)),
        Command::Steps {
            features,
            channel,
            out,
            script,
            filter,
        } => Some(steps(
            features,
            filter,
            *channel,
            out.as_deref(),
            script.as_deref(),
        )),
        Command::Probe {
            seconds,
            filter,
            vendor,
            cmd,
            payload,
            channel,
            features,
            register,
        } => Some(probe(&ProbeRequest {
            seconds: *seconds,
            filter,
            vendor: vendor.as_deref(),
            cmd: cmd.as_deref(),
            payload,
            channel: *channel,
            features: features.as_deref(),
            register: register.as_deref(),
        })),
        _ => None,
    }
}

#[cfg(windows)]
fn rf_scan(filter: &str, uuid: Option<&str>) -> Result<(), Boxed> {
    use momentum::rfcomm;
    let uuid = uuid.unwrap_or(momentum::GAIA_SERVICE_UUID);
    let needle = if filter.is_empty() {
        None
    } else {
        Some(filter)
    };
    let targets = rfcomm::find(uuid, needle)?;
    if targets.is_empty() {
        println!("no paired device publishes RFCOMM service {uuid}");
        println!("the headset needs to be paired in Windows and switched on for this to appear");
    }
    for target in &targets {
        println!(
            "{:<16} {:<18} channel {}  ({})",
            target.name,
            rfcomm::peer_address(&target.device_id).unwrap_or_default(),
            rfcomm::channel(&target.device_id)
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into()),
            target.device_id,
        );
    }
    Ok(())
}

/// Connect, subscribe to pushes and hand back a live session.
///
/// Windows lists one entry per resolved channel, and the headset accepts only
/// some of them depending on its state, so candidates are tried in turn.
#[cfg(windows)]
fn open_session(filter: &str) -> Result<momentum::Session<momentum::rfcomm::WindowsRfcomm>, Boxed> {
    connect_first(filter, true, None)
}

/// Connect to the first channel that answers. `register` decides whether to
/// hand the headset a feature registration and wait for its ack, which is what
/// a real session needs and a passive listener must not do. `only` pins the
/// attempt to one RFCOMM server channel: this headset publishes the same service
/// on two of them, and everything ever measured here went to whichever the OS
/// happened to list first.
#[cfg(windows)]
fn connect_first(
    filter: &str,
    register: bool,
    only: Option<u32>,
) -> Result<momentum::Session<momentum::rfcomm::WindowsRfcomm>, Boxed> {
    let needle = if filter.is_empty() {
        None
    } else {
        Some(filter)
    };
    let found = momentum::rfcomm::find(momentum::GAIA_SERVICE_UUID, needle)?;
    if found.is_empty() {
        return Err(format!(
            "nothing publishes the control service ({}). Is the headset switched on? \
             Try `momentum rf-scan`.",
            momentum::GAIA_SERVICE_UUID
        )
        .into());
    }
    let targets: Vec<_> = match only {
        None => found,
        Some(want) => found
            .into_iter()
            .filter(|target| momentum::rfcomm::channel(&target.device_id) == Some(want))
            .collect(),
    };
    if targets.is_empty() {
        return Err(format!(
            "this device publishes no control service on channel {}; `momentum rf-scan` \
             lists the channels it does.",
            only.unwrap_or_default()
        )
        .into());
    }
    let mut last_error = None;
    for target in &targets {
        println!(
            "connecting to {} ({}) on channel {} ...",
            target.name,
            momentum::rfcomm::peer_address(&target.device_id).unwrap_or_default(),
            momentum::rfcomm::channel(&target.device_id)
                .map_or_else(|| "?".to_string(), |c| c.to_string()),
        );
        let link = match momentum::rfcomm::connect(target) {
            Ok(link) => link,
            Err(err) => {
                println!("  no channel: {err}");
                last_error = Some(err);
                continue;
            }
        };
        let mut session = momentum::Session::new(link);
        if register && let Err(err) = session.open(&momentum::DEFAULT_FEATURES) {
            println!("  no reply: {err}");
            println!("  link: {}", momentum::Link::diagnostics(session.link()));
            last_error = Some(err);
            continue;
        }
        return Ok(session);
    }
    Err(match last_error {
        Some(err) => err.to_string().into(),
        None => "no candidate accepted the connection".into(),
    })
}

#[cfg(windows)]
fn status(filter: &str) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    session.refresh()?;
    // Not a failure if this firmware keeps its labels to itself; the session
    // records the reason in `warnings` below.
    let identity = session.identity().ok().flatten();
    println!("{}", session.state.summary());
    if let Some(identity) = identity {
        println!("device: {identity}");
    }
    for warning in &session.warnings {
        println!("note: {warning}");
    }
    Ok(())
}

/// Change several parameters of one band over one connection, with an optional counted
/// restore, because listening to a change means living with it for a few seconds and then
/// not living with it.
///
/// The values it will restore to are printed **before** the hold, not only after: the
/// only way to abandon a hold is to interrupt the program, and an interrupted restore is
/// one you have to finish by hand from numbers you can no longer see.
#[cfg(windows)]
fn eq_set(
    band: u8,
    gain: Option<f32>,
    freq: Option<u16>,
    q: Option<f32>,
    kind: Option<u8>,
    hold: Option<u64>,
    filter: &str,
) -> Result<(), Boxed> {
    if gain.is_none() && freq.is_none() && q.is_none() && kind.is_none() {
        return Err("nothing to change: give --gain, --freq, --q or --type".into());
    }
    if let Some(hz) = freq.filter(|hz| !(10..=24000).contains(hz)) {
        return Err(format!("{hz} Hz is outside 10..24000").into());
    }
    if let Some(raw_q) = q.filter(|raw_q| !raw_q.is_finite() || !(0.05..=16.0).contains(raw_q)) {
        return Err(format!("{raw_q} is not a Q between 0.05 and 16").into());
    }
    let tenths = match gain {
        Some(db) => {
            let scaled = (f64::from(db) * 10.0).round();
            if !db.is_finite() || scaled.abs() > 127.0 {
                return Err(
                    format!("{db} dB is not something a signed byte of tenths can hold").into(),
                );
            }
            Some(scaled as i8)
        }
        None => None,
    };

    let mut session = open_session(filter)?;
    let eq = session.equaliser()?.clone();
    let index = usize::from(band);
    if index >= eq.bands() {
        return Err(format!(
            "this equaliser describes {} bands, so there is no band {band}",
            eq.bands()
        )
        .into());
    }
    let at = |row: &[Option<u16>]| row.get(index).copied().flatten().unwrap_or(0);
    let was = Band {
        hz: at(&eq.hz),
        q: at(&eq.q12),
        kind: at(&eq.filter_types),
        tenths: eq
            .gains_db()
            .get(index)
            .copied()
            .map_or(0, |db| (f64::from(db) * 10.0).round() as i8),
    };

    if let Some(value) = tenths {
        session.eq_set_u8(cmd::SET_EQ_GAIN, band, value as u8)?;
    }
    if let Some(hz) = freq {
        session.eq_set_u16(cmd::SET_EQ_FREQUENCY, band, hz)?;
    }
    if let Some(raw_q) = q {
        session.eq_set_u16(
            cmd::SET_EQ_Q,
            band,
            (f64::from(raw_q) * 4096.0).round() as u16,
        )?;
    }
    if let Some(value) = kind {
        session.eq_set_u8(cmd::SET_EQ_FILTER_TYPE, band, value)?;
    }
    session.forget_equaliser();
    print_bank(&session.equaliser()?);

    let Some(seconds) = hold else {
        return Ok(());
    };
    println!(
        "holding {seconds}s, then restoring band {band} to {} Hz, Q {:.3}, type {}, {:+.1} dB.",
        was.hz,
        f64::from(was.q) / 4096.0,
        was.kind,
        f64::from(was.tenths) / 10.0,
    );
    std::thread::sleep(Duration::from_secs(seconds));

    if tenths.is_some() {
        session.eq_set_u8(cmd::SET_EQ_GAIN, band, was.tenths as u8)?;
    }
    if freq.is_some() {
        session.eq_set_u16(cmd::SET_EQ_FREQUENCY, band, was.hz)?;
    }
    if q.is_some() {
        session.eq_set_u16(cmd::SET_EQ_Q, band, was.q)?;
    }
    if kind.is_some() {
        session.eq_set_u8(cmd::SET_EQ_FILTER_TYPE, band, was.kind as u8)?;
    }
    session.forget_equaliser();
    print_bank(&session.equaliser()?);
    for warning in &session.warnings {
        println!("note: {warning}");
    }
    Ok(())
}

/// Move one band's gain, then read that band back, so the number printed is the number
/// the headset is storing rather than the one we asked for.
#[cfg(windows)]
fn eq_gain(band: u8, db: f32, filter: &str) -> Result<(), Boxed> {
    let tenths = (f64::from(db) * 10.0).round();
    if !db.is_finite() || tenths.abs() > 127.0 {
        return Err(format!("{db} dB is not something a signed byte of tenths can hold").into());
    }
    let mut session = open_session(filter)?;
    match session.set_eq_gain(band, tenths as i8)? {
        Some(read) => println!(
            "band {band}: asked for {:+.1} dB, the headset now reports {:+.1} dB.",
            db,
            f32::from(read) / 10.0
        ),
        // Saying so beats reporting the value we wanted.
        None => println!("band {band}: the write was acked; the read-back carried no byte."),
    }
    println!("that is audible now, and the headset keeps the curve when it is switched off.");
    Ok(())
}

/// Write one parameter of one band, then read the whole bank back.
///
/// The re-read is the point: the session drops its cached tables and re-registers the
/// feature, so what prints is a fresh push from the headset. Without it, "band 0 now at
/// 4000 Hz" would only mean that we asked. Note what the ack does and does not say — the
/// store took the value; whether the audio path consults it is a matter for your ears,
/// and the wide gain store is precedent that a writable table can be silent.
#[cfg(windows)]
fn eq_param(
    band: u8,
    what: String,
    command: u16,
    value: u16,
    wide: bool,
    filter: &str,
) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    let bands = session.equaliser()?.bands();
    if usize::from(band) >= bands {
        return Err(
            format!("this equaliser describes {bands} bands, so there is no band {band}").into(),
        );
    }
    if wide {
        session.eq_set_u16(command, band, value)?;
    } else {
        session.eq_set_u8(command, band, value as u8)?;
    }
    println!("band {band}: asked for {what}.");
    session.forget_equaliser();
    print_bank(&session.equaliser()?);
    for warning in &session.warnings {
        println!("note: {warning}");
    }
    Ok(())
}

/// Zero every gain, which is the factory curve and the way out of a sound you no longer
/// want. Only the gains: the filter bank's frequencies, Qs and types are left alone,
/// because nothing has established that resetting those is harmless.
#[cfg(windows)]
fn eq_flat(filter: &str) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    let bands = session.equaliser()?.bands();
    if bands == 0 {
        return Err("the headset sent no equaliser tables, so there is nothing to flatten".into());
    }
    for band in 0..bands as u8 {
        session.eq_set_u8(cmd::SET_EQ_GAIN, band, 0)?;
    }
    session.forget_equaliser();
    print_bank(&session.equaliser()?);
    Ok(())
}

/// One band, in the units the wire carries, which is the only honest way to hold it.
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq)]
struct Band {
    hz: u16,
    q: u16,
    kind: u16,
    tenths: i8,
}

/// Write every band of `want` and keep `now` in step with what the headset has.
///
/// A failure stops the pass rather than ploughing on: half a snapshot applied is worse
/// than none, because the table on screen and the table in the headphone would then
/// disagree in a way nobody can see.
#[cfg(windows)]
fn apply_bands(
    session: &mut momentum::Session<momentum::rfcomm::WindowsRfcomm>,
    want: &[Band],
    now: &mut [Band],
) -> Result<(), Boxed> {
    for (index, band) in want.iter().enumerate() {
        let band = *band;
        let at = index as u8;
        session.eq_set_u8(cmd::SET_EQ_GAIN, at, band.tenths as u8)?;
        session.eq_set_u16(cmd::SET_EQ_FREQUENCY, at, band.hz)?;
        session.eq_set_u16(cmd::SET_EQ_Q, at, band.q)?;
        session.eq_set_u8(cmd::SET_EQ_FILTER_TYPE, at, band.kind as u8)?;
        now[index] = band;
    }
    Ok(())
}

#[cfg(windows)]
fn print_bands(bands: &[Band]) {
    println!(
        "{:<5} {:>7}  {:>7}  {:>5}  {:>7}",
        "band", "Hz", "Q", "type", "dB"
    );
    for (index, band) in bands.iter().enumerate() {
        println!(
            "{:<5} {:>7}  {:>7.3}  {:>5}  {:>+7.1}",
            index,
            band.hz,
            f64::from(band.q) / 4096.0,
            band.kind,
            f64::from(band.tenths) / 10.0,
        );
    }
}

/// Interactive equaliser tuner, made for listening rather than for scripting.
///
/// The tables are read once, each edit goes out on Enter, and `r` restores the snapshot
/// taken at the start. That is the whole design: the frequency, Q and filter type of
/// every band are writable, and whether the audio path consults them is settled by your
/// ears in ten seconds, not by a frame log. Nothing here pretends to know whether a
/// write was audible — the tool reports that the store accepted it, nothing more.
#[cfg(windows)]
fn eq_tune(filter: &str) -> Result<(), Boxed> {
    use std::io::Write;

    let mut session = open_session(filter)?;
    let eq = session.equaliser()?;
    let gains = eq.gains_db();
    let at = |row: &[Option<u16>], index: usize| row.get(index).copied().flatten().unwrap_or(0);
    let mut current: Vec<Band> = (0..eq.bands())
        .map(|index| Band {
            hz: at(&eq.hz, index),
            q: at(&eq.q12, index),
            kind: at(&eq.filter_types, index),
            tenths: (gains.get(index).copied().unwrap_or(0.0) * 10.0).round() as i8,
        })
        .collect();
    let snapshot = current.clone();
    print_bands(&current);
    println!(
        "b <band> <dB> · f <band> <Hz> · q <band> <Q> · t <band> <type> · \
         f all <Hz> sets every band"
    );
    println!("g show · r restore the snapshot · x quit restoring · ! quit as it stands · ? help");

    let stdin = std::io::stdin();
    loop {
        print!("eq> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(head) = words.first().copied() else {
            continue;
        };
        match head {
            "g" => print_bands(&current),
            "?" => println!(
                "b/f/q/t take a band index from the table above. dB is decibels, Hz hertz, \
                 Q the decimal Q (0.71 is what the device ships), type the small integer. \
                 `r` restores what you started with; `!` leaves it as it is."
            ),
            "!" => break,
            "r" | "x" => {
                apply_bands(&mut session, &snapshot, &mut current)?;
                print_bands(&current);
                println!("restored to what it was when this session opened.");
                if head == "x" {
                    break;
                }
            }
            key @ ("b" | "f" | "q" | "t") => {
                let mut words = words.iter().skip(1);
                let (Some(where_which), Some(value)) = (words.next(), words.next()) else {
                    println!("`{key}` wants a band and a value, like `{} 0 4000`", key);
                    continue;
                };
                let spread = where_which.eq_ignore_ascii_case("all");
                let index = if spread {
                    0
                } else {
                    match where_which.parse::<usize>() {
                        Ok(index) if index < current.len() => index,
                        Ok(index) => {
                            println!(
                                "there is no band {index}: this equaliser has {} bands, 0..{}",
                                current.len(),
                                current.len() - 1
                            );
                            continue;
                        }
                        Err(_) => {
                            println!("`{where_which}` is neither a band number nor `all`");
                            continue;
                        }
                    }
                };
                let count = if spread { current.len() } else { 1 };
                for offset in 0..count {
                    let band = (index + offset) as u8;
                    let outcome = match (key, value.parse::<f64>()) {
                        ("b", Ok(db)) if db.abs() <= 12.7 => {
                            let tenths = (db * 10.0).round() as i8;
                            session
                                .eq_set_u8(cmd::SET_EQ_GAIN, band, tenths as u8)
                                .map(|()| Band {
                                    tenths,
                                    ..current[usize::from(band)]
                                })
                        }
                        ("f", Ok(hz)) if (10.0..=24000.0).contains(&hz) => {
                            let hz = hz.round() as u16;
                            session
                                .eq_set_u16(cmd::SET_EQ_FREQUENCY, band, hz)
                                .map(|()| Band {
                                    hz,
                                    ..current[usize::from(band)]
                                })
                        }
                        ("q", Ok(q)) if (0.05..=16.0).contains(&q) => {
                            let raw = (q * 4096.0).round() as u16;
                            session.eq_set_u16(cmd::SET_EQ_Q, band, raw).map(|()| Band {
                                q: raw,
                                ..current[usize::from(band)]
                            })
                        }
                        ("t", Ok(kind)) if (0.0..=255.0).contains(&kind) => {
                            let kind = kind as u8;
                            session
                                .eq_set_u8(cmd::SET_EQ_FILTER_TYPE, band, kind)
                                .map(|()| Band {
                                    kind: kind as u16,
                                    ..current[usize::from(band)]
                                })
                        }
                        (_, Ok(_)) => {
                            println!(
                                "out of range: dB within ±12.7 here, Hz 10..24000, Q 0.05..16, type 0..255"
                            );
                            continue;
                        }
                        _ => {
                            println!("`{value}` is not a number");
                            continue;
                        }
                    };
                    match outcome {
                        Ok(updated) => current[usize::from(band)] = updated,
                        Err(err) => println!("band {band}: {err}"),
                    }
                }
                print_bands(&current);
            }
            other => println!("`{other}` is not a command; ? lists them"),
        }
    }
    Ok(())
}

/// The audio modes as Smart Control Plus names them. The numbering is shared across that
/// app's whole product family, so a name here is a claim about the enum, not about this
/// firmware: only `off`, `usereq` and `podcast` have been seen to do anything on a
/// MOMENTUM 5, and `parametric` was accepted while nobody could hear a difference.
fn mode_name(mode: u8) -> &'static str {
    match mode {
        0 => "off",
        1 => "usereq",
        2 => "podcast",
        3 => "personalized",
        4 => "parametric",
        5 => "hearing",
        _ => "unknown",
    }
}

/// Accept either the name or the number, because `momentum mode 1` is not something
/// anyone remembers without looking up.
fn parse_mode(raw: &str) -> Result<u8, String> {
    if let Ok(number) = raw.parse::<u8>() {
        return if number <= 5 {
            Ok(number)
        } else {
            Err(format!(
                "{number} is outside the 0..5 the app's enumeration defines"
            ))
        };
    }
    match raw.to_ascii_lowercase().as_str() {
        "off" => Ok(0),
        "usereq" | "eq" | "user" => Ok(1),
        "podcast" | "podcastmode" => Ok(2),
        "personalized" | "personalizedsound" => Ok(3),
        "parametric" | "parametriceq" | "peq" => Ok(4),
        "hearing" | "hearingenhancement" => Ok(5),
        other => Err(format!(
            "`{other}` is neither a number 0..5 nor one of off, usereq, podcast, \
             personalized, parametric, hearing"
        )),
    }
}

/// Print a mode payload — two bytes big-endian — as the number and the name.
fn mode_text(payload: &[u8]) -> String {
    let raw = u16::from_be_bytes([
        payload.first().copied().unwrap_or(0),
        payload.get(1).copied().unwrap_or(0),
    ]);
    let mode = u8::try_from(raw).map_or(255, |small| if small <= 5 { small } else { 255 });
    format!("{raw} ({})", mode_name(mode))
}

/// Show or change the audio mode, reading it back either way.
#[cfg(windows)]
fn audio_mode(mode: Option<u8>, filter: &str) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    let Some(want) = mode else {
        println!("audio mode: {}.", mode_text(&session.audio_mode()?));
        return Ok(());
    };
    match session.set_audio_mode(u16::from(want)) {
        Ok(payload) => {
            println!("audio mode: {}.", mode_text(&payload));
            // The device reports this mode as already active when it is already active,
            // so silence here means the mode was already what you asked for.
            if want == 4 {
                println!(
                    "this firmware accepts mode 4 and sits in it; no parametric parameter \
                          store has been found on the wire, and no ear has attributed any change \
                          of sound to entering it."
                );
            }
            if want != 1 {
                println!(
                    "the user equaliser is not heard in this mode — the curve stays stored and silent."
                );
            }
        }
        // `already` arrives as an error frame, which is information rather than failure.
        Err(err) => {
            println!("mode {want} ({}) was not accepted: {err}", mode_name(want));
            println!("audio mode: {}.", mode_text(&session.audio_mode()?));
        }
    }
    for warning in &session.warnings {
        println!("note: {warning}");
    }
    Ok(())
}

/// The equaliser commands: one connection per invocation, and every write prints the
/// bank as the headset reported it afterwards rather than as the request asked for it.
#[cfg(windows)]
fn eq(action: &EqAction) -> Result<(), Boxed> {
    match action {
        EqAction::Show { raw, filter } => eq_show(filter, *raw),
        EqAction::Set {
            band,
            gain,
            freq,
            q,
            kind,
            hold,
            filter,
        } => eq_set(*band, *gain, *freq, *q, *kind, *hold, filter),
        EqAction::Gain { band, db, filter } => eq_gain(*band, *db, filter),
        EqAction::Freq { band, hz, filter } => eq_param(
            *band,
            format!("frequency {hz} Hz"),
            cmd::SET_EQ_FREQUENCY,
            *hz,
            true,
            filter,
        ),
        EqAction::Q { band, q, filter } => {
            if !q.is_finite() || !(0.05..=16.0).contains(q) {
                return Err(format!("{q} is not a Q between 0.05 and 16").into());
            }
            eq_param(
                *band,
                format!("Q {q}"),
                cmd::SET_EQ_Q,
                (f64::from(*q) * 4096.0).round() as u16,
                true,
                filter,
            )
        }
        EqAction::Kind { band, kind, filter } => eq_param(
            *band,
            format!("filter type {kind}"),
            cmd::SET_EQ_FILTER_TYPE,
            u16::from(*kind),
            false,
            filter,
        ),
        EqAction::Flat { filter } => eq_flat(filter),
        EqAction::Tune { filter } => eq_tune(filter),
    }
}

/// One row per band, in the units the device itself uses. Only `dB` is calibrated; the
/// frequency, Q and type are the device's own numbers, printed as numbers rather than
/// converted into an interpretation of them.
fn print_bank(eq: &momentum::proto::state::Equaliser) {
    use momentum::APP_BANDS_HZ;

    let cell = |row: &[Option<u16>], band: usize| {
        row.get(band)
            .copied()
            .flatten()
            .map_or_else(|| "-".to_string(), |value| value.to_string())
    };
    let db = eq.gains_db();
    let q = |band: usize| {
        eq.q12.get(band).copied().flatten().map_or_else(
            || "-".to_string(),
            |raw| format!("{:.3}", f64::from(raw) / 4096.0),
        )
    };
    // Two frequency columns, because the two disagree and nothing on the wire says
    // which one the sliders are named after. The write commands take the band **index**,
    // which both tables agree on; the hertz are here so a reader can find the slider.
    println!(
        "{:<4} {:>8}  {:>8}  {:>7}  {:>5}  {:>6}",
        "band", "app/Hz", "Hz", "Q", "type", "dB"
    );
    for band in 0..eq.bands() {
        println!(
            "{:<4} {:>8}  {:>8}  {:>7}  {:>5}  {:>6}",
            band,
            APP_BANDS_HZ
                .get(band)
                .map_or_else(|| "-".to_string(), |hz| hz.to_string()),
            cell(&eq.hz, band),
            q(band),
            cell(&eq.filter_types, band),
            db.get(band)
                .map_or_else(|| "-".to_string(), |gain| format!("{gain:+.1}")),
        );
    }
    println!(
        "dB is calibrated: ten raw units per decibel, and the one column the ear confirms. \
         Frequency, Q and type are stored, acknowledged and read back exactly as written, \
         and were measured not to change what you hear — on firmware 6.21.1 and again on \
         6.21.5. Q is Q12 over 4096, so 0.707 is 1/√2."
    );
    println!(
        "These are the values the headset holds now, not a promise about the next boot: a \
         table written flat here came back as the app's own preset after a power-off, with \
         the phone's Bluetooth switched off. Treat this table as present tense."
    );
}

/// What moved between two readings of the equaliser, one line per parameter that differs.
///
/// This exists because the device does not push these tables when a client writes them, and
/// something on this account does write them: a curve found at one time of evening was not
/// the curve found ninety minutes later, with nobody's hands on it. So the useful question
/// is not what the values are but what changed and when — and *when* belongs to the
/// caller, who owns the clock.
fn eq_diff(
    before: &momentum::proto::state::Equaliser,
    after: &momentum::proto::state::Equaliser,
) -> Vec<String> {
    let mut out = Vec::new();
    for band in 0..before.bands().min(after.bands()) {
        let gain = |e: &momentum::proto::state::Equaliser| e.gains.get(band).copied();
        if let Some((was, now)) = gain(before)
            .zip(gain(after))
            .filter(|(was, now)| was != now)
        {
            out.push(format!(
                "band {band}: gain {:+.1} → {:+.1} dB",
                f32::from(was) / 10.0,
                f32::from(now) / 10.0
            ));
        }
        for (label, was_row, now_row) in [
            ("Hz", &before.hz, &after.hz),
            ("Q12", &before.q12, &after.q12),
            ("type", &before.filter_types, &after.filter_types),
            ("wide", &before.gains_wide, &after.gains_wide),
        ] {
            let was = was_row.get(band).copied().flatten();
            let now = now_row.get(band).copied().flatten();
            if was != now {
                out.push(format!("band {band}: {label} {was:?} → {now:?}"));
            }
        }
    }
    out
}

/// The personalization payloads worth watching, each named as narrowly as its decoding is.
fn personal_name(id: u16) -> &'static str {
    match id {
        0x2101 => "personalization mode",
        0x2103 => "thresholds",
        0x2105 => "ratios",
        0x2107 => "gains and preset indices",
        _ => "personalization payload",
    }
}

/// What moved in the personalization block between two snapshots, compared **by id** rather
/// than by position: a poll in which one request fails must report nothing rather than
/// compare two unrelated payloads that happen to share a slot in a list.
fn personal_diff(before: &[(u16, Vec<u8>)], after: &[(u16, Vec<u8>)]) -> Vec<String> {
    after
        .iter()
        .filter_map(|(id, now)| {
            before
                .iter()
                .find(|(was_id, _)| was_id == id)
                .map(|(_, was)| (id, was, now))
        })
        .filter(|(_, was, now)| was != now)
        .map(|(id, was, now)| {
            format!(
                "{} {id:#06x}: [{}] → [{}]",
                personal_name(*id),
                hex(was),
                hex(now)
            )
        })
        .collect()
}

/// Poll the equaliser tables and the personalization block, printing only what moved.
///
/// Polling is the whole method here. The device pushes these tables when it changes them
/// itself and stays silent when another client writes them, so an app rewriting your curve
/// mid-session is invisible unless you keep asking — which is exactly the case this is for.
///
/// Each poll opens its own connection and closes it again, and that is not laziness: this
/// unit's server accepts **one** control connection at a time, refusing a second on either
/// channel with `0x80070490`. A watcher that held the line would occupy the only slot and
/// so would prevent, rather than observe, the write it was waiting for. The gaps between
/// polls are where somebody else gets their turn — and therefore the only place a change
/// can come from. Collisions are part of that: a poll that finds the slot busy says so once
/// and tries again, because the thing occupying it is precisely what is being watched for.
#[cfg(windows)]
fn eq_watch(seconds: u64, every: u64, filter: &str) -> Result<(), Boxed> {
    let every = every.max(2);
    let start = Instant::now();
    let mut seen: Option<momentum::proto::state::Equaliser> = None;
    let mut seen_personal: Vec<(u16, Vec<u8>)> = Vec::new();
    let mut misses = 0u32;
    println!(
        "polling the equaliser and the personalization block every {every}s, connecting fresh \
         each time. Ctrl-C to stop."
    );
    loop {
        let mut session = match open_session(filter) {
            Ok(session) => {
                misses = 0;
                session
            }
            Err(err) => {
                misses += 1;
                if misses == 1 || misses.is_multiple_of(10) {
                    println!(
                        "t+{:6.1}s  no channel this time ({err}); somebody else is holding it",
                        start.elapsed().as_secs_f32()
                    );
                }
                if misses >= 8 {
                    println!(
                        "giving up: no channel {misses} times running. The headset is off, or something has the line."
                    );
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(every));
                continue;
            }
        };
        let now = session.equaliser()?;
        // The personalization block is polled by raw id and compared as bytes. Nothing here
        // needs to know what a threshold *is* — the question being asked is who writes it and
        // when, and decoding it would only make a wrong answer look authoritative.
        let mut personal = Vec::new();
        for id in [0x2001u16, 0x2003, 0x2005, 0x2007] {
            if let Ok(frame) = session.request(id, Vec::new(), Duration::from_millis(700)) {
                personal.push((frame.cmd, frame.payload));
            }
        }
        match &seen {
            None => {
                print_bank(&now);
                for (id, payload) in &personal {
                    println!("{id:#06x} {:<26} [{}]", personal_name(*id), hex(payload));
                }
                println!("that is the baseline; every line after it is somebody's edit.");
                for warning in &session.warnings {
                    println!("note: {warning}");
                }
            }
            Some(before) => {
                for line in eq_diff(before, &now) {
                    println!("t+{:6.1}s  {line}", start.elapsed().as_secs_f32());
                }
                for line in personal_diff(&seen_personal, &personal) {
                    println!("t+{:6.1}s  {line}", start.elapsed().as_secs_f32());
                }
            }
        }
        seen = Some(now);
        seen_personal = personal;
        if seconds != 0 && start.elapsed() >= Duration::from_secs(seconds) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(every));
    }
}

/// Ask each namespace on the headset what it implements, and print the answer next to what
/// this program has seen those features actually do.
///
/// This exists because it is a better answer than taking an APK apart: the firmware states
/// its own feature list, so "does this product have the thing" is answered by the product
/// rather than by a version of an app that may not ship its model. Both raw payloads are
/// printed, because the `(feature, version)` pairing is read off Qualcomm's documented
/// shape — and one pair of bytes misread in either direction would quietly rename every
/// feature below.
#[cfg(windows)]
fn features(filter: &str) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    for (vendor, label, sennheiser) in [
        (
            momentum::proto::frame::VENDOR_QUALCOMM,
            "vendor 0x0495",
            true,
        ),
        (0x001d, "vendor 0x001d, Qualcomm core", false),
    ] {
        match session.request_vendor(
            vendor,
            momentum::cmd::GET_SUPPORTED_FEATURES,
            Vec::new(),
            Duration::from_secs(3),
        ) {
            Ok(frame) => {
                println!("{label} declares, raw {:02x?}:", frame.payload);
                let (more, rest) = frame
                    .payload
                    .split_first()
                    .map_or((false, &[][..]), |(first, rest)| (*first != 0, rest));
                for pair in rest.chunks(2).filter(|pair| pair.len() == 2) {
                    println!(
                        "  feature {:>2} (0x{:02x}) version {:>2}  commands 0x{:02x}xx  {}",
                        pair[0],
                        pair[0],
                        pair[1],
                        u16::from(pair[0]) * 2,
                        feature_note(pair[0], sennheiser)
                    );
                }
                if more {
                    println!("  (more pages: `momentum probe --vendor … --cmd 0002`)");
                }
            }
            Err(err) => println!("{label}: no answer ({err})"),
        }
    }
    for warning in &session.warnings {
        println!("note: {warning}");
    }
    Ok(())
}

/// What a declared feature covers.
///
/// **The ids in this list are the vendor's feature numbers, not command high bytes.** A
/// feature's commands begin at `feature × 2` in the high byte, so active noise control is
/// feature `13` = `0x0d` and its commands are `0x1a00` upwards, and sound personalization is
/// `16` = `0x10` at `0x2000`. Reading the list as high bytes instead makes both of them look
/// absent from a device that answers them fluently, which is a mistake this program made and
/// recorded on 2026-11-01. (numbering from the vendor's device model, cross-checked against
/// commands this unit answers)
fn feature_note(feature: u8, sennheiser: bool) -> &'static str {
    if !sennheiser {
        // Qualcomm's core numbering is its own, and identical numbers mean unrelated things:
        // the core's `0d` is not Sennheiser's `0d`, and printing the noise-control note beside
        // it would be the exact error this command exists to avoid.
        return match feature {
            0x00 => "core: version, supported features, transport info",
            0x03 => "voice UI, the selection for a voice assistant",
            0x04 => "debug: panic-log retrieval `0x0803`, `0x0804`",
            0x06 => "device firmware update — nothing here sends any part of it",
            0x0c => "statistics: usage counters `0x1800` to `0x1802`",
            _ => "a Qualcomm core feature, not examined by this program",
        };
    }
    match feature {
        0x00 => "core: identity, and the factory reset nothing here should send",
        0x01 => "upgrade: firmware over the air",
        0x02 => "device: wear detection, physical state, and the Hi-Res switch",
        0x03 => "battery and power: level, charging, and the auto power-off timer",
        0x04 => "generic audio: codec, audio mode, sidetone, prompts, and the stream rate",
        0x08 => "user equaliser: the eight gains and the tables of metadata around them",
        0x09 => "versions: the firmware the app gates on, and the model id",
        0x0a => "device management: the paired-host list, connect, disconnect, delete",
        0x0b => "mmi: the touch map and the touch lock",
        0x0c => "transparent hearing: its own mode, status, and push",
        0x0d => "active noise control: the mode pairs, the level, and the master switch",
        0x10 => "sound personalization: mode, thresholds, ratios, and the hearing-test gains",
        _ => "not in the vendor's model of the M4, and not read by anything in this program",
    }
}

/// The equaliser as the headset reports it, and the tables it cannot explain.
#[cfg(windows)]
fn eq_show(filter: &str, raw: bool) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    let eq = session.equaliser()?;
    if eq.is_empty() {
        println!("the headset sent no equaliser tables");
        for warning in &session.warnings {
            println!("note: {warning}");
        }
        return Ok(());
    }
    print_bank(&eq);

    // The wide gain store is a table of its own again: writable, range-checked, durable,
    // never once touched by the app. It keeps a heading because it belongs to no control
    // anyone has identified.
    if raw && !eq.gains_wide.is_empty() {
        println!("\nthe wide gain store — writable, inaudible, unexplained");
        // Raw hex next to the decimal: an unexplained number is worth keeping exactly as
        // the device sent it, not as an interpretation of it.
        let hexed = |row: &[Option<u16>], at: usize| {
            row.get(at)
                .copied()
                .flatten()
                .map_or_else(|| "-".to_string(), |v| format!("{v:#06x} ({v})"))
        };
        // Stored signed — `0x1010` accepted −180 and refused −182, so two's complement is
        // a fact about representation, not a guess about units. Printing 65526 for what
        // the device means as −10 is not neutrality, it is a display that manufactures
        // mystery.
        let signed = |row: &[Option<u16>], at: usize| {
            row.get(at)
                .copied()
                .flatten()
                .map_or_else(|| "-".to_string(), |v| (v as i16).to_string())
        };
        println!("{:<4} {:>8}  {:>16}", "band", "wide", "as sent");
        for row in 0..eq.bands() {
            println!(
                "{:<4} {:>8}  {:>16}",
                row,
                signed(&eq.gains_wide, row),
                hexed(&eq.gains_wide, row),
            );
        }

        if !eq.state.is_empty() {
            println!("0x1013 state, meaning unknown: {:02x?}", eq.state);
        }
    }
    for warning in &session.warnings {
        println!("note: {warning}");
    }
    Ok(())
}

/// Product, firmware and serial, one per line.
#[cfg(windows)]
fn info(filter: &str) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    // No `refresh()`: these three frames are the whole job, so a script that
    // polls `info` does not also pay for a battery and mode read each time.
    for (label, value) in [
        ("product", session.product()?),
        ("firmware", session.firmware()?),
        ("serial", session.serial()?),
    ] {
        match value {
            Some(text) => println!("{label}: {text}"),
            None => println!("{label}: not reported"),
        }
    }
    for warning in &session.warnings {
        println!("note: {warning}");
    }
    Ok(())
}

/// The audio link as the headset itself reports it.
///
/// Deliberately not Windows' endpoint: the operating system's idea of the format is a
/// request, while `0x089a` is what the codec agreed to. The two disagree every time
/// aptX refuses the file's rate, and only one of them describes what was heard.
#[cfg(windows)]
fn audio(seconds: u64, filter: &str) -> Result<(), Boxed> {
    use momentum::cmd::feature;
    use momentum::proto::frame::VENDOR_QUALCOMM;

    let mut link = connect_first(filter, false, None)?.into_link();
    // Feature 0x10 is registered alongside the audio feature because `0x208a` has
    // arrived at every stream transition ever observed, and a frame that only shows
    // up in one of two connections is worth watching from both.
    for id in [feature::AUDIO, 0x10] {
        momentum::Link::write_frame(
            &mut link,
            &command(VENDOR_QUALCOMM, momentum::cmd::REGISTER_FEATURE, vec![id]),
        )?;
    }
    println!(
        "watching the audio link for {seconds}s — play something, pause it, switch \
         device: silence is a reading here, not an absence of one"
    );

    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut rate: Option<u32> = None;
    let mut codec: Option<u8> = None;
    while Instant::now() < deadline {
        let Some(frame) = momentum::Link::read_frame(&mut link, Duration::from_millis(500))? else {
            continue;
        };
        if frame.cmd == momentum::cmd::PUSH_SAMPLE_RATE && frame.payload.len() == 4 {
            rate = Some(u32::from_be_bytes(frame.payload[..4].try_into().unwrap()));
        } else if frame.cmd == momentum::cmd::PUSH_CODEC {
            codec = frame.payload.first().copied();
        }
        println!(
            "  t+{:6.1}s  0x{:04x} {:<22} {:02x?}{}",
            started.elapsed().as_secs_f32(),
            frame.cmd,
            momentum::cmd::name(frame.cmd),
            frame.payload,
            note(frame.cmd, &frame.payload),
        );
    }

    match rate {
        Some(0) => println!("the headset reports no stream: nothing is playing."),
        Some(hz) => println!(
            "the stream is {hz} Hz, whichever cable or radio it arrived on. Over aptX from \
             the inbox Windows driver this reads 44100 however the file is encoded; over \
             USB-C from the same PC it reads 96000, with the phone app's Codec tab saying \
             \"USB 96 kHz\" at the same moment."
        ),
        None => println!("no sample-rate frame in {seconds}s of listening."),
    }
    match codec {
        // The names below 0x0b are the vendor's, from the device model behind the phone app;
        // only `02`, `08`, `0b` and `ff` have actually been seen on this unit. `0b` is past
        // the end of their list and is not a codec at all: it appeared while audio arrived
        // over USB-C, and the app's Codec tab called that same state "USB 96 kHz". So
        // `0x0800` reports the audio *path*, one of whose values involves no encoding —
        // which is also the explanation for a 96 kHz reading this program's author
        // attributed to a Bluetooth codec last night, wrongly.
        Some(byte) => {
            let name = match byte {
                0x00 => "SBC, the Bluetooth baseline",
                0x01 => "AAC",
                0x02 => "aptX",
                0x03 => "aptX Low Latency",
                0x04 => "MP3",
                0x05 => "aptX HD",
                0x06 => "Faststream, Qualcomm's low-latency path",
                0x07 => "LHDC",
                0x08 => "aptX Adaptive",
                0x09 => "aptX Lossless — a mode of Adaptive, and never seen here",
                0x0a => "LC3, the Low Energy Audio codec",
                0x0b => "the wired path — USB-C, measured here, no codec involved",
                0xff => "nothing is streaming",
                _ => "outside the vendor's list, and unmeasured on this unit",
            };
            println!("audio path {byte:#04x}: {name}.");
        }
        None => println!("no audio-path frame arrived, so there was nothing to report."),
    }
    Ok(())
}

#[cfg(windows)]
fn watch(seconds: u64, filter: &str, raw: bool) -> Result<(), Boxed> {
    if raw {
        // Passive: nothing is written, so whatever arrives is the headset
        // talking on its own. This is how an unknown command shows up.
        let mut link = connect_first(filter, false, None)?.into_link();
        println!("listening for {seconds}s of raw traffic; nothing will be sent");
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            match momentum::Link::read_frame(&mut link, Duration::from_millis(500)) {
                Ok(Some(frame)) => println!(
                    "0x{:04x} {:<22} {:02x?}",
                    frame.cmd,
                    momentum::cmd::name(frame.cmd),
                    frame.payload,
                ),
                Ok(None) => {}
                Err(err) => {
                    println!("link ended: {err}");
                    break;
                }
            }
        }
        println!("link: {}", momentum::Link::diagnostics(&link));
        return Ok(());
    }

    let mut session = open_session(filter)?;
    session.refresh()?;
    let identity = session.identity().ok().flatten();
    println!("{}", session.state.summary());
    if let Some(identity) = identity {
        println!("device: {identity}");
    }
    println!("watching for {seconds}s: switch modes on the headset or in the Sennheiser app");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if let Some(event) = session.pump(Duration::from_millis(500))? {
            println!("{}", event.describe());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn switch(text: &str) -> Result<bool, Boxed> {
    match text.to_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Ok(true),
        "off" | "0" | "false" | "no" => Ok(false),
        other => Err(format!("expected on or off, got \"{other}\"").into()),
    }
}

/// Mute, on the same OS path as [`volume`]: the headset has no mute command of
/// its own, and a mute that survived a volume change would otherwise look like a
/// bug in this tool.
#[cfg(windows)]
fn mute(state: Option<&str>, filter: &str) -> Result<(), Boxed> {
    use momentum::volume;

    let endpoint = volume::select(filter)?;
    let was = if endpoint.muted { "muted" } else { "not muted" };
    let Some(text) = state else {
        println!("{}: {was}", endpoint.name);
        return Ok(());
    };

    let now = volume::set_muted(&endpoint, switch(text)?)?;
    let now = if now { "muted" } else { "not muted" };
    println!("{}: {was} -> {now}", endpoint.name);
    Ok(())
}

/// Volume is the one thing users expect that the control channel does not
/// implement, so it goes through the OS instead of pretending to be a headset
/// command. See [`momentum::volume`].
#[cfg(windows)]
fn volume(level: Option<u8>, filter: &str, list: bool) -> Result<(), Boxed> {
    use momentum::volume;

    if list {
        for endpoint in volume::endpoints()? {
            println!(
                "{:<2} {:<36} {:>3}%{}",
                if endpoint.is_default { "*" } else { "" },
                endpoint.name,
                endpoint.percent(),
                if endpoint.muted { "  muted" } else { "" },
            );
        }
        return Ok(());
    }

    let endpoint = volume::select(filter)?;
    let Some(wanted) = level else {
        println!(
            "{}: {}%{}",
            endpoint.name,
            endpoint.percent(),
            if endpoint.muted { " (muted)" } else { "" },
        );
        return Ok(());
    };

    let reached = volume::set_level(&endpoint, f32::from(wanted) / 100.0)?;
    let got = (f64::from(reached.clamp(0.0, 1.0)) * 100.0).round() as u8;
    println!("{}: {wanted}% -> {got}%", endpoint.name);
    if got != wanted {
        println!("note: the endpoint reports {got}%; some drivers quantise the level");
    }
    Ok(())
}

/// Sennheiser's Bluetooth SIG company id — Windows also reports it as this
/// device's `ManufacturerId` (29).
#[cfg(windows)]
const SENNHEISER_VENDOR: u16 = 0x001D;

/// Parse a hex byte spec: `01-0f`, `01,03,08`, or a single `08`. The sweep takes
/// one because walking a span id by id is tedious enough that nobody would.
#[cfg(windows)]
fn byte_spec(text: &str) -> Result<Vec<u8>, Boxed> {
    let mut out = Vec::new();
    for part in text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((lo, hi)) = part.split_once('-') {
            let (lo, hi) = (
                u8::from_str_radix(lo.trim(), 16)?,
                u8::from_str_radix(hi.trim(), 16)?,
            );
            if lo > hi {
                return Err(format!("{lo:02x}-{hi:02x} runs backwards").into());
            }
            out.extend(lo..=hi);
        } else {
            out.push(u8::from_str_radix(part, 16)?);
        }
    }
    if out.is_empty() {
        return Err("empty byte spec".into());
    }
    Ok(out)
}

/// Experiment scripts for `steps`: exact states, in order, so the headset's
/// condition at any frame is never in doubt. Wording matters here — each line says
/// what to touch, in which order, and for how long, because an unlabelled frame is
/// worth nothing.
#[cfg(windows)]
const SCRIPTS: &[(&str, &[&str])] = &[
    (
        "wear",
        &[
            "Headphones ON your head, nothing playing. Do nothing: this is the baseline.",
            "Take them OFF your head and hold them in your hands. Count ten seconds.",
            "Put them back ON your head. Count ten seconds.",
            "Still wearing them. On the phone: Bluetooth on, open Smart Control, turn \
             WEAR DETECTION ON. (It will re-apply your saved EQ — that is a different \
             table and does not disturb this one.)",
            "Take them OFF your head. Count ten seconds.",
            "Put them back ON your head. Count ten seconds.",
            "Take them off and press the power button until they switch OFF. The listener \
             ends by itself when they do — with `--out`, the transcript is already saved.",
        ],
    ),
    (
        "buttons",
        &[
            "Headphones ON your head, nothing playing. Do nothing: this is the baseline.",
            "Press the noise-control button ONCE.",
            "Press it once more to go back to where you started.",
            "Press and hold the noise-control button to switch to transparency.",
            "Volume up twice, then volume down twice.",
            "Press play on the phone, count five seconds, press pause.",
        ],
    ),
];

/// Drive an experiment by hand: every Enter closes a step, so the frames in each
/// block belong to whatever was done while that block was open.
///
/// Deliberately synchronous. Listening in the background and hoping the physical
/// action lands inside the window is fragile in exactly the way this is not: the
/// human decides when a step begins and ends, and the transcript says so.
#[cfg(windows)]
fn steps(
    features: &str,
    filter: &str,
    channel: Option<u32>,
    out: Option<&str>,
    script: Option<&str>,
) -> Result<(), Boxed> {
    use std::io::{BufRead, Write as _};
    use std::sync::mpsc;

    let mut link = connect_first(filter, false, channel)?.into_link();
    let ids = byte_spec(features)?;
    let mut file = out.map(std::fs::File::create).transpose()?;
    // One path to the screen and to the file, so the transcript cannot drift from
    // what was watched.
    let mut say = |line: &str| {
        println!("{line}");
        if let Some(file) = file.as_mut() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    };

    for id in ids {
        let frame = command(
            momentum::proto::frame::VENDOR_QUALCOMM,
            momentum::cmd::REGISTER_FEATURE,
            vec![id],
        );
        say(&format!(">> register feature {id:02x}"));
        momentum::Link::write_frame(&mut link, &frame)?;
    }

    // Enter arrives on a thread because reading stdin blocks, and a blocked read
    // would hold the whole loop still while frames pile up unread.
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let started = Instant::now();
    let mut step = 0usize;
    let mut since = 0usize;
    let script: Vec<&str> = match script {
        None => Vec::new(),
        Some(name) => SCRIPTS
            .iter()
            .find(|(named, _)| *named == name)
            .map(|(_, lines)| lines.to_vec())
            .ok_or_else(|| {
                format!(
                    "no script called {name}. there are: {}",
                    SCRIPTS
                        .iter()
                        .map(|(named, _)| *named)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
    };
    let mut next = usize::from(!script.is_empty());
    let instruct = |index: usize, say: &mut dyn FnMut(&str)| {
        if index < script.len() {
            say(&format!(
                "\n>>> STEP {}: {}\n    do that now, then press Enter. every frame until \
                 then belongs to this step.\n",
                index + 1,
                script[index]
            ));
        }
    };
    say("listening. `q` then Enter quits at any time; frames are stamped from now.");
    instruct(0, &mut say);
    loop {
        // A closed channel is how this experiment ends — the last step of the wear
        // script is switching the headset off — so it closes the transcript rather
        // than escaping as an error. Losing the peer is an outcome, not a fault.
        let polled = match momentum::Link::read_frame(&mut link, Duration::from_millis(150)) {
            Ok(next) => next,
            Err(momentum::Error::Disconnected) => {
                say(&format!(
                    "\nthe headset closed the channel during step {step} — expected when \
                     the experiment ends by switching it off. {since} frame(s) were still \
                     open in this step."
                ));
                break;
            }
            Err(err) => return Err(err.into()),
        };
        if let Some(frame) = polled {
            since += 1;
            say(&format!(
                "  t+{:6.1}s  << vendor 0x{:04x}  0x{:04x} {:<22} {:02x?}{}",
                started.elapsed().as_secs_f32(),
                frame.vendor,
                frame.cmd,
                momentum::cmd::name(frame.cmd),
                frame.payload,
                note(frame.cmd, &frame.payload),
            ));
            continue;
        }
        if let Ok(line) = rx.try_recv() {
            if line.trim().eq_ignore_ascii_case("q") {
                break;
            }
            step += 1;
            say(&format!(
                "── step {step}: {since} frame(s) since {} ──",
                if step == 1 {
                    "registration"
                } else {
                    "the previous Enter"
                }
            ));
            if script.is_empty() {
                say("press Enter to close the next step, or `q` to quit.");
            } else if next < script.len() {
                instruct(next, &mut say);
                next += 1;
            } else {
                say("\nscript finished. press `q` then Enter to leave.");
            }
            since = 0;
        }
    }
    say(&format!(
        "quit after {step} step(s); {}",
        momentum::Link::diagnostics(&link)
    ));
    Ok(())
}

/// What a payload is known to say, appended to a printed frame when anything is.
///
/// Only the sample rate is known this well: its four bytes, big-endian, were 48000
/// while a phone streamed and 44100 at rest, and no other frame in this project has
/// been pinned down that cleanly. Everything else prints as bytes, on purpose.
#[cfg(windows)]
fn note(cmd: u16, payload: &[u8]) -> String {
    if cmd != momentum::cmd::PUSH_SAMPLE_RATE {
        return String::new();
    }
    let Ok(bytes) = <[u8; 4]>::try_from(payload) else {
        return String::new();
    };
    let hertz = u32::from_be_bytes(bytes);
    if hertz == 0 {
        "  (no stream)".to_string()
    } else {
        format!("  ({hertz} Hz)")
    }
}

/// Report whether this machine could host the control service, and on `--serve`,
/// actually host it and log whatever arrives.
#[cfg(windows)]
fn host(serve: bool, name: &str) -> Result<(), Boxed> {
    if serve {
        return momentum::host::serve(&momentum::host::Serve { name, replies: &[] })
            .map_err(Into::into);
    }
    println!("{}.", momentum::host::adapter_summary());
    println!("{}", momentum::host::probe(momentum::GAIA_SERVICE_UUID)?);
    println!("nothing advertised, nothing connected; the provider is released here.");
    Ok(())
}

/// Send read-only queries and print whatever answers.
///
/// Every GAIA frame carries a vendor id, and a headset that does not own the
/// vendor in the frame answers *nothing* — silence, not an error. Windows reports
/// this headset's company id as 0x001D while the protocol itself is Qualcomm's
/// 0x0495, so the same queries are tried under both before any conclusion is
/// drawn.
/// Everything one `probe` run may be asked to do. It arrived as eight positional
/// arguments, which is how a tool ends up sending the wrong payload to the wrong id;
/// the caller now has to name what it means.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct ProbeRequest<'a> {
    seconds: u64,
    filter: &'a str,
    vendor: Option<&'a str>,
    cmd: Option<&'a str>,
    payload: &'a str,
    channel: Option<u32>,
    features: Option<&'a str>,
    register: Option<&'a str>,
}

/// Commands `momentum probe --cmd` refuses to put on the wire.
///
/// `probe` sends whatever it is handed, which is the right design for a tool whose job is to
/// find out what a device answers — until the id handed to it is a factory reset. These four
/// are in the vendor's own "dangerous, do not touch" list, they all destroy state this
/// program cannot rebuild, and no measurement needs them: everything worth knowing about this
/// headset has come from reading it.
fn destructive(id: u16) -> Option<&'static str> {
    match id {
        0x0040 => Some("it is the factory reset"),
        0x0200 | 0x0201 => Some("it belongs to the firmware upgrade path"),
        0x1405 => Some("it deletes one host from the pairing list"),
        0x1406 => Some("it deletes the entire pairing list"),
        0x2009 => {
            Some("it erases the hearing profile, which can only be remade by the hearing test")
        }
        _ => None,
    }
}

#[cfg(test)]
#[test]
fn probe_refuses_the_four_commands_that_cannot_be_undone() {
    for id in [0x0040u16, 0x0200, 0x0201, 0x1405, 0x1406, 0x2009] {
        assert!(destructive(id).is_some(), "{id:#06x} must be refused");
    }
    for id in [0x0041u16, 0x1400, 0x1404, 0x1a05, 0x2001, 0x2007] {
        assert!(
            destructive(id).is_none(),
            "{id:#06x} is a read, not a wreck"
        );
    }
}

#[cfg(windows)]
fn probe(request: &ProbeRequest<'_>) -> Result<(), Boxed> {
    use momentum::cmd::feature;
    use momentum::proto::frame::VENDOR_QUALCOMM;
    let ProbeRequest {
        seconds,
        filter,
        vendor,
        cmd,
        payload,
        channel,
        features,
        register,
    } = *request;

    // Refused before the link is opened rather than after: there is one control connection at
    // a time on this device, so a refusal that costs the owner their app's session, and then
    // risks the reconnect wedge, is a badly designed refusal.
    if let Some(text) = cmd {
        for id in parse_u16_spec(text)? {
            if let Some(reason) = destructive(id) {
                return Err(format!(
                    "`0x{id:04x}` is not sent from this program: {reason}. It cannot be \
                     undone from here, and nothing in this program needs it."
                )
                .into());
            }
        }
    }

    let mut link = connect_first(filter, false, channel)?.into_link();

    // Declaring a feature first, because "the device refused" and "the device refused
    // a client that never announced itself" are different findings, and only the
    // second one would be our mistake.
    if let Some(spec) = register {
        for id in byte_spec(spec)? {
            println!(">> declaring feature {id:02x} before the probes");
            momentum::Link::write_frame(
                &mut link,
                &command(VENDOR_QUALCOMM, momentum::cmd::REGISTER_FEATURE, vec![id]),
            )?;
        }
        let settle = Instant::now() + Duration::from_millis(700);
        while Instant::now() < settle {
            if let Some(frame) = momentum::Link::read_frame(&mut link, Duration::from_millis(100))?
            {
                println!(
                    "   its tables: 0x{:04x} {:<22} {:02x?}",
                    frame.cmd,
                    momentum::cmd::name(frame.cmd),
                    frame.payload,
                );
            }
        }
    }

    let mut probes: Vec<momentum::Frame> = Vec::new();
    if let Some(spec) = features {
        for id in byte_spec(spec)? {
            probes.push(command(
                VENDOR_QUALCOMM,
                momentum::cmd::REGISTER_FEATURE,
                vec![id],
            ));
        }
    } else if let Some(text) = cmd {
        let vendor = parse_u16(vendor.unwrap_or("495"))?;
        let bytes = hex_bytes(payload)?;
        for id in parse_u16_spec(text)? {
            probes.push(command(vendor, id, bytes.clone()));
        }
    } else {
        let queries = [
            (momentum::cmd::REGISTER_FEATURE, vec![feature::BATTERY]),
            (momentum::cmd::GET_BATTERY, Vec::new()),
            (momentum::cmd::GET_ANC, Vec::new()),
            (momentum::cmd::GET_SERIAL_NUMBER, Vec::new()),
        ];
        for vendor in [VENDOR_QUALCOMM, SENNHEISER_VENDOR] {
            for (cmd, payload) in queries.iter() {
                probes.push(command(vendor, *cmd, payload.clone()));
            }
        }
    }

    let mut answered = 0;
    for probe in &probes {
        println!(
            ">> vendor 0x{:04x}  cmd 0x{:04x} {:<22} payload {:02x?}",
            probe.vendor,
            probe.cmd,
            momentum::cmd::name(probe.cmd),
            probe.payload,
        );
        momentum::Link::write_frame(&mut link, probe)?;
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut replies = 0;
        while Instant::now() < deadline {
            match momentum::Link::read_frame(&mut link, Duration::from_millis(200))? {
                Some(frame) => {
                    replies += 1;
                    println!(
                        "<< vendor 0x{:04x}  0x{:04x} {:<22} {:02x?}{}",
                        frame.vendor,
                        frame.cmd,
                        momentum::cmd::name(frame.cmd),
                        frame.payload,
                        note(frame.cmd, &frame.payload),
                    );
                }
                None => continue,
            }
        }
        if replies > 0 {
            answered += 1;
        }
    }
    println!(
        "{} of {} probes answered; {}",
        answered,
        probes.len(),
        momentum::Link::diagnostics(&link)
    );
    Ok(())
}

/// Build a host-to-headset frame with an explicit vendor id.
#[cfg(windows)]
fn command(vendor: u16, cmd: u16, payload: Vec<u8>) -> momentum::Frame {
    momentum::Frame {
        kind: momentum::FrameKind::Command,
        vendor,
        cmd,
        payload,
    }
}

#[cfg(windows)]
fn parse_u16(text: &str) -> Result<u16, Boxed> {
    u16::from_str_radix(text.trim().trim_start_matches("0x"), 16)
        .map_err(|e| format!("\"{text}\" is not a hex number: {e}").into())
}

/// Command ids as hex: one `1005`, a span `1000-101f`, or a comma list. The span is
/// what makes a refusal pattern visible — one id tells you about a command, a block
/// of them tells you whether the firmware is refusing the *id* or the *payload*, and
/// those two have been telling very different stories all week.
#[cfg(windows)]
fn parse_u16_spec(text: &str) -> Result<Vec<u16>, Boxed> {
    let mut out = Vec::new();
    for part in text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((lo, hi)) = part.split_once('-') {
            let (lo, hi) = (parse_u16(lo)?, parse_u16(hi)?);
            if lo > hi {
                return Err(format!("{lo:04x}-{hi:04x} runs backwards").into());
            }
            out.extend(lo..=hi);
        } else {
            out.push(parse_u16(part)?);
        }
    }
    if out.is_empty() {
        return Err("no command ids given".into());
    }
    Ok(out)
}

/// Parse "01 f2" or "01,f2" into bytes; empty text means no payload.
#[cfg(windows)]
fn hex_bytes(text: &str) -> Result<Vec<u8>, Boxed> {
    text.split([',', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            u8::from_str_radix(part, 16)
                .map_err(|e| format!("\"{part}\" is not a hex byte: {e}").into())
        })
        .collect()
}

#[cfg(windows)]
fn anc(state: &str, filter: &str) -> Result<(), Boxed> {
    let wanted = switch(state)?;
    let mut session = open_session(filter)?;
    let event = session.set_anc(wanted)?;
    println!("{} -> {}", event.describe(), session.state.mode().label());
    Ok(())
}

#[cfg(windows)]
fn transparency(level: u8, filter: &str) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    let event = session.set_transparency(level)?;
    println!("{} -> {}", event.describe(), session.state.mode().label());
    Ok(())
}

/// The tilt written by `ramp1`, in raw units — ten per decibel. Deliberately no
/// band left at zero, so switching profiles reverses the whole curve rather than
/// moving one slider.
#[cfg(windows)]
const RAMP_UP: [i16; 8] = [-60, -50, -30, -10, 10, 30, 50, 60];
/// `ramp1` seen in a mirror.
#[cfg(windows)]
const RAMP_DOWN: [i16; 8] = [60, 50, 30, 10, -10, -30, -50, -60];
/// Zeros, to undo either.
#[cfg(windows)]
const RAMP_FLAT: [i16; 8] = [0; 8];

/// Write a tilt into the writable store and report per band whether the device
/// kept it. Temporary instrument, kept because it is the only way this device's
/// most mysterious table can be tested by ear.
#[cfg(windows)]
fn ramp(filter: &str, wanted: &[i16; 8]) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    let eq = session.set_eq_gains_wide(wanted)?;
    println!(
        "{:<5} {:>8}  {:>8}  {:>6}",
        "band", "wrote/dB", "read/dB", "stuck?"
    );
    let mut all = true;
    for (band, want) in wanted.iter().enumerate() {
        let got = eq.gains_wide.get(band).copied().flatten().map(|v| v as i16);
        let ok = got == Some(*want);
        all &= ok;
        println!(
            "{:<5} {:>+8.1}  {:>8}  {:>6}",
            band,
            *want as f32 / 10.0,
            got.map_or_else(|| "-".to_string(), |v| format!("{:+.1}", v as f32 / 10.0)),
            if ok { "yes" } else { "NO" },
        );
    }
    if all {
        println!(
            "stuck: all eight re-read from the headset after re-registering the \
             feature, not echoed from what we sent."
        );
    } else {
        println!("did not stick: at least one band came back different or absent.");
    }
    Ok(())
}

#[cfg(windows)]
fn bass(state: &str, filter: &str) -> Result<(), Boxed> {
    let wanted = switch(state)?;
    let mut session = open_session(filter)?;
    println!("{}", session.set_bass_boost(wanted)?.describe());
    Ok(())
}

/// The mode of the hearing profile, in the vendor's words rather than ours.
fn personal_mode(mode: Option<u8>) -> &'static str {
    match mode {
        Some(0) => "not parameterized — there is no hearing profile on this headset",
        Some(1) => "disabled — a profile exists and is switched off",
        Some(2) => "standby — a profile exists, and the vendor's own app counts this as on",
        Some(3) => "enabled",
        Some(4) => "calibrating — a hearing test is running somewhere",
        Some(_) => "a mode nobody has named",
        None => "no answer, so nothing is known",
    }
}

/// `on` / `off` / nothing, into the byte `0x2000` wants. Separate from the command so that the
/// refusal of every other word is tested rather than asserted.
fn personal_switch(state: Option<&str>) -> Result<Option<u8>, String> {
    match state.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") => Ok(None),
        Some("on") => Ok(Some(3)),
        Some("off") => Ok(Some(1)),
        Some(other) => Err(format!(
            "`{other}` is not a state: `momentum personalization` takes `on`, `off`, or nothing"
        )),
    }
}

/// Big-endian `REAL32`s: the personalization block carries floats where the rest of this
/// protocol carries bytes, which is why it needs a decoder of its own.
fn floats(payload: &[u8]) -> Vec<f32> {
    payload
        .chunks(4)
        .filter(|chunk| chunk.len() == 4)
        .map(|chunk| f32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// The floats as one line, two decimals — enough to show that the pairs are exactly 15.00
/// apart, not nearly.
fn numbers(payload: &[u8]) -> String {
    let rendered: Vec<String> = floats(payload)
        .iter()
        .map(|value| format!("{value:+.2}"))
        .collect();
    if rendered.is_empty() {
        "nothing".to_string()
    } else {
        rendered.join("   ")
    }
}

/// The same values where they are not levels: a ratio of +1.00 is a sign error waiting to be
/// read, so these lose the sign and gain a digit.
fn ratios(payload: &[u8]) -> String {
    let rendered: Vec<String> = floats(payload)
        .iter()
        .map(|value| format!("{value:.3}"))
        .collect();
    if rendered.is_empty() {
        "nothing".to_string()
    } else {
        rendered.join("   ")
    }
}

#[cfg(test)]
#[test]
fn floats_are_read_big_endian_in_fours() {
    let measured = [
        0xc2u8, 0x9b, 0xcc, 0xf8, 0xc2, 0x7b, 0x99, 0xf1, 0x3f, 0x80, 0x00, 0x00,
    ];
    let values = floats(&measured);
    assert_eq!(values.len(), 3);
    assert!((values[0] - -77.9).abs() < 0.01, "{}", values[0]);
    assert!((values[1] - -62.9).abs() < 0.01, "{}", values[1]);
    assert!((values[2] - 1.0).abs() < f32::EPSILON);
    assert_eq!(floats(&[0x00, 0x01]), [], "a partial float is not a number");
    assert!(numbers(&[0x00]).starts_with("nothing"));
}

#[cfg(test)]
#[test]
fn personalization_accepts_on_off_and_nothing_only() {
    assert_eq!(personal_switch(None).unwrap(), None);
    assert_eq!(personal_switch(Some("")).unwrap(), None);
    assert_eq!(personal_switch(Some("ON")).unwrap(), Some(3));
    assert_eq!(personal_switch(Some(" off ")).unwrap(), Some(1));
    assert!(personal_switch(Some("standby")).is_err());
    assert!(personal_switch(Some("3")).is_err());
}

/// Read the hearing profile, or switch it. See the command's own help for what is guessed and
/// what is known: the shapes are the vendor's, the values are this device's, and the meaning
/// of a threshold is neither.
#[cfg(windows)]
fn personalization(state: Option<&str>, filter: &str) -> Result<(), Boxed> {
    use momentum::cmd;
    let wanted = personal_switch(state).map_err(|err| -> Boxed { err.into() })?;
    let wait = Duration::from_millis(900);
    let mut session = open_session(filter)?;
    let reply = session
        .request(cmd::GET_PERSONAL_MODE, Vec::new(), wait)
        .ok();
    let current = reply
        .as_ref()
        .and_then(|frame| frame.payload.first().copied());
    match current {
        Some(mode @ 5..) => println!("personalization: {} ({mode:#04x}).", personal_mode(current)),
        _ => println!("personalization: {}.", personal_mode(current)),
    }

    if let Some(byte) = wanted {
        if byte == 3 && current == Some(0) {
            return Err(
                "there is no hearing profile on this headset — the mode is `not \
                 parameterized` — so there is nothing to switch on. The profile is made by a \
                 hearing test in the phone app, not from here."
                    .into(),
            );
        }
        // The phone app greys this control out while Hi-Res is on. That is the app's rule and
        // not the device's: turning Hi-Res off here left the mode untouched, twice.
        if byte == 3
            && session
                .request(0x0406, Vec::new(), wait)
                .ok()
                .is_some_and(|frame| frame.payload.first() == Some(&0))
        {
            println!(
                "note: Hi-Res is on. The phone app will not send this while it is; the headset \
                 has never refused it here."
            );
        }
        println!(">> 0x2000 [{byte:02x}]  {}", personal_mode(Some(byte)));
        match session.request(cmd::SET_PERSONAL_MODE, vec![byte], wait) {
            Ok(frame) => println!("<< {} {:02x?}", cmd::name(frame.cmd), frame.payload),
            Err(err) => println!("the headset refused the write: {err}"),
        }
        if let Ok(frame) = session.request(cmd::GET_PERSONAL_MODE, Vec::new(), wait) {
            let now = frame.payload.first().copied();
            println!("read back: {} — {now:#04x?}", personal_mode(now));
        }
    }

    for (id, label, level) in [
        (cmd::GET_PERSONAL_THRESHOLDS, "thresholds", true),
        (cmd::GET_PERSONAL_RATIOS, "ratios  ", false),
        (cmd::GET_PERSONAL_GAINS, "gains   ", true),
    ] {
        match session.request(id, Vec::new(), wait) {
            Ok(frame) if label == "gains   " => {
                let preset = frame.payload.get(12..).unwrap_or(&[]);
                println!(
                    "{label} {}   preset indices {}",
                    numbers(&frame.payload),
                    momentum::proto::state::hex(preset)
                );
            }
            Ok(frame) => {
                let row = if level {
                    numbers(&frame.payload)
                } else {
                    ratios(&frame.payload)
                };
                println!("{label} {row}");
            }
            Err(_) => println!("{label} no answer"),
        }
    }
    println!(
        "The vendor's model calls these Threshold, Ratio and Gains and never says what a \
         threshold is measured against. On this unit three of the six sit exactly 15.00 dB \
         above another three, three of the six ratios are exactly 1.000, and the gains are \
         three floats and two bytes of preset indices — printed because they are in the \
         device, they change, and nothing here explains them yet."
    );
    for warning in &session.warnings {
        println!("note: {warning}");
    }
    Ok(())
}

async fn adapters() -> Result<Vec<Adapter>, Boxed> {
    let manager = Manager::new().await?;
    let list = manager.adapters().await?;
    if list.is_empty() {
        return Err(
            "no Bluetooth adapter found: is the radio on and Windows' Bluetooth service running?"
                .into(),
        );
    }
    Ok(list)
}

async fn discover(seconds: u64) -> Result<Vec<Peripheral>, Boxed> {
    let mut found = Vec::new();
    for adapter in adapters().await? {
        println!("scanning on {} ...", adapter.adapter_info().await?);
        adapter.start_scan(ScanFilter::default()).await?;
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        adapter.stop_scan().await.ok();
        found.extend(adapter.peripherals().await?);
    }
    Ok(found)
}

async fn name_of(peripheral: &Peripheral) -> String {
    peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .and_then(|p| p.local_name.clone())
        .unwrap_or_else(|| "(unnamed)".into())
}

async fn address_of(peripheral: &Peripheral) -> String {
    peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .map(|p| p.address.to_string())
        .unwrap_or_else(|| "?".into())
}

/// Find one peripheral by address, else by case-insensitive name substring.
async fn select(
    peripherals: Vec<Peripheral>,
    address: Option<BDAddr>,
    filter: &str,
) -> Result<Peripheral, Boxed> {
    let needle = filter.to_lowercase();
    let mut matches: Vec<Peripheral> = Vec::new();
    for peripheral in peripherals {
        let Ok(Some(properties)) = peripheral.properties().await else {
            continue;
        };
        if let Some(wanted) = address {
            if properties.address == wanted {
                return Ok(peripheral);
            }
            continue;
        }
        if properties
            .local_name
            .unwrap_or_default()
            .to_lowercase()
            .contains(&needle)
        {
            matches.push(peripheral);
        }
    }
    if let Some(wanted) = address {
        return Err(format!(
            "{wanted} was not seen while scanning. Put the headset in range; if it is already \
             connected elsewhere its LE advertisements may have stopped."
        )
        .into());
    }
    match matches.len() {
        0 => Err(format!("no device matching \"{filter}\" was found while scanning").into()),
        1 => Ok(matches.remove(0)),
        count => {
            eprintln!("several devices match \"{filter}\", pick one with --address:");
            for peripheral in &matches {
                println!(
                    "  {} {}",
                    address_of(peripheral).await,
                    name_of(peripheral).await
                );
            }
            Err(format!("{count} devices matched").into())
        }
    }
}

async fn scan(seconds: u64, filter: &str) -> Result<(), Boxed> {
    let needle = filter.to_lowercase();
    let peripherals = discover(seconds).await?;
    let mut shown = 0;
    for peripheral in &peripherals {
        let Ok(Some(properties)) = peripheral.properties().await else {
            continue;
        };
        let name = properties.local_name.clone().unwrap_or_default();
        if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
            continue;
        }
        println!(
            "{:<18} connected={:<5} {}",
            properties.address,
            peripheral.is_connected().await.unwrap_or(false),
            if name.is_empty() { "(unnamed)" } else { &name }
        );
        // What the advertisement itself carries. Whether a control path exists over LE
        // is usually answered here, without a connection: a device that means to be
        // controlled publishes its service in the advertisement, and a manufacturer id
        // says whose advertisement this is.
        if !properties.services.is_empty() {
            println!("    advertised services: {:?}", properties.services);
        }
        for (id, data) in &properties.manufacturer_data {
            println!("    manufacturer 0x{id:04x}: {data:02x?}");
        }
        for (uuid, data) in &properties.service_data {
            println!("    service data {uuid}: {data:02x?}");
        }
        if let Some(appearance) = properties.appearance {
            println!("    appearance 0x{appearance:04x}");
        }
        shown += 1;
    }
    if shown == 0 {
        println!("nothing matching \"{filter}\"; try --filter \"\" to list every device");
    }
    Ok(())
}

async fn gatt(
    address: Option<BDAddr>,
    filter: &str,
    seconds: u64,
    no_read: bool,
    notify: Option<u64>,
) -> Result<(), Boxed> {
    let peripheral = select(discover(seconds).await?, address, filter).await?;
    let label = format!(
        "{} {}",
        address_of(&peripheral).await,
        name_of(&peripheral).await
    );

    let we_connected = if peripheral.is_connected().await? {
        false
    } else {
        println!("connecting to {label} ...");
        peripheral.connect().await?;
        true
    };

    println!("discovering services on {label} ...");
    peripheral.discover_services().await?;
    let services = peripheral.services();
    println!("{} service(s)", services.len());

    let mut subscribable = Vec::new();
    for service in &services {
        println!(
            "\nservice {} [{}]",
            service.uuid,
            if service.primary {
                "primary"
            } else {
                "included"
            }
        );
        for characteristic in &service.characteristics {
            print!(
                "  {} {}",
                characteristic.uuid,
                flags(&characteristic.properties)
            );
            if !no_read && characteristic.properties.contains(CharPropFlags::READ) {
                match peripheral.read(characteristic).await {
                    Ok(value) => print!(" = {}{}", hex(&value), annotate(&value)),
                    Err(err) => print!(" = <read failed: {err}>"),
                }
            }
            println!();
            if characteristic
                .properties
                .intersects(CharPropFlags::NOTIFY | CharPropFlags::INDICATE)
            {
                subscribable.push(characteristic.clone());
            }
        }
    }

    if let Some(seconds) = notify {
        let mut stream = peripheral.notifications().await?;
        for characteristic in &subscribable {
            match peripheral.subscribe(characteristic).await {
                Ok(()) => println!("subscribed {}", characteristic.uuid),
                Err(err) => println!("could not subscribe {}: {err}", characteristic.uuid),
            }
        }
        if subscribable.is_empty() {
            println!("this device publishes nothing notify-capable over BLE");
        }
        println!("streaming for {seconds}s: switch modes on the headset or in the Sennheiser app");
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while let Ok(Some(notification)) =
            tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), stream.next()).await
        {
            println!(
                "notify {} (service {}) -> {}{}",
                notification.uuid,
                notification.service_uuid,
                hex(&notification.value),
                annotate(&notification.value)
            );
        }
    }

    if we_connected {
        peripheral.disconnect().await.ok();
    }
    Ok(())
}

/// Render characteristic properties compactly, e.g. `[r N ]`.
fn flags(properties: &CharPropFlags) -> String {
    let mut out = String::from("[");
    let mut push = |enabled: bool, flag: &str| {
        if enabled {
            out.push_str(flag);
            out.push(' ');
        }
    };
    push(properties.contains(CharPropFlags::READ), "r");
    push(properties.contains(CharPropFlags::WRITE), "w");
    push(
        properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE),
        "nw",
    );
    push(properties.contains(CharPropFlags::NOTIFY), "N");
    push(properties.contains(CharPropFlags::INDICATE), "I");
    out.push(']');
    out
}

/// Hint when a value looks like a GAIA frame, so a BLE transport could reuse
/// the protocol layer in this crate unchanged.
fn annotate(value: &[u8]) -> String {
    if value.len() < HEADER_LEN || value[0] != 0xFF {
        return String::new();
    }
    let mut framer = Framer::new();
    framer.push(value);
    match framer.next_frame() {
        Some(frame) if frame.vendor == momentum::VENDOR_QUALCOMM => {
            format!("  <- GAIA {} ({:#06x})", cmd::name(frame.cmd), frame.cmd)
        }
        _ => String::new(),
    }
}

#[cfg(all(windows, test))]
mod tests {
    use super::{eq_diff, note};

    #[test]
    fn the_sample_rate_frame_is_read_big_endian() {
        let rate = momentum::cmd::PUSH_SAMPLE_RATE;
        assert_eq!(note(rate, &[0x00, 0x00, 0xac, 0x44]), "  (44100 Hz)");
        assert_eq!(note(rate, &[0x00, 0x00, 0xbb, 0x80]), "  (48000 Hz)");
        assert_eq!(note(rate, &[0x00, 0x00, 0x00, 0x00]), "  (no stream)");
    }

    #[test]
    fn nothing_else_claims_a_meaning() {
        // The link frame moves with the stream but its values are not understood, so
        // it stays bytes, and a payload of the wrong length says nothing at all.
        assert_eq!(note(0x0880, &[0x08]), "");
        assert_eq!(note(momentum::cmd::PUSH_SAMPLE_RATE, &[0x00, 0x00]), "");
    }

    #[test]
    fn eq_diff_reports_only_what_moved() {
        use momentum::proto::state::Equaliser;
        let before = Equaliser {
            gains: vec![0, 10, -10],
            hz: vec![Some(90), Some(325), Some(1500)],
            ..Default::default()
        };
        let after = Equaliser {
            gains: vec![0, 10, -20],
            hz: vec![Some(90), Some(325), Some(1500)],
            q12: vec![Some(2908), Some(2908), Some(2908)],
            ..Default::default()
        };
        let diff = eq_diff(&before, &after);
        // A whole table that arrived between the two reads is a change on every band:
        // nothing had said what the Q was, and now something has.
        assert_eq!(
            diff,
            [
                "band 0: Q12 None → Some(2908)",
                "band 1: Q12 None → Some(2908)",
                "band 2: gain -1.0 → -2.0 dB",
                "band 2: Q12 None → Some(2908)",
            ]
        );
    }

    #[test]
    fn eq_diff_is_silent_when_nothing_moved() {
        use momentum::proto::state::Equaliser;
        let read = Equaliser {
            gains: vec![0, 10],
            hz: vec![Some(90), Some(325)],
            ..Default::default()
        };
        assert!(eq_diff(&read, &read).is_empty());
    }

    #[test]
    fn personal_diff_names_the_payload_that_moved() {
        use super::personal_diff;
        let before = vec![(0x2101u16, vec![0x02]), (0x2103, vec![0xc2, 0x9b])];
        let after = vec![(0x2101u16, vec![0x03]), (0x2103, vec![0xc2, 0x9b])];
        assert_eq!(
            personal_diff(&before, &after),
            ["personalization mode 0x2101: [02] → [03]".to_string()]
        );
    }

    /// The reason the comparison is keyed by id: a poll in which one request fails has a
    /// shorter list, and zipping by position would compare the wrong payloads and print a
    /// change that never happened.
    #[test]
    fn personal_diff_survives_a_poll_that_answered_less() {
        use super::personal_diff;
        let before = vec![(0x2101u16, vec![0x02]), (0x2103, vec![0xc2, 0x9b])];
        let after = vec![(0x2103u16, vec![0xc2, 0x9b])];
        assert!(personal_diff(&before, &after).is_empty());
    }
}
