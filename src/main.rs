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
    /// Print the equaliser: every band with its centre frequency and the values
    /// the headset stores for it. Read-only by design — the write path exists on
    /// the wire and was confirmed with a no-op, but the gain scale in decibels
    /// has never been measured, and this tool will not print a number it cannot
    /// account for.
    Eq {
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
        /// Also print the raw tables the headset reports but this tool cannot
        /// yet explain: `0x100b`, the u16 gain store, `0x100d`, `0x100f` and
        /// `0x1013`. Their band index has never been connected to the app's
        /// bands, so they are off by default rather than labelled carefully.
        #[arg(long)]
        raw: bool,
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
    Watch {
        /// Seconds to keep the channel open.
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
        /// Only listen: send nothing and print raw frames as they arrive.
        #[arg(long)]
        raw: bool,
    },
    /// Turn active noise control on or off.
    Anc {
        /// on | off
        state: String,
        /// Select by service name or peer address; empty means any.
        #[arg(long, default_value = "")]
        filter: String,
    },
    /// Set the transparent-hearing level, 0..=100. 0 turns it off.
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
                Some(text) => Some(text.parse::<BDAddr>().map_err(|e| format!("bad address {text}: {e}"))?),
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
        Command::Eq { filter, raw } => Some(eq(filter, *raw)),
        Command::Ramp1 { filter } => Some(ramp(filter, &RAMP_UP)),
        Command::Ramp2 { filter } => Some(ramp(filter, &RAMP_DOWN)),
        Command::Ramp0 { filter } => Some(ramp(filter, &RAMP_FLAT)),
        Command::Host { serve, name } => Some(host(*serve, name)),
        Command::Watch { seconds, filter, raw } => Some(watch(*seconds, filter, *raw)),
        Command::Anc { state, filter } => Some(anc(state, filter)),
        Command::Transparency { level, filter } => Some(transparency(*level, filter)),
        Command::Bass { state, filter } => Some(bass(state, filter)),
        Command::Volume {
            level,
            filter,
            list,
        } => Some(volume(*level, filter, *list)),
        Command::Mute { state, filter } => Some(mute(state.as_deref(), filter)),
        Command::Steps { features, channel, out, script, filter } => Some(steps(
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
    let needle = if filter.is_empty() { None } else { Some(filter) };
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
fn open_session(
    filter: &str,
) -> Result<momentum::Session<momentum::rfcomm::WindowsRfcomm>, Boxed> {
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
    let needle = if filter.is_empty() { None } else { Some(filter) };
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
            println!(
                "  link: {}",
                momentum::Link::diagnostics(session.link())
            );
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

/// The equaliser as one table. The `dB` column is calibrated; the rest are raw
/// values whose units the wire has not established, including the device's own
/// band table, which disagrees with the app about where the bands are.
#[cfg(windows)]
fn eq(filter: &str, raw: bool) -> Result<(), Boxed> {
    let mut session = open_session(filter)?;
    let eq = session.equaliser()?;
    if eq.is_empty() {
        println!("the headset sent no equaliser tables");
        for warning in &session.warnings {
            println!("note: {warning}");
        }
        return Ok(());
    }

    let cell = |row: &[Option<u16>], band: usize| {
        row.get(band).copied().flatten().map_or_else(
            || "-".to_string(),
            |value| value.to_string(),
        )
    };
    use momentum::APP_BANDS_HZ;

    let db = eq.gains_db();
    println!("{:<4} {:>8}  {:>6}", "band", "app/Hz", "dB");
    for band in 0..eq.bands() {
        println!(
            "{:<4} {:>8}  {:>6}",
            band,
            APP_BANDS_HZ
                .get(band)
                .map_or_else(|| "-".to_string(), |hz| hz.to_string()),
            db.get(band)
                .map_or_else(|| "-".to_string(), |gain| format!("{gain:+.1}")),
        );
    }
    println!(
        "dB is calibrated: ten raw units per decibel, read back exactly for a ramp \
         of -6, -5, -4, -3, -2, -1, 0, +1 set in the app."
    );

    // These carry their own band index, and nothing proves it is the app's band
    // order — setting a decibel ramp across the sliders left every one of them
    // untouched. Printing them against app/Hz would imply a relationship that has
    // never been observed, so they get their own table and their own label.
    if raw
        && !(eq.row_100b.is_empty()
            && eq.gains_wide.is_empty()
            && eq.band_values.is_empty()
            && eq.limits.is_empty())
    {
        println!(
            "\nother tables, index space unproven — none moved when the sliders did"
        );
        // Raw hex next to the decimal: an unexplained number is worth keeping
        // exactly as the device sent it, not as an interpretation of it.
        let hexed = |row: &[Option<u16>], at: usize| {
            row.get(at).copied().flatten().map_or_else(
                || "-".to_string(),
                |v| format!("{v:#06x} ({v})"),
            )
        };
        // Stored signed — `0x1010` accepted −180 and refused −182, so two's
        // complement is a fact about representation, not a guess about units.
        // Printing 65526 for what the device means as −10 is not neutrality, it is
        // a display that manufactures mystery.
        let signed = |row: &[Option<u16>], at: usize| {
            row.get(at)
                .copied()
                .flatten()
                .map_or_else(|| "-".to_string(), |v| (v as i16).to_string())
        };
        println!(
            "{:<4} {:>7}  {:>8}  {:>14}  {:>6}",
            "row", "0x100b", "gain16/s", "value", "limit"
        );
        for row in 0..eq.bands() {
            println!(
                "{:<4} {:>7}  {:>8}  {:>14}  {:>6}",
                row,
                cell(&eq.row_100b, row),
                signed(&eq.gains_wide, row),
                hexed(&eq.band_values, row),
                cell(&eq.limits, row),
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
        println!(
            "link: {}",
            momentum::Link::diagnostics(&link)
        );
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
    for part in text.split(',').map(str::trim).filter(|part| !part.is_empty()) {
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
                    SCRIPTS.iter().map(|(named, _)| *named).collect::<Vec<_>>().join(", ")
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
    say(&format!("quit after {step} step(s); {}", momentum::Link::diagnostics(&link)));
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
            if let Some(frame) = momentum::Link::read_frame(&mut link, Duration::from_millis(100))? {
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
            probes.push(command(VENDOR_QUALCOMM, momentum::cmd::REGISTER_FEATURE, vec![id]));
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
    for part in text.split(',').map(str::trim).filter(|part| !part.is_empty()) {
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
    println!("{:<5} {:>8}  {:>8}  {:>6}", "band", "wrote/dB", "read/dB", "stuck?");
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

async fn adapters() -> Result<Vec<Adapter>, Boxed> {
    let manager = Manager::new().await?;
    let list = manager.adapters().await?;
    if list.is_empty() {
        return Err("no Bluetooth adapter found: is the radio on and Windows' Bluetooth service running?".into());
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
        if properties.local_name.unwrap_or_default().to_lowercase().contains(&needle) {
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
                println!("  {} {}", address_of(peripheral).await, name_of(peripheral).await);
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
    let label = format!("{} {}", address_of(&peripheral).await, name_of(&peripheral).await);

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
            if service.primary { "primary" } else { "included" }
        );
        for characteristic in &service.characteristics {
            print!("  {} {}", characteristic.uuid, flags(&characteristic.properties));
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
    push(properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE), "nw");
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
    use super::note;

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
}
