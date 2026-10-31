//! Windows transport: the headset's private RFCOMM service, via WinRT.
//!
//! `RfcommDeviceService` is the supported way to reach a proprietary serial
//! profile on Windows: Windows performs the SDP lookup for a service UUID and
//! hands back a service object that a `StreamSocket` can connect to. SDP,
//! channel negotiation and pairing stay inside the OS.
//!
//! All WinRT handles live on one worker thread. `IInputStream` and friends wrap
//! a bare `NonNull`, so they are neither `Send` nor `Sync`, and `DataReader` has
//! no timeout of its own: the socket is only ever touched there, and the public
//! side talks to it over channels so [`Link::read_frame`] can honour a deadline.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use windows::Devices::Bluetooth::Rfcomm::{RfcommDeviceService, RfcommServiceId};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Networking::Sockets::StreamSocket;
use windows::Storage::Streams::{DataWriter, IBuffer, InputStreamOptions};
use windows::core::HSTRING;

use crate::error::{Error, Result};
use crate::link::Link;
use crate::proto::frame::{Frame, Framer};

/// WinRT `AsyncStatus`, compared by value so this module does not depend on
/// where the enum happens to be re-exported.
#[allow(dead_code)]
pub(crate) mod status {
    pub const STARTED: i32 = 0;
    pub const COMPLETED: i32 = 1;
    pub const CANCELED: i32 = 2;
    pub const ERROR: i32 = 3;
}

/// WinRT errors become socket errors, so `?` works throughout this module.
impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Io(std::io::Error::other(e.to_string()))
    }
}

/// Block on a WinRT async operation, whichever of the four async interfaces it
/// implements: they all expose `Status` and `ErrorCode`.
macro_rules! await_sync {
    ($op:expr) => {{
        let op = $op;
        loop {
            match op.Status()? {
                code if code.0 == $crate::rfcomm::status::COMPLETED => break,
                code if code.0 == $crate::rfcomm::status::CANCELED => {
                    return Err($crate::error::Error::Disconnected);
                }
                code if code.0 == $crate::rfcomm::status::ERROR => {
                    let detail = match op.ErrorCode() {
                        Ok(code) => format!("headset call failed: {code}"),
                        Err(_) => "headset call failed: unknown WinRT error".into(),
                    };
                    return Err($crate::error::Error::Connect(detail));
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
    }};
}

/// A GAIA control channel found on a paired device.
#[derive(Debug, Clone)]
pub struct RfcommTarget {
    /// Friendly name Windows reports, usually the device name.
    pub name: String,
    /// `DeviceInformation` id of the service; input to `FromIdAsync`.
    pub device_id: String,
    /// Service class UUID in braces, the socket's "remote service name".
    pub service_name: String,
}

/// Initialise WinRT on this thread. MTA is what a console worker wants, and an
/// already-initialised thread is not an error.
pub(crate) fn init_com() -> Result<()> {
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        // S_OK: initialised here. S_FALSE is an "error" code that means this
        // thread was already initialised, which is fine.
        Ok(()) => Ok(()),
        Err(code) if code.code().is_ok() => Ok(()),
        Err(code) => Err(Error::RadioUnavailable(format!("RoInitialize: {code}"))),
    }
}

/// Parse a UUID. `windows_core::GUID` deliberately has no string parser, so
/// this is the canonical form: 8-4-4-4-12 hex digits, braces optional.
pub(crate) fn parse_guid(text: &str) -> Result<windows::core::GUID> {
    let digits: String = text
        .chars()
        .filter(|c| !matches!(c, '-' | '{' | '}' | ' '))
        .collect();
    if digits.len() != 32 {
        return Err(Error::ServiceNotFound(format!(
            "\"{text}\" is not a UUID: expected 32 hex digits"
        )));
    }
    u128::from_str_radix(&digits, 16)
        .map(windows::core::GUID::from_u128)
        .map_err(|e| Error::ServiceNotFound(format!("\"{text}\" is not a UUID: {e}")))
}

/// Enumerate paired devices publishing `uuid` as an RFCOMM service.
///
/// Windows names these entries after the *service* — the MOMENTUM 5's entry is
/// "GAIA" — so pass an address or a service name to filter, not a product name.
pub fn find(uuid: &str, needle: Option<&str>) -> Result<Vec<RfcommTarget>> {
    init_com()?;
    let guid = parse_guid(uuid)?;
    let service_id = RfcommServiceId::FromUuid(guid)
        .map_err(|e| Error::ServiceNotFound(format!("FromUuid: {e}")))?;
    let selector = RfcommDeviceService::GetDeviceSelector(&service_id)
        .map_err(|e| Error::ServiceNotFound(format!("GetDeviceSelector: {e}")))?;

    let finder = DeviceInformation::FindAllAsyncAqsFilter(&selector)?;
    await_sync!(&finder);
    let found = finder.GetResults()?;

    // The socket's "remote service name" is the service class UUID in braces.
    let service_name = format!("{{{uuid}}}");
    let needle = needle.map(|s| s.to_lowercase());
    let mut targets = Vec::new();
    for info in found {
        // Windows names the service entry after the service, e.g. "GAIA".
        let name = info.Name()?.to_string();
        let device_id = info.Id()?.to_string();
        if let Some(needle) = &needle {
            // Match the service name or the peer address, since the product
            // name appears in neither.
            let haystack = format!("{name} {}", peer_address(&device_id).unwrap_or_default());
            if !haystack.to_lowercase().contains(needle) {
                continue;
            }
        }
        targets.push(RfcommTarget {
            name,
            device_id,
            service_name: service_name.clone(),
        });
    }
    Ok(targets)
}

/// The peer's BD_ADDR, as embedded in a Windows `Bluetooth#...` device id.
///
/// Ids look like
/// `Bluetooth#Bluetoothb4:0e:de:4a:9e:39-80:c3:ba:c5:bb:97#RFCOMM:00000001:{guid}`:
/// the local radio address comes first, the peer second, so the second match
/// is the one we talk to.
pub fn peer_address(device_id: &str) -> Option<String> {
    let bytes = device_id.as_bytes();
    let mut found: Vec<String> = Vec::new();
    let mut index = 0;
    while index + 17 <= bytes.len() {
        let candidate = &device_id[index..index + 17];
        if is_bd_addr(candidate) {
            found.push(candidate.to_ascii_lowercase());
            index += 17;
        } else {
            index += 1;
        }
    }
    found.into_iter().nth(1)
}

/// True for `xx:xx:xx:xx:xx:xx` bounded by non-hex-colon characters, so a
/// fragment inside a longer run of hex is not mistaken for an address.
fn is_bd_addr(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 17 {
        return false;
    }
    for (index, &byte) in bytes.iter().enumerate() {
        if index % 3 == 2 {
            if byte != b':' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// The RFCOMM server channel Windows resolved, or 0 when it is left to SDP.
pub fn channel(device_id: &str) -> Option<u32> {
    let rest = device_id.split_once("RFCOMM:")?.1;
    u32::from_str_radix(rest.get(..8)?, 16).ok()
}

/// Requests the worker thread handles on our behalf.
enum Request {
    /// Write these bytes, then reply with the outcome.
    Write(Vec<u8>, Sender<Result<()>>),
    /// Close the socket and finish.
    Shutdown,
}

/// Progress from the worker while it brings the channel up.
enum Stage {
    /// A step the caller can print, so a stall is attributable.
    Progress(String),
    /// Terminal: the channel is open, or it failed.
    Done(Result<()>),
}

/// Connected GAIA control channel. Holds channels only, no WinRT handles.
pub struct WindowsRfcomm {
    requests: Sender<Request>,
    incoming: Receiver<Result<Vec<u8>>>,
    framer: Framer,
    label: String,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx_bytes: usize,
    tx_bytes: usize,
}

/// Connect to `target`.
///
/// The SDP lookup, RFCOMM connect and every later read happen on a worker
/// thread, so this returns only once the channel is really open.
pub fn connect(target: &RfcommTarget) -> Result<WindowsRfcomm> {
    let (request_tx, request_rx) = mpsc::channel::<Request>();
    let (chunk_tx, incoming) = mpsc::channel::<Result<Vec<u8>>>();
    let (stage_tx, stage_rx) = mpsc::channel::<Stage>();

    let owned = target.clone();
    let label = owned.name.clone();
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_flag = shutdown.clone();
    let progress = stage_tx.clone();
    let worker = thread::spawn(move || {
        // On success `serve` has already reported Ready; only a failure still
        // needs to be delivered here. The handle is dropped, so the pump thread
        // is detached and ends by itself when the session is dropped.
        if let Err(err) = serve(&owned, request_rx, &chunk_tx, &progress, &worker_flag) {
            let _ = stage_tx.send(Stage::Done(Err(err)));
        }
    });
    drop(worker);

    // Report each stage as it happens: a hang in SDP looks very different from
    // a hang in RFCOMM connect, and that difference is the whole diagnosis.
    loop {
        match stage_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Stage::Progress(message)) => println!("  {message}"),
            Ok(Stage::Done(Ok(()))) => break,
            Ok(Stage::Done(Err(err))) => return Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(Error::Connect(
                    "the Bluetooth stack did not answer within 20 s".into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Connect("the Bluetooth worker thread stopped".into()));
            }
        }
    }

    Ok(WindowsRfcomm {
        requests: request_tx,
        incoming,
        framer: Framer::new(),
        label,
        shutdown,
        rx_bytes: 0,
        tx_bytes: 0,
    })
}

/// An `IBuffer` of `size` writable bytes.
///
/// `IBuffer` is not activatable on its own, and `DataWriter::DetachBuffer` hands
/// over exactly the bytes it was given, which makes it the plainest way to
/// allocate one.
fn spare_buffer(size: u32) -> Result<IBuffer> {
    let writer = DataWriter::new()?;
    writer.WriteBytes(&vec![0u8; size as usize])?;
    Ok(writer.DetachBuffer()?)
}

/// Copy up to `count` bytes out of a buffer a read completed into.
fn take_bytes(filled: &IBuffer, count: usize) -> Result<Vec<u8>> {
    use windows::Storage::Streams::DataReader;
    let reader = DataReader::FromBuffer(filled)?;
    let mut out = vec![0u8; count];
    reader.ReadBytes(&mut out)?;
    Ok(out)
}

/// Write a frame's bytes and push them out to the radio.
fn write_bytes(writer: &DataWriter, bytes: &[u8]) -> Result<()> {
    writer.WriteBytes(bytes)?;
    let store = writer.StoreAsync()?;
    await_sync!(&store);
    let flush = writer.FlushAsync()?;
    await_sync!(&flush);
    Ok(())
}

/// Serve one queued request. `false` means the pump should stop.
fn serve_write(
    request: std::result::Result<Request, mpsc::TryRecvError>,
    writer: &DataWriter,
) -> bool {
    match request {
        Ok(Request::Write(bytes, ack)) => {
            let outcome = write_bytes(writer, &bytes);
            let ok = outcome.is_ok();
            ack.send(outcome).ok();
            ok
        }
        Ok(Request::Shutdown) => false,
        // The caller is gone; nothing left to serve.
        Err(mpsc::TryRecvError::Disconnected) => false,
        Err(mpsc::TryRecvError::Empty) => true,
    }
}

/// Open the channel and pump it until shutdown. Runs on the worker thread.
fn serve(
    target: &RfcommTarget,
    requests: Receiver<Request>,
    chunks: &Sender<Result<Vec<u8>>>,
    progress: &Sender<Stage>,
    shutdown: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;

    let _ = progress.send(Stage::Progress("looking up the service".into()));
    init_com()?;
    let opener = RfcommDeviceService::FromIdAsync(&HSTRING::from(target.device_id.as_str()))
        .map_err(|e| Error::Connect(format!("FromIdAsync: {e}")))?;
    await_sync!(&opener);
    let service = opener.GetResults()?;

    let _ = progress.send(Stage::Progress("opening RFCOMM".into()));
    let socket = StreamSocket::new()?;
    let host = service.ConnectionHostName()?;
    let connecting = socket.ConnectAsync(&host, &HSTRING::from(&target.service_name))?;
    // Windows will complete this DLC even when nothing on the headset is
    // listening, so failing here means the link itself is down: powered off, out
    // of range, or another host holding the channel.
    (|| -> Result<()> {
        await_sync!(&connecting);
        Ok(())
    })()?;
    let _ = progress.send(Stage::Progress("channel open".into()));
    // The caller can start talking now; the rest of this function is the pump.
    let _ = progress.send(Stage::Done(Ok(())));

    let writer = DataWriter::CreateDataWriter(&socket.OutputStream()?)?;
    let input = socket.InputStream()?;

    loop {
        // Read with Partial: a GAIA frame is 8..263 bytes and `LoadAsync`-style
        // reads wait for the *whole* requested count, which for a 512-byte
        // request means never. This is the difference between a live channel and
        // one that looks dead.
        let buffer = spare_buffer(512)?;
        let read = input.ReadAsync(&buffer, 512, InputStreamOptions::Partial)?;

        loop {
            // Serve every queued write while the read is outstanding. The
            // headset may say nothing for minutes; a command must never wait
            // for a reply that has not come.
            loop {
                match requests.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => break,
                    request => {
                        if !serve_write(request, &writer) {
                            return Ok(());
                        }
                    }
                }
            }
            match read.Status()? {
                code if code.0 == status::COMPLETED => break,
                code if code.0 == status::CANCELED || code.0 == status::ERROR => {
                    chunks.send(Err(Error::Disconnected)).ok();
                    return Ok(());
                }
                _ => {
                    if shutdown.load(Relaxed) {
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }

        let filled = read.GetResults()?;
        let count = filled.Length()? as usize;
        if count == 0 {
            // A completed read with nothing in it means the peer hung up.
            chunks.send(Err(Error::Disconnected)).ok();
            return Ok(());
        }
        let chunk = take_bytes(&filled, count)?;
        if chunks.send(Ok(chunk)).is_err() {
            return Ok(());
        }
    }
}

impl Link for WindowsRfcomm {
    fn write_frame(&mut self, frame: &Frame) -> Result<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        let bytes = frame.encode();
        self.tx_bytes += bytes.len();
        self.requests
            .send(Request::Write(bytes, ack_tx))
            .map_err(|_| Error::Disconnected)?;
        ack_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| Error::Timeout {
                cmd: frame.cmd,
                waited_ms: 5000,
            })?
    }

    fn read_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(frame) = self.framer.next_frame() {
                return Ok(Some(frame));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match self
                .incoming
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(Ok(chunk)) => {
                    self.rx_bytes += chunk.len();
                    self.framer.push(&chunk);
                }
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Err(Error::Disconnected),
            }
        }
    }

    fn describe(&self) -> String {
        format!("{} (rfcomm, winrt)", self.label)
    }

    fn diagnostics(&self) -> String {
        format!(
            "tx {} bytes, rx {} bytes, {} dropped, {} buffered",
            self.tx_bytes,
            self.rx_bytes,
            self.framer.dropped_bytes(),
            self.framer.buffered()
        )
    }
}

impl Drop for WindowsRfcomm {
    fn drop(&mut self) {
        // Ask the worker to stop, then raise the flag for the case where it is
        // parked in a read: either way it returns, drops the socket, and the
        // channel closes cleanly.
        let _ = self.requests.send(Request::Shutdown);
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device id captured from the real MOMENTUM 5 entry named "GAIA".
    const REAL_ID: &str = "Bluetooth#Bluetoothb4:0e:de:4a:9e:39-80:c3:ba:c5:bb:97\
                           #RFCOMM:00000001:{a2129ff3-081b-4c45-8afe-469d9c4842ec}";

    #[test]
    fn reads_the_peer_address_out_of_a_windows_device_id() {
        assert_eq!(
            peer_address(REAL_ID).as_deref(),
            Some("80:c3:ba:c5:bb:97"),
            "the second address is the peer, the first is our radio"
        );
        assert_eq!(channel(REAL_ID), Some(1));
    }

    #[test]
    fn tolerates_ids_without_a_resolved_channel_or_address() {
        assert_eq!(channel("Bluetooth#x#RFCOMM:00000000:{abc}"), Some(0));
        assert_eq!(channel("nonsense"), None);
        assert_eq!(peer_address("no addresses here"), None);
        assert!(
            !is_bd_addr("1234567890123456789"),
            "must be exactly 17 chars"
        );
    }

    #[test]
    fn parses_a_uuid_with_or_without_decoration() {
        let plain = parse_guid("a2129ff3-081b-4c45-8afe-469d9c4842ec").unwrap();
        let braced = parse_guid("{A2129FF3-081B-4C45-8AFE-469D9C4842EC}").unwrap();
        assert_eq!(plain, braced);
        assert_eq!(
            plain.to_u128(),
            0xa2129ff3_081b_4c45_8afe_469d9c4842ec_u128,
            "GUID must carry the canonical textual order"
        );
    }

    #[test]
    fn rejects_something_that_is_not_a_uuid() {
        for bad in ["", "hello", "a2129ff3-081b-4c45-8afe-469d9c4842"] {
            let err = parse_guid(bad).expect_err("should reject");
            assert!(
                matches!(err, Error::ServiceNotFound(_)),
                "unexpected error for {bad:?}: {err}"
            );
        }
    }
}
