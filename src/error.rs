//! Crate-wide error type. Std-only on purpose: no `thiserror` dependency.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// No Bluetooth radio, or the stack refused to create an RFCOMM socket.
    RadioUnavailable(String),
    /// The headset is not reachable / did not accept the RFCOMM connection.
    Connect(String),
    /// Socket-level failure while reading or writing.
    Io(std::io::Error),
    /// The headset closed the RFCOMM channel. Expected when it powers down or
    /// goes into its case; the control service is not offered while idle.
    Disconnected,
    /// A command was sent but the headset never answered as expected.
    Timeout { cmd: u16, waited_ms: u128 },
    /// The headset answered with something we did not expect.
    UnexpectedFrame { expected: u16, got: u16 },
    /// The device was found but the GAIA service is not published on it.
    ServiceNotFound(String),
    /// A required feature is not implemented by this build.
    Unsupported(&'static str),
    /// The headset answered an error frame: it knows the command but will not
    /// carry it out, or the vendor id in the frame is not its own. The message
    /// carries the decoded GAIA error code.
    Refused(String),
    /// The Windows audio endpoint API refused the request, or the device named
    /// has no volume interface. Volume is an OS concern, not a headset command.
    Audio(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::RadioUnavailable(s) => write!(f, "no usable Bluetooth radio: {s}"),
            Error::Connect(s) => write!(f, "could not connect to the headset: {s}"),
            Error::Io(e) => write!(f, "socket error: {e}"),
            Error::Disconnected => write!(f, "the headset closed the control channel"),
            Error::Timeout { cmd, waited_ms } => write!(
                f,
                "timed out after {waited_ms} ms waiting for a reply to {cmd:#06x}"
            ),
            Error::UnexpectedFrame { expected, got } => {
                write!(f, "expected reply to {expected:#06x}, got {got:#06x}")
            }
            Error::ServiceNotFound(s) => {
                write!(f, "GAIA control service not found on the device: {s}")
            }
            Error::Unsupported(s) => write!(f, "not supported by this build: {s}"),
            Error::Refused(s) => write!(f, "the headset refused the command: {s}"),
            Error::Audio(s) => write!(f, "audio endpoint error: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
