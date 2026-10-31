# momentum

Control a Sennheiser MOMENTUM headset from the command line: noise cancelling,
transparent hearing, ambient level, bass boost, battery, serial and firmware — without the
phone app.

It speaks the headset's own private control channel (Qualcomm GAIA over an
RFCOMM serial port), which is the same channel the Sennheiser Smart Control app
uses. Windows finds the service for us; this crate only frames commands and
decodes answers. See [`docs/PROTOCOL.md`](docs/PROTOCOL.md) for the wire format,
which commands are verified on which model, and how to find out more.

## Works today, on Windows, against a real MOMENTUM 5

Everything below was run against BD_ADDR `80:C3:BA:C5:BB:97` and observed to
change how the headset behaves:

```console
$ momentum status
noise control: noise cancelling | ambient level 0 | battery 100% | bass boost off
device: MOMENTUM 5 | firmware 6.21.1 (built 2026-05-20 06:46:42+00:00) | serial 8000800326034155

$ momentum info
product: MOMENTUM 5
firmware: 6.21.1 / 2026-05-20 06:46:42+00:00
serial: 8000800326034155

$ momentum anc off
anc off -> off

$ momentum transparency 25
transparency level 25 -> transparent hearing

$ momentum bass on
bass boost on
```

Read-only and safe to poke at:

```console
$ momentum rf-scan            # which paired devices publish the control service
GAIA             80:c3:ba:c5:bb:97 channel 0  (Bluetooth#Bluetooth…-80:c3:ba:c5:bb:97#RFCOMM:00000000:{a2129ff3-…})
GAIA             80:c3:ba:c5:bb:97 channel 1  (…)

$ momentum watch --seconds 20 # decoded state pushes, live
$ momentum watch --raw --seconds 20   # passive: sends nothing, prints raw frames
$ momentum probe              # read-only queries, printed raw, incl. unknown ids
```

## Build

```console
cargo build --release
```

Rust 1.85 or newer (edition 2024). The RFCOMM transport is WinRT, so no drivers,
no admin rights, and no pairing changes: the headset must simply be paired in
Windows the usual way.

`--filter` selects a device by service name or peer address — Windows names the
service entry after the *service* ("GAIA"), not after the product, so
`--filter=80:c3:ba` is the reliable form. Empty, the default, means "whatever
publishes the control service", and every candidate channel is tried until one
answers.

## Commands

| Command | What it does |
| --- | --- |
| `status` | Noise-control mode, ambient level, battery, bass boost, and the device line |
| `info` | Product name, firmware version with build stamp, serial number |
| `eq show` | Every band with its centre frequency, Q, filter type and gain in decibels; `--raw` adds the tables we cannot yet explain |
| `eq set <band> [--gain 6] [--freq 4000] [--q 0.5] [--type 14] [--hold 12]` | One connection, several parameters, and a counted undo — the form for listening: `eq set 0 --gain 6 --hold 12` |
| `eq gain <band> <dB>` | Move one band, read it back, and mean it: the change is stored and audible |
| `eq freq <band> <Hz>` | Move a band's centre frequency. Acknowledged and stored; whether the audio path reads it is what the listening test decides |
| `eq q <band> <Q>` | Set a band's Q, as a decimal — `0.71` is 1/√2, `12` is a narrow peak |
| `eq type <band> <n>` | Set a band's filter type. An opaque enumeration nobody has published; `14` is what every band ships with |
| `eq flat` | Zero every gain |
| `eq tune` | Interactive tuner: one connection, an edit per keystroke, `r` puts the session's starting snapshot back |
| `mode [name\|0-5]` | Read or change the audio mode the DSP applies — `off`, `usereq`, `podcast`, `personalized`, `parametric`, `hearing` |
| `anc <on\|off>` | Noise cancelling |
| `transparency <0..=100>` | Transparent-hearing level; `0` turns it off |
| `bass <on\|off>` | Bass boost |
| `volume [0..=100]` | Playback volume, `--list` to see devices — via Windows, not the headset |
| `mute [on\|off]` | Mute that endpoint; the level survives it |
| `watch [--seconds N] [--raw]` | Live state pushes, decoded or raw |
| `audio [--seconds N]` | The sample rate the headset is decoding and the codec in use, straight from the headset |
| `probe [--cmd 1A05 --payload 01 --vendor 495]` | Send queries, print answers; the reverse-engineering tool |
| `steps [--script wear\|buttons] [--out file]` | Listener you drive by hand: it names the state to put the headphones in, waits for Enter, and attributes every frame to that step |
| `host [--serve]` | Ask whether Windows will let this PC hold the control service — and with `--serve`, hold it, advertise it and log whatever a client sends |
| `rf-scan [--uuid …]` | List paired devices publishing an RFCOMM control service |
| `scan [--seconds N] [--filter text]` | Bluetooth LE advertisements, with the services and manufacturer data each one carries |
| `gatt [--address A] [--notify N]` | Dump an LE device's GATT table, read it, stream its notifications |
| `scan`, `gatt` | Bluetooth LE probes, for headsets that expose control over GATT instead |

## As a library

The transport is a trait, so the protocol is usable without this CLI:

```rust
use momentum::{DEFAULT_FEATURES, Session, rfcomm};

let targets = momentum::rfcomm::find(momentum::GAIA_SERVICE_UUID, Some("80:c3:ba"))?;
let mut session = Session::new(rfcomm::connect(&targets[0])?);
session.open(&DEFAULT_FEATURES)?;   // subscribe and wait for the ack
session.refresh()?;                 // one round of queries into session.state
println!("{}", session.state.summary());
session.set_anc(true)?;             // writes, then reads the value back
```

`Session` keeps a `DeviceState` current from both replies and unsolicited
pushes, so a long-lived `watch` loop and a one-shot `status` share the same code.

## Notes on real hardware

- **The equaliser writes, after a week of the wrong payload shape.** `momentum eq gain 3 2.5`
  sends `0x1001 [band, gain]` — one band, gain signed in tenths of a decibel — and prints
  what the headset reports back, which is the number that is stored. The id and its two-byte
  payload came from outside: Sennheiser's own app carries the command table as JSON, and
  [xndbogdan's protocol map](https://github.com/xndbogdan/sennheiser-control/blob/main/PROTOCOL.md)
  publishes it as `UserEQ_SetSliderGain`. Every shape we had guessed — the eight-byte curve,
  sixteen, twenty-four — was the whole table where the device wants one band. The limits are
  the device's own too: `0x1000` answers eight bands, −6.0 to +6.0 dB.
- **The equaliser is only audible in one audio mode.** `0x0804` reports it, and the user
  curve is heard when the mode is `userEq`. A write made under another mode is stored
  correctly and heard not at all.
- **There is no parametric equaliser hiding here, tested as hard as the wire allows.** The
  per-band frequency, Q and filter type that `momentum eq show` prints are the graphic
  sliders' own biquads — the MOMENTUM 4 pushes the same three tables for its five sliders —
  and Sennheiser's older five-band parametric protocol is answered `not supported` when
  `0x1003` is queried in the format its own app used. Audio mode 4, which the shared enum
  calls `parametricEq`, is accepted and sat in, and changes nothing you can hear: what
  sounded like a different tuning turned out to be the **bass boost** sub-mode, which belongs
  to `userEq` and stops applying in any other mode. `momentum bass off` and the two modes are
  indistinguishable.
- **Volume is not a headset command.** It travels over AVRCP as "absolute
  volume", and Windows owns that negotiation, so `momentum volume` sets the
  endpoint's master volume — the tray slider's own path, which for Bluetooth
  headphones really does move the headset. It lives in `src/volume.rs`, apart
  from the protocol modules, so the code does not imply the headset implements it.
- **A USB-C cable gives you two endpoints.** With the cable in, Windows lists both
  the Bluetooth and the USB audio endpoint of the same headset ("Headphones
  (MOMENTUM 5)" and "Headphones (2- MOMENTUM 5)"), and the cable is not what
  silences the control channel. `momentum volume --list` shows which is default;
  `--filter=momentum` picks by name.
- **A connection is not a conversation.** Windows will open the RFCOMM channel
  even when nothing on the headset is listening. `open()` therefore waits for the
  feature-registration ack, and `status` prints byte counters (`tx 9 bytes, rx 0
  bytes`) when a command goes unanswered — that difference is the whole
  diagnosis.
- **A USB-C cable is not a problem.** The control channel answers with the cable
  in, contrary to what you might expect from a headset that switches to USB
  audio.
- The headset pushes state changes on its own (battery, ANC, transparency), which
  is why `watch` needs no polling.
- Noise cancelling and transparent hearing are exclusive, but the headset does
  **not** push `anc(false)` when transparency switches on. Both flags are kept
  separate internally for that reason; see `docs/PROTOCOL.md`.

## Status and shape

- `src/proto/` — the protocol: frames, commands, state. No I/O, fully unit
  tested; this is where the knowledge lives.
- `src/link.rs` — the one-method-pair trait every transport implements.
- `src/rfcomm.rs` — Windows transport (WinRT `RfcommDeviceService`).
- `src/session.rs` — registration, reply correlation, push folding.
- `src/main.rs` — CLI.

Not done: the same transport for Linux (`bluer`, behind a feature flag), the
user equaliser, and find-my-headphones, which needs a command id nobody has
confirmed yet — `probe` exists precisely so that gap gets closed by measurement
rather than guesswork.
