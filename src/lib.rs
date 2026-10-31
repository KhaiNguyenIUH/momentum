//! Control library for Sennheiser MOMENTUM headphones over Bluetooth RFCOMM.
//!
//! The Sennheiser Smart Control app does not use BLE for control. It opens a
//! BR/EDR RFCOMM channel to a private SDP service on the headset and speaks a
//! Qualcomm GAIA-flavoured binary protocol on it.
//!
//! Wire format:
//!
//! ```text
//! FF <kind> 00 <payload_len> <vendor:2 BE> <cmd:2 BE> <payload..>
//! ```
//!
//! Verified against two independent reverse-engineering efforts — btsnoop
//! captures of the MOMENTUM True Wireless 4 and a Linux tool for the MOMENTUM 4
//! — and then command by command against a live MOMENTUM 5. `docs/PROTOCOL.md`
//! records the evidence and the certainty level of every id; nothing is decoded
//! from an id nobody has observed.
//!
//! Two firmware behaviours shape this API: set-acks arrive empty, so a write is
//! only confirmed by reading the value back (see [`Session::set_anc`]), and noise
//! cancelling and transparent hearing are exclusive without the headset ever
//! pushing `anc(false)`, which is why [`DeviceState`] keeps both flags.
//!
//! RFCOMM is a byte stream, so frames arrive glued together and split across
//! reads. [`proto::frame::Framer`] reassembles them and resynchronises after
//! noise.

pub mod error;
#[cfg(windows)]
pub mod host;
pub mod link;
pub mod proto;
#[cfg(windows)]
pub mod rfcomm;
pub mod session;
#[cfg(windows)]
pub mod volume;

pub use error::Error;
pub use link::{Link, MockLink};
pub use proto::cmd;
pub use proto::frame::{Frame, FrameKind, Framer, VENDOR_QUALCOMM, VENDOR_SENNHEISER};
pub use proto::state::{APP_BANDS_HZ, Battery, DeviceState, Equaliser, Event};
pub use session::{DEFAULT_FEATURES, DEFAULT_TIMEOUT, Session};

/// SDP service class id of the GAIA control channel.
///
/// Confirmed present on a paired MOMENTUM 5 (Windows `BTHENUM` service
/// registry) and on the MOMENTUM True Wireless 4 captures. A second vendor
/// service, `df21fe2c-2515-4fdb-8886-f12c4d67927c`, is also published.
pub const GAIA_SERVICE_UUID: &str = "a2129ff3-081b-4c45-8afe-469d9c4842ec";

/// Vendor UUID published by the second Sennheiser control service.
pub const SECONDARY_SERVICE_UUID: &str = "df21fe2c-2515-4fdb-8886-f12c4d67927c";
