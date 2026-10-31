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

## The equaliser

Feature `0x08` — listed as *probable, from the MOMENTUM 4* — is real on this
firmware, and registering it is the read path. `0x0007 [08]` answers the usual
`0x0107` ack and then pushes the whole configuration unsolicited:

| Push | Base | Payload | Read as |
| --- | --- | --- | --- |
| `0x1082` | `0x1002` | `00 00 00 00 00 00 00 00` | user gains, one signed byte per band — all flat on this unit |
| `0x108b` | `0x100b` | `00 00 5a 01 01 45 02 05 dc …` | eight (band, u16 BE) rows: 90, 325, 1500, 2500, 4500, 8000, 13000, 16000 — ascending, probably hertz, **not the graphic EQ's bands** |
| `0x108d` | `0x100d` | `00 08 00 01 0b 5c …` | eight (band, u16 BE) rows: 0x0800 for band 0, 0x0b5c for the rest — unit unknown |
| `0x108f` | `0x100f` | `00 0e 01 0e … 07 0e` | eight (band, u8) rows, all 14 — a per-band limit, unit unknown |
| `0x1091` | `0x1011` | `00 00 00 01 00 00 …` | eight (band, u16 BE) rows of user gain, all zero |
| `0x1093` | `0x1013` | `00 00` | two bytes, meaning unknown |
| `0x1089` | `0x1009` | `00` | this is the **bass boost** push, not EQ — the ids sit in the same range |

**Retracted: this table is not the equaliser's band set.** An earlier version of
this section read those eight numbers as the centre frequencies, and reasoned that
eight ascending values in the plausible audio range "is exactly what the app
offers" — which was not true and was never checked. **Smart Control's graphic EQ
offers 50, 100, 250, 600, 1.4 k, 3 k, 7 k and 12 k Hz.** Both sets have eight
members and both ascend; neither scales into the other (50 → 90 is ×1.8, 12000 →
16000 is ×1.33), so this is a different filter bank, and its purpose is unknown.
It is printed as `0x100b`, never as hertz, and the app's real band labels live in
`APP_BANDS_HZ`.

The lesson is worth keeping beside the mistake: an ascending run of numbers in a
plausible unit is not a band set. That the gains were calibrated correctly, against
the app's own slider labels, is exactly why the frequencies could not be trusted by
the same route — nobody can read a decibel off a frequency.

**The row layout is inference from one device; the ids and payloads are
measurements.**

### Indexed reads, and the even/odd rule

These ids are push-only as bare reads — `0x1002`/`0x100b`/`0x100d`/`0x1011` with an
empty payload all answer `0x018x`… `0x118x [05]`, not supported. They take a **band
index**:

| Query | Reply | Meaning |
| --- | --- | --- |
| `0x1003 []` | `0x1103 [00 ×8]` | all eight user gains at once |
| `0x100b [i]` | `0x110b [i, 00, 5a]` | centre frequency of band `i` |
| `0x100d [i]` | `0x110d [i, 08, 00]` | that row's value for band `i` |
| `0x100f [i]` | `0x110f [i, 0e]` | the per-band limit |
| `0x1011 [i]` | `0x1111 [i, 00, 00]` | user gain of band `i` as u16 |
| `0x1013 [i]` | `0x1113 [00, 00]` | unknown |

Even ids write, odd ids read — the same rule as bass boost (`0x1008`/`0x1009`) and
the ambient level (`0x1A02`/`0x1A03`). Confirmed by writing values the device had
just reported, so nothing changed:

- `0x1010 [00, 00, 00]` → ack `0x1110 []`, then `0x1091` re-pushed the full table.
- `0x1012 [00, 00]` → ack `0x1112 []`, then `0x1093` re-pushed.
- `0x100e [00, 00, 0e]` → **refused `[05]`**: not every even id is writable, and the
  frequencies and limits are read-only, which is the right way round.

Still missing before `momentum eq` can take decibels instead of raw bytes: the
**scale and sign of a gain byte** (is `01` = 0.5 dB? is the range ±14, given the
limit row?) and whether `0x1002` takes `(band, gain)` — eight bytes were refused
where one byte was accepted. Both need one audible write, which is easy to undo
but should be a deliberate act, not a side effect of probing.

### What a slider drag looks like on the wire

Captured over the PC connection while a phone running Smart Control moved the
custom-EQ sliders: the MOMENTUM 5 accepts two connections at once, and every
change the phone makes is **pushed to the other connection too**. That makes a
second host an observer of the app's own writes, which is a better way to learn
this protocol than guessing.

Two facts came out of it.

**The app writes one band per command.** A preset selection arrives as eight
`0x1082` pushes in band order, each carrying all eight gains with one field
changed — so the payload is a full eight-byte table, not a single band, and a
preset is eight writes:

```
-20,0,0,0,0,0,0,0     -20,0,22,0,0,0,0,0     -20,0,22,27,0,0,0,0   …
```

**Gains are signed bytes and finer than 1 dB.** Manual drags step by single
units (`…, 5, -3, -4, -2, 0`) and the observed span across presets and drags is
**-35 … +30**, well outside the `14` reported by `0x100f` — so that per-band
value is not the slider's limit either. Preset curves seen (bands 0–7, raw):

```
-20,  0, 22, 27, 26, 11, -10, -26
 30,  0,-15,-19,-12,  0,   5,  10
  0,-20,-18,  0, 12, 22,  10, -10
-35,-20, -8,  0,  8, 12,  10,   5
 25, 18,  5,-12,-15,  0, -15, -15
```

**The scale is now measured: ten raw units per decibel.** Setting every band to
−6 dB in Smart Control and reading the table gave **−60 on all eight**; +2 dB gave
**+20**. Flat is 0, so the sign is plain two's complement with no 128-style offset
and the scale holds in both directions. That reinterprets the earlier capture
correctly: the preset extremes of −35 and +30 were **−3.5 dB and +3.0 dB**, and the
single-unit steps during a drag were 0.1 dB. The `14` reported by `0x100f` is
confirmed *not* to be the slider range, since −60 sits comfortably outside it.

Neither the `0x100d` table (still `2048` / `2908`) nor the `0x1013` state (still
`[00, 00]`) moved at −6 dB or at +2 dB, so neither is the applied-gain table or a
level-compensation field. Both remain unnamed.

**The ramp closes the calibration and separates the two index spaces.** Setting the
app's eight sliders to −6, −5, −4, −3, −2, −1, 0, +1 dB read back as −60, −50, −40,
−30, −20, −10, 0, +10: seven distinct points, both signs, zero at zero, so the
scale is linear with no offset. And because each band held a *different* value, the
ordering is now demonstrated rather than assumed — band *n* of the byte table is
band *n* of the app, lowest frequency first.

Meanwhile `0x100b`, `0x100d`, `0x100f` and `0x1011` did not move by a single byte.
Those tables carry explicit band indices, and nothing has ever connected that index
space to the app's bands. They are therefore printed separately and labelled
*index space unproven*; lining them up against the app's frequencies would be a
relationship invented for the sake of a tidy table.

### Loudness does not track the curve

Reported while measuring: **−6 dB across all eight bands and +2 dB across all eight
bands sounded equally loud.** Eight decibels of difference in the stored curve with
no difference in perceived level means something normalises the output — a pre-amp
trim, or a feature like adaptive hearing deciding for itself.

That is a listener's observation rather than a wire measurement, but it constrains
how everything below may be read. Any argument of the form *"nothing was audible,
therefore the field does nothing"* is weaker than it looks while level is being
adjusted underneath. So the u16 gain table in the next section is recorded as
**not proven audible** — which is what was measured — rather than *inert*, which is
what was assumed. A single band carved 12 dB while its neighbours stay flat is the
test that survives normalisation, and it has not been done.

What remains unknown about the byte store is the **write command**, not the unit:
`0x1002` refused both the eight-byte table and the `(band, gain)` pair, so writing
it is still the app's own affair.

**One correction to note.** Eight-byte pushes prove the *notification* carries a
whole table, and the band-by-band progression implies eight separate writes, but
that is inference about the phone's commands, not an observation of them: our own
`0x1002` with eight bytes was refused with `05`, while one byte was accepted. So
the id and shape the app actually writes are still unidentified — possibly a
different command, possibly the second GAIA service that `rf-scan` shows on RFCOMM
channel 1. **A second connection observing the first is not the same as seeing the
first connection's frames.**

### The u16 store: range-checked, stored, and not audible

`0x1010` writes the u16 gain of one band. It is **range-checked**: on a MOMENTUM 5,
band 3 accepted `-50`, `-140`, `-180`, and refused `-182`, `-190`, `-200`, `-2000`
with `0x1190 [82]` — so the boundary is 180 or 181 by magnitude, and **error `0x82`
is a range violation**, distinct from the `01` and `05` seen elsewhere. Values
inside the bound are stored and echoed back; restoring zero reads back flat.

And yet **nothing is audible**. `-50` on the 90 Hz band and every value since left
the sound unchanged, while across 170 frames of app-driven slider movement only the
signed-byte table ever changed.

An earlier version of this document read "accepts any value, no clamp" and
concluded the field was scratch space. **That was wrong** — it was measured at one
value, `-50`, and `-2000` disproves it. The corrected reading is less comfortable
but more honest: a range-checked field that stores what it is given and does nothing
audible is most consistent with **the equaliser being switched off**, or with this
table feeding a path that a separate enable governs. `0x1013`/`0x1012` answers
`[00, 00]` and is writable; that is the obvious candidate for such a switch, and it
has not been tested.

So: an ack here means *stored*, not *applied*; acceptance is not understanding. This
crate **reads both tables and writes neither**, and a headset behaving oddly for a
reason spread across six sessions of probing is worse than a tool that says *not
implemented*.

### Writing the live table: what has been ruled out

The byte store reflects every slider move ever made, and refuses every write this
host has attempted. What has been excluded, rather than assumed:

- **Not a collision with the phone.** The same attempts were made with the phone's
  Bluetooth switched off, and every refusal was byte-identical. The theory that the
  headset ignored writes while the app sat on the EQ screen is dead.
- **Not the payload shape alone.** `0x1002` was tried with one byte, with a
  `(band, gain)` pair and with a whole eight-byte table; `0x100a` and `0x100c` — the
  even neighbours the read ids point at — took the `(band, gain)` pair. All answered
  `05`, not supported.

Two things came out of the sweep instead. `0x1002` with a single byte answers with a
value that has always matched that band's stored gain, so it reads like a per-band
read rather than the write its even number suggests — the even/odd rule has now
broken here twice. And **`0x1014` answers `[00, 3c]` to everything it is given**:
`0x3c` is 60, which is **+6.0 dB under the calibration**, and 6 dB is the range the
app's sliders have. That is the most likely reading — a gain ceiling — but it is a
hypothesis about a fixed answer, not a decoded field. `0x1016` refuses with `01`
where the others refuse with `05`, which says the id is known and only the code is
wrong; that is a thread worth pulling later.

The command the phone writes with has still never been seen by any host but the
phone. Watching cannot help further: a second connection is notified about *state*,
it does not mirror another connection's *commands*. The remaining instruments are a
controller-side snoop log, or the second GAIA service that `rf-scan` shows on
RFCOMM channel 1, which nothing has ever connected to.

### Which store survives a disconnect

The two stores behave completely differently, and only one of them is durable.

Written to `0x1010` as a whole table — `+60, +50, +30, +10, −10, −30, −50, −60` —
the values were still there on every read afterwards, through many connections and
through the phone's Bluetooth being switched off. **`0x1010` accepts the entire
24-byte table in a single frame**, which `0x1002` never did: one byte, a `(band,
gain)` pair and a full eight bytes were all refused there.

**Corrected: the byte store is not volatile.** The paragraph that stood here claimed
it was, on the strength of one observation — a curve read as the app's `−6 … +1` ramp
while the phone was connected, and as eight zeros a minute after its Bluetooth went
off. A power cycle settles what that observation could not. A curve dialed in the app
(`−3, +2, −1, 0, 0, +2, +1, +1`) survived the headset switched off at the button,
USB-C unplugged, the phone's and the laptop's radios off, no host of any kind able to
reach it — and came back byte-identical.

What fits every observation is **two tiers**: something is held non-volatile and
restored at power-up, and a connected controller can also push a curve that lives
only for the session, which is what the app does on connect. Reverting to the stored
value when the link dropped is exactly what a second host would see. Which writes
reach the non-volatile tier — a slider drag, a preset push, only an explicit save — is
**not settled**, and the experiment that would have separated them was ruined by my
own tidiness: the u16 store was zeroed before the power cycle, so a test that could
have told flash from RAM came back with nothing to look for.

The consequence for this tool is the good kind. If the user curve lives in flash, the
write command worth finding is a feature rather than a footnote: a curve set from a
PC would outlast the headset being switched off.

`0x1012` writes persist, and the two paths disagree on byte order — `0x1013` echoes
what was stored while `0x1093` reports the pair reversed, so a value of 1 written as
`01 00` comes back as `[00, 01]` on one path and `[01, 00]` on the other. It is not
a profile selector: states `00`, `01`, `02` and `03` all reported the same gain
curve.

### The app does not read the writable store

Decisive, and cheap: `0x1010` was loaded with `+6.0 … −6.0`, verified on a second
connection, and the phone was then brought back and its saved preset selected. The
app showed its own `−6 … +1` curve and pushed it into the byte table, while the u16
values sat underneath, byte-identical. So **the two stores share nothing** — the
phone keeps its presets locally and pushes them down with a command no second
connection has ever seen.

That gives the missing write command a *when* without giving it a *what*: it fires
on preset selection, at an instant we can point at, on a link we are not party to.
A second GAIA client is told about state; it is not mirrored another connection's
commands, so no amount of watching from here will capture it.

### The listening test: this store does not reach the ears

The last excuse for the u16 store was that level normalisation had been hiding
small single-band changes. That excuse is gone. The whole table was written as
`−6, −5, −3, −1, +1, +3, +5, +6` dB and then as its mirror image — no band left at
zero, the entire tilt reversed — switched back and forth by `momentum ramp1` and
`ramp2` with music playing and someone listening for it. **No change in the sound.**

So this store is writable, range-checked, durable across connections and
disconnects, and **not the user equaliser**. Together with the two facts before it —
the app neither reads it nor clears it, and the byte table is the one that moves —
the likeliest reading is a factory or service table, or a reserved one. It stays
documented rather than deleted precisely because it looks like an equaliser and is
one, which is a trap worth labelling.

**Non-volatile as well.** With the headset switched off at the button — phone and
laptop radios off, USB-C unplugged, both RFCOMM channels verified unreachable, no
process on the PC holding a link — the table came back byte-identical, exactly as the
user curve beside it did. So **both writable stores live in flash**, which turns
`momentum ramp1` and `ramp2` from probes into edits of persisted state, and makes this
table stranger rather than duller: a per-band store with a limit field and a range
check, kept faithfully across power cuts, that nothing so far can be found to switch
on.

The remaining way to change this headset's sound from a PC is the write command for
the **byte** store, and it is not in the range tried: `0x1002`, `0x100a`, `0x100c`
and `0x100e` all answer `05` with the phone connected or not.

### RFCOMM channel 1 is the same server

The headset publishes the control service twice — `rf-scan` lists `RFCOMM:00000000`
and `RFCOMM:00000001` under one UUID — and until `probe --channel` existed, every
measurement in this document had gone to whichever the OS listed first. The second
channel was the last place a different, more permissive command set could have been
hiding, on the general theory that Qualcomm parts sometimes put tuning on a second
control channel.

It is the same server. On both channels: the same register ack, the same battery and
charging pushes, the same serial under vendor `0x001D`, the same seven-table equaliser
response to registering feature `0x08` — table for table, byte for byte — and the same
`0x1182 [05]` refusal for the `(band, gain)` write. A second connection is a second
connection, nothing more. `--channel` stays in the tool because proving that required
it, and because the next person to wonder deserves to answer it in one command.

## Still unknown
- **Find my headphones.** No confirmed id. It will be a command that makes sound,
  so it should be found in a capture of the phone app rather than by probing.
- **User EQ command shape.** See *The equaliser* — the ids, the band table and the
  write acks are measured; the dB scale and the multi-band write shape are not.
- **The dB scale of an EQ gain**, and the shape `0x1002` wants when writing gains.
  See *The equaliser* above; both need one deliberate audible write.
- **`0x1a81`, seen twice during the same capture**, payload `01 01 03 00 02 00` and
  once as `01 00 03 00 02 00`. That is `0x1a01` with the notification bit, so it
  belongs to the noise-control feature and the shape reads like three (mode, value)
  pairs — plausibly the list of available ANC modes, which is exactly the kind of
  thing a headset sends when a mode list is asked for or a mode changes. It arrived
  without this host asking, so the phone's app triggered it. Unnamed, unparsed.
- **What `0x0001` and `0x0002` mean in the Sennheiser namespace.** Both answer, and
  the 13-byte `0x0101` looks like a feature/version list. Decoding it is a
  reading problem, not a missing command.
- **Linux.** Same protocol over any RFCOMM socket: implement `Link` with
  `bluer`'s SDP lookup and `connect(2)` on `BTPROTO_RFCOMM`, and everything above
  `src/link.rs` is already portable.
