# The Sennheiser MOMENTUM control protocol, as measured

Notes on what this program sends and receives, and how certain we are of each
claim. Nothing here is invented: an id that nobody has observed is labelled
*probe* and is only ever sent by `momentum probe`, never by a normal command.

## Certainty vocabulary

| Level | Meaning |
| --- | --- |
| **live** | Observed on a MOMENTUM 5, BD_ADDR `80:C3:BA:C5:BB:97`, driven by this program, with the headset's behaviour seen to change. |
| **confirmed** | In btsnoop captures of the official Smart Control app driving a MOMENTUM True Wireless 4, and byte-identical in an independent MOMENTUM 4 tool. |
| **probable** | Documented for one MOMENTUM model; same Qualcomm GAIA family, not yet seen here. |
| **probe** | Plausible id from the Qualcomm GAIA basic command set. Sent only on request; answer printed raw. |

## Transport

The control channel is **RFCOMM**, not Bluetooth LE. The headset publishes it as
an SDP service:

| Service | UUID | Notes |
| --- | --- | --- |
| GAIA control | `a2129ff3-081b-4c45-8afe-469d9c4842ec` | **live**: Windows enumerates two entries for it, both named "GAIA", server channels 0 and 1. |
| Secondary vendor service | `df21fe2c-2515-4fdb-8886-f12c4d67927c` | **live**: published, name reported as `Rfcomm Service {…}`. Not needed for anything this program does. |

On Windows, `RfcommDeviceService::GetDeviceSelector` + `FindAllAsyncAqsFilter`
performs the SDP lookup and `StreamSocket::ConnectAsync(host, "{uuid}")` opens
the DLC — no channel numbers, no admin rights, no pairing changes. Three things
cost real time to get right and are worth repeating:

1. **A DLC that opens proves nothing.** Windows completes the connect even when
   nothing on the headset is listening, so a session is only live after the
   `0x0107` registration ack. If a command goes unanswered, compare sent and
   received byte counts (`tx 9 bytes, rx 0 bytes` = the headset is silent;
   `rx 40 bytes, 40 dropped` = we are reading the wrong thing).
2. **Reads must be partial.** `DataReader::LoadAsync(512)` completes when *512
   bytes* have arrived. GAIA frames are 8–263 bytes, so that read waits forever
   and the channel looks dead while the headset is answering normally. Use
   `IInputStream::ReadAsync(buffer, 512, InputStreamOptions::Partial)`; this was
   the single largest source of false conclusions during development.
3. **WinRT objects are neither `Send` nor `Sync`.** `IInputStream` wraps a bare
   `NonNull`, so all socket handles live on one worker thread which is fed
   through channels. `RoInitialize(RO_INIT_MULTITHREADED)` must be called on that
   thread too.

Enumerating is safe and read-only: nothing is written until a command is sent.

## Frame format

```text
offset  size  field
0       1     0xFF            sync
1       1     kind            0x03 host → headset, 0x04 headset → host
2       1     0x00            reserved, must be zero
3       1     payload length  payload only, header excluded
4       2     vendor id       big endian
6       2     command id      big endian
8       n     payload
```

There is **no application-layer checksum**. The FCS-8 at the end of the capture
belongs to RFCOMM and is added and verified by the OS Bluetooth stack; a capture
taken below that layer (btsnoop) shows it, a capture taken through the OS does
not.

Registering the battery feature, byte for byte (**live**, this is what `status`
sends first):

```text
ff 03 00 01 04 95 00 07 03
│  │  │  │  └─┴──┴──┴──┴── vendor 0x0495, cmd 0x0007, payload [0x03] = feature battery
│  │  │  └──── payload length 1
│  │  └─────── reserved
│  └────────── command
└───────────── sync
```

and the headset's answer:

```text
ff 04 00 00 04 95 01 07        register ack, empty payload
ff 04 00 01 04 95 06 82 01     charging push,   charging = yes
ff 04 00 01 04 95 06 83 64     battery push,    100 %
```

## The three bits of a command id

Replies and pushes are not distinguished by a flag byte; they are the command id
with bits folded in:

| Form | Expression | Example |
| --- | --- | --- |
| reply | `cmd \| 0x0100` | `0x0603` → `0x0703` |
| notification (push) | `cmd \| 0x0080` | `0x0603` → `0x0683` |
| refusal | `cmd \| 0x0180` | `0x0007` → `0x0187`, payload = error code |

The base command is `cmd & !(0x0180)`, which is how the tables here are indexed.

Refusals carry a Qualcomm GAIA error code (**live** for 0x01 and 0x05):

| Code | Name | Seen |
| --- | --- | --- |
| 0x00 | no error | live, oddly, as `0x0783 [00]` |
| 0x01 | unexpected code | live: `0x0003` under vendor `0x0495` |
| 0x02 | invalid message size | |
| 0x03 | invalid parameters | |
| 0x04 | not ready | |
| 0x05 | not supported | live: `0x0007` under vendor `0x001D` |
| 0x06 | not initialised | |

A refusal matters operationally: a firmware that does not own the vendor id in
your frame **answers with an error**, it does not go silent. Treating that as a
timeout sends you hunting for a dead channel that is actually alive.

## Two vendor namespaces

Most commands belong to Qualcomm's namespace `0x0495`. A few belong to
Sennheiser's own `0x001D` — Bluetooth SIG company id 29, which is also what
Windows reports for this device as `ManufacturerId`. Live evidence, same frame,
two vendor ids:

| Vendor | `0x0007` register | `0x0003` | `0x0603` battery |
| --- | --- | --- | --- |
| `0x0495` | `0x0107` ack ✓ | `0x0183 [01]` refused | `0x0703 [64]` ✓ |
| `0x001D` | `0x0187 [05]` not supported | `0x0103 "8000800326034155"` **the serial number** | `0x0783 [00]` |

So the serial number is only obtainable in Sennheiser's namespace, which is why
`Session::request_vendor` exists and `Session::serial()` uses it. Expect other
vendor-specific commands to behave the same way.

It is not a one-off: `0x0001`–`0x0006` turn out to be a **device-info block that
answers only under `0x001D`**. The same six ids under `0x0495` each refuse with
`0x018x [01]`, unexpected code.

| Id | Reply under `0x001D` | Payload, read live from `80:C3:BA:C5:BB:97` |
| --- | --- | --- |
| `0x0001` | `0x0101` | `00 0d 01 0c 01 06 02 04 02 07 01 00 05` — a feature list; layout not decoded, so nothing in the code reads it |
| `0x0002` | `0x0102` | `[00]`, meaning unknown |
| `0x0003` | `0x0103` | `8000800326034155` — serial number |
| `0x0004` | `0x0104` | `MOMENTUM 5` — product name |
| `0x0005` | `0x0105` | `6.21.1 / 2026-05-20 06:46:42+00:00` — firmware version and build stamp |
| `0x0006` | `0x0106` | empty |

For contrast, `0x0001` under `0x0495` answers with 27 bytes instead of 13, so the
two namespaces do not merely differ in which ids exist — they answer the same id
with different content. Try `0x001D` before concluding an id is dead.

## Commands

| Id | Reply | Push | Meaning | Level |
| --- | --- | --- | --- | --- |
| `0x0007` | `0x0107` | | Register feature, payload = feature id. The ack means the session is live. | **live** |
| `0x0003` | `0x0103` | | Serial number, ASCII digits, **vendor `0x001D` only**. | **live** |
| `0x0004` | `0x0104` | | Product name, ASCII, **vendor `0x001D` only**. | **live** |
| `0x0005` | `0x0105` | | Firmware version and build stamp, ASCII, **vendor `0x001D` only**. | **live** |
| `0x0602` | `0x0702` | `0x0682` | Charging / in-case state. | **live** as a push |
| `0x0603` | `0x0703` | `0x0683` | Battery, one byte per unit, `0xFF` = not reported. | **live** |
| `0x1A04` | `0x1B04` | | Set ANC, `01`/`00`. | **live** |
| `0x1A05` | `0x1B05` | `0x1A85` | Query ANC. | **live** |
| `0x1A02` | `0x1B02` | `0x1A83` | Set transparent-hearing level 0–100, `0` off. | **live** |
| `0x1A03` | `0x1B03` | | Query the level. | **live** |
| `0x1804` | `0x1904` | `0x1885` | Transparency on/off. | confirmed (TW4), not exercised here |
| `0x1803` | `0x1903` | | Query transparency; the reply is a **level**, not a flag. | confirmed |
| `0x1008` | `0x1108` | | Set bass boost. | **live** |
| `0x1009` | `0x1109` | | Query bass boost. | **live** |

Features for `0x0007`: `0x03` battery (**live**), `0x04` unit state, `0x08` user
EQ (probable, MOMENTUM 4), `0x0C` transparency, `0x0D` ANC — the last four
confirmed on TW4.

### Battery width differs by model

The MOMENTUM 5 answers `0x0603` with **one** byte (`[64]` = 100 %). MOMENTUM True
Wireless answers with **three** (left, right, case) — the TW4 capture's
`0x0683 [50 50 3C]` is 80 %, 80 %, 60 %. The decoder fills as many of its three
slots as the payload supplies, so both work, and `0xFF` means "not reported"
rather than zero.

## Traps in this firmware

Three behaviours that will bite anyone writing a client, all observed here:

1. **Set-acks carry no state.** `0x1B04` and `0x1904` arrive with an empty
   payload. Confirming a write means querying afterwards, which is what
   `Session::set_and_confirm` does; trusting the ack leaves a UI showing a state
   the headset may not have entered.
2. **ANC and transparency are exclusive, with no push for it.** Turning
   transparency on does *not* push `anc(false)`. A client that treats ANC as a
   single "mode" enum desyncs the moment the user presses the ear-cup button, so
   both flags are stored independently and the mode is derived.
3. **`0x1803` replies with a level.** Its name suggests a boolean; the payload is
   0–100.

## Reverse-engineering workflow

```console
# What answers at all, and how? Read-only, prints every reply incl. unknown ids.
momentum probe

# One specific frame, e.g. a GAIA basic-set query.
momentum probe --cmd 0004 --payload ""

# Everything the headset volunteers, decoded. Press buttons on the ear cup.
momentum watch --seconds 30

# The same without touching the headset: unsolicited traffic only, raw.
momentum watch --raw --seconds 60
```

Rules that keep this file trustworthy:

- An id earns a row here only with an observed frame. Anything else stays a
  *probe* and is printed as hex, never decoded into state.
- New findings are recorded with the model and, where possible, the firmware —
  the MOMENTUM 4, MOMENTUM 5 and True Wireless families share a protocol but
  differ in payload widths and in which commands they accept.
- `Event::decode` deliberately turns an unmapped frame into `Event::Unknown`,
  keeping the bytes so a capture can be re-read later. Decoders guess; this one
  files.

For a full capture below the OS — including the RFCOMM headers and FCS, and what
the Sennheiser phone app actually sends — take a btsnoop log on Android
(Developer options → Enable Bluetooth snoop dump log, then reproduce the action)
or `btmon -w capture.log` on Linux, then filter for the two UUIDs above. That is
where the *confirmed* rows came from, and it remains the way to settle anything
marked probable or probe.

## What is deliberately not on this channel

**Volume.** Headphone volume is negotiated over AVRCP as "absolute volume", which
the Windows audio stack drives; no capture has shown a set-volume command on the
GAIA channel. `momentum volume` therefore uses `IAudioEndpointVolume`, and that
was verified live: setting the endpoint changed the level the headset reports, and
the tray slider moves with it. The code sits in `src/volume.rs`, outside the
protocol modules, so nobody reads it as a command the headset implements but this
tool happens not to use. **Mute** is the same story and the same interface: it is
an endpoint property, not a volume of zero, and verified to survive a volume read
unchanged.

Two findings from the same measurement, both of which cost time:

- With a USB-C cable connected Windows lists **two** endpoints for one headset —
  `Headphones (MOMENTUM 5)` and `Headphones (2- MOMENTUM 5)` — so "the volume" is
  undefined until a device is chosen. The Bluetooth one is the default.
- WinRT names audio devices by *device interface path*,
  `\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{guid}#{e6327cad-…}`, while
  `IMMDeviceEnumerator::GetDevice` wants the `{0.0.0.00000000}.{guid}` inside it —
  the form `MediaDevice::GetDefaultAudioRenderId` returns too. Handing it the path
  fails with `0x80070057`, which reads like a bad argument rather than a wrong
  naming layer.

## Still unknown

- **Find my headphones.** No confirmed id. It will be a command that makes sound,
  so it should be found in a capture of the phone app rather than by probing.
- **User EQ.** Feature `0x08` and the MOMENTUM 4 tool agree on the ids; the MOMENTUM 5
  payload layout is unmeasured, and a wrong-length payload is the surest way to
  get `0x02` back.
- **What `0x0001` and `0x0002` mean in the Sennheiser namespace.** Both answer, and
  the 13-byte `0x0101` looks like a feature/version list. Decoding it is a
  reading problem, not a missing command.
- **Linux.** Same protocol over any RFCOMM socket: implement `Link` with
  `bluer`'s SDP lookup and `connect(2)` on `BTPROTO_RFCOMM`, and everything above
  `src/link.rs` is already portable.
