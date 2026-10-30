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
        /// Command id as hex, e.g. 1A05. Sends that one frame instead of the
        /// standard matrix.
        #[arg(long)]
        cmd: Option<String>,
        /// Payload bytes as hex pairs, space or comma separated. Only with --cmd.
        #[arg(long, default_value = "")]
        payload: String,
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
        Command::Probe {
            seconds,
            filter,
            vendor,
            cmd,
            payload,
        } => Some(probe(
            *seconds,
            filter,
            vendor.as_deref(),
            cmd.as_deref(),
            payload,
        )),
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
    connect_first(filter, true)
}

/// Connect to the first channel that answers. `register` decides whether to
/// hand the headset a feature registration and wait for its ack, which is what
/// a real session needs and a passive listener must not do.
#[cfg(windows)]
fn connect_first(
    filter: &str,
    register: bool,
) -> Result<momentum::Session<momentum::rfcomm::WindowsRfcomm>, Boxed> {
    let needle = if filter.is_empty() { None } else { Some(filter) };
    let targets = momentum::rfcomm::find(momentum::GAIA_SERVICE_UUID, needle)?;
    if targets.is_empty() {
        return Err(format!(
            "nothing publishes the control service ({}). Is the headset switched on? \
             Try `momentum rf-scan`.",
            momentum::GAIA_SERVICE_UUID
        )
        .into());
    }
    let mut last_error = None;
    for target in &targets {
        println!(
            "connecting to {} ({}) ...",
            target.name,
            momentum::rfcomm::peer_address(&target.device_id).unwrap_or_default()
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
        let mut link = connect_first(filter, false)?.into_link();
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

/// Send read-only queries and print whatever answers.
///
/// Every GAIA frame carries a vendor id, and a headset that does not own the
/// vendor in the frame answers *nothing* — silence, not an error. Windows reports
/// this headset's company id as 0x001D while the protocol itself is Qualcomm's
/// 0x0495, so the same queries are tried under both before any conclusion is
/// drawn.
#[cfg(windows)]
fn probe(
    seconds: u64,
    filter: &str,
    vendor: Option<&str>,
    cmd: Option<&str>,
    payload: &str,
) -> Result<(), Boxed> {
    use momentum::cmd::feature;
    use momentum::proto::frame::VENDOR_QUALCOMM;

    let mut link = connect_first(filter, false)?.into_link();

    let mut probes: Vec<momentum::Frame> = Vec::new();
    if let Some(text) = cmd {
        probes.push(command(
            parse_u16(vendor.unwrap_or("495"))?,
            parse_u16(text)?,
            hex_bytes(payload)?,
        ));
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
                        "<< vendor 0x{:04x}  0x{:04x} {:<22} {:02x?}",
                        frame.vendor,
                        frame.cmd,
                        momentum::cmd::name(frame.cmd),
                        frame.payload,
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
