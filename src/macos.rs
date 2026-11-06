//! macOS transport: the headset's private RFCOMM service, via IOBluetooth.
//!
//! IOBluetooth is the supported way to reach a proprietary serial profile on
//! macOS: the paired device's cached SDP records say which RFCOMM server channel
//! carries a service UUID, and `openRFCOMMChannelSync` opens it. Pairing stays in
//! System Settings.
//!
//! IOBluetooth delivers its delegate callbacks on the run loop of the thread
//! that opened the channel, and its objects are not `Send`. Opening an RFCOMM
//! channel also has to happen on the process main thread: from a spawned thread
//! `openRFCOMMChannelSync` fails with `kIOReturnError` (0xe00002bc) on every
//! attempt, while the same calls on the main thread succeed. So the CLI runs its
//! command on a worker thread inside [`run_with_main_loop`], and the main thread
//! spins `CFRunLoopRunInMode` in short slices, running the jobs the worker posts
//! to it. Every call into IOBluetooth happens in such a job, and the public side
//! talks to it over channels so [`Link::read_frame`] can honour a deadline — the
//! same shape as the Windows transport in `rfcomm.rs`.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::NSString;
use objc2_io_bluetooth::{
    IOBluetoothDevice, IOBluetoothRFCOMMChannel, IOBluetoothRFCOMMChannelDelegate,
    IOBluetoothSDPServiceRecord, IOBluetoothSDPUUID,
};

use crate::error::{Error, Result};
use crate::link::Link;
use crate::proto::frame::{Frame, Framer};

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopRunInMode(
        mode: *const c_void,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;
}

/// Let IOBluetooth deliver whatever callbacks are due, for at most `seconds`.
fn spin(seconds: f64) {
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 0);
    }
}

/// How long one run-loop slice lasts. Bounds how long a queued write or job
/// waits for the main thread to notice it.
const SLICE: f64 = 0.01;

type Job = Box<dyn FnOnce() + Send>;

/// Where worker threads post jobs for the main thread; set by
/// [`run_with_main_loop`].
static MAIN_JOBS: OnceLock<Sender<Job>> = OnceLock::new();

/// Run `work` on a new thread while the calling thread, which must be the
/// process main thread, spins its run loop and runs the jobs `work` posts.
/// Returns what `work` returns.
pub fn run_with_main_loop<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    let (jobs_tx, jobs) = mpsc::channel::<Job>();
    let _ = MAIN_JOBS.set(jobs_tx);
    thread::scope(|scope| {
        let handle = scope.spawn(work);
        while !handle.is_finished() {
            while let Ok(job) = jobs.try_recv() {
                job();
            }
            spin(SLICE);
        }
        handle
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// Queue `job` for the main thread.
fn post(job: impl FnOnce() + Send + 'static) -> Result<()> {
    MAIN_JOBS
        .get()
        .ok_or_else(|| Error::RadioUnavailable("the main-thread run loop is not running".into()))?
        .send(Box::new(job))
        .map_err(|_| Error::RadioUnavailable("the main-thread run loop has stopped".into()))
}

/// How long a device's SDP query may take before the device is given up on.
const SDP_WAIT: Duration = Duration::from_secs(5);

/// A GAIA control channel found on a paired device.
#[derive(Debug, Clone)]
pub struct RfcommTarget {
    /// The device name System Settings shows, e.g. "Khai moment".
    pub name: String,
    /// `<address>#RFCOMM:<channel as 8 hex digits>`, the shape [`peer_address`]
    /// and [`channel`] read back.
    pub device_id: String,
    /// Service class UUID, the one the SDP record was matched on.
    pub service_name: String,
}

/// Parse a UUID into its 16 bytes: 8-4-4-4-12 hex digits, braces optional.
fn uuid_bytes(text: &str) -> Result<[u8; 16]> {
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
        .map(u128::to_be_bytes)
        .map_err(|e| Error::ServiceNotFound(format!("\"{text}\" is not a UUID: {e}")))
}

/// An address with the separators and case taken out, so `80:C3:BA`,
/// `80-c3-ba` and `80c3ba` compare equal.
fn bare_address(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, ':' | '-'))
        .collect::<String>()
        .to_lowercase()
}

/// True when `needle` appears in the device name or its address.
fn matches_needle(needle: &str, name: &str, address: &str) -> bool {
    let bare = bare_address(needle);
    name.to_lowercase().contains(&needle.to_lowercase())
        || (!bare.is_empty() && bare_address(address).contains(&bare))
}

/// The peer's BD_ADDR, as lower-case `xx:xx:xx:xx:xx:xx`, out of a device id.
pub fn peer_address(device_id: &str) -> Option<String> {
    device_id
        .split_once('#')
        .map(|(address, _)| address.to_owned())
}

/// The RFCOMM server channel the SDP record names.
pub fn channel(device_id: &str) -> Option<u32> {
    let rest = device_id.split_once("RFCOMM:")?.1;
    u32::from_str_radix(rest.get(..8)?, 16).ok()
}

/// The RFCOMM channel `device`'s SDP record for `uuid` names, if it has one.
fn service_channel(device: &IOBluetoothDevice, uuid: &IOBluetoothSDPUUID) -> Option<u32> {
    let record: Retained<IOBluetoothSDPServiceRecord> =
        unsafe { device.getServiceRecordForUUID(Some(uuid)) }?;
    let mut id: u8 = 0;
    (unsafe { record.getRFCOMMChannelID(&mut id) } == 0).then_some(u32::from(id))
}

/// Enumerate paired devices publishing `uuid` as an RFCOMM service. IOBluetooth
/// lists some devices twice, so each address and channel is reported once.
///
/// `needle` matches the device name or the address. A connected device whose
/// services have never been read is asked over SDP and waited for; a device that
/// is not connected is skipped rather than woken up to be asked.
pub fn find(uuid: &str, needle: Option<&str>) -> Result<Vec<RfcommTarget>> {
    let bytes = uuid_bytes(uuid)?;
    let uuid = uuid.to_owned();
    let needle = needle.map(str::to_owned);
    let (done_tx, done) = mpsc::channel();
    post(move || {
        let _ = done_tx.send(find_on_main(&bytes, &uuid, needle.as_deref()));
    })?;
    done.recv()
        .map_err(|_| Error::RadioUnavailable("the IOBluetooth job panicked".into()))?
}

fn find_on_main(bytes: &[u8; 16], uuid: &str, needle: Option<&str>) -> Result<Vec<RfcommTarget>> {
    let sdp_uuid =
        unsafe { IOBluetoothSDPUUID::uuidWithBytes_length(bytes.as_ptr().cast(), 16) }
            .ok_or_else(|| Error::ServiceNotFound(format!("\"{uuid}\" is not an SDP UUID")))?;
    let paired = unsafe { IOBluetoothDevice::pairedDevices() }.ok_or_else(|| {
        Error::RadioUnavailable(
            "no paired devices were reported: pair the headset in System Settings, and \
             check that this terminal may use Bluetooth in Privacy & Security"
                .into(),
        )
    })?;

    let mut targets = Vec::new();
    for object in paired.iter() {
        let Ok(device) = object.downcast::<IOBluetoothDevice>() else {
            continue;
        };
        let name = unsafe { device.name() }.to_string();
        let address = unsafe { device.addressString() }
            .map(|a| a.to_string().replace('-', ":").to_lowercase())
            .unwrap_or_default();
        if let Some(needle) = needle
            && !matches_needle(needle, &name, &address)
        {
            continue;
        }
        if unsafe { device.services() }.is_none() && unsafe { device.isConnected() } {
            unsafe { device.performSDPQuery(None) };
            let deadline = Instant::now() + SDP_WAIT;
            while unsafe { device.services() }.is_none() && Instant::now() < deadline {
                spin(SLICE);
            }
        }
        if let Some(server_channel) = service_channel(&device, &sdp_uuid) {
            let device_id = format!("{address}#RFCOMM:{server_channel:08x}");
            if targets
                .iter()
                .any(|t: &RfcommTarget| t.device_id == device_id)
            {
                continue;
            }
            targets.push(RfcommTarget {
                name,
                device_id,
                service_name: uuid.to_owned(),
            });
        }
    }
    Ok(targets)
}

struct DelegateIvars {
    chunks: Sender<Result<Vec<u8>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `Delegate` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "MomentumRfcommDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl IOBluetoothRFCOMMChannelDelegate for Delegate {
        #[unsafe(method(rfcommChannelData:data:length:))]
        unsafe fn rfcomm_channel_data(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
            data: *mut c_void,
            length: usize,
        ) {
            let chunk = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) }.to_vec();
            let _ = self.ivars().chunks.send(Ok(chunk));
        }

        #[unsafe(method(rfcommChannelClosed:))]
        unsafe fn rfcomm_channel_closed(&self, _channel: Option<&IOBluetoothRFCOMMChannel>) {
            let _ = self.ivars().chunks.send(Err(Error::Disconnected));
        }
    }
);

impl Delegate {
    fn new(chunks: Sender<Result<Vec<u8>>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { chunks });
        unsafe { msg_send![super(this), init] }
    }
}

/// Requests the main thread handles on our behalf.
enum Request {
    /// Write these bytes, then reply with the outcome.
    Write(Vec<u8>, Sender<Result<()>>),
    /// Close the channel and finish.
    Shutdown,
}

/// Progress from the main thread while it brings the channel up.
enum Stage {
    /// A step the caller can print, so a stall is attributable.
    Progress(String),
    /// Terminal: the channel is open, or it failed.
    Done(Result<()>),
}

/// Connected GAIA control channel. Holds channels only, no IOBluetooth handles.
pub struct MacRfcomm {
    requests: Sender<Request>,
    incoming: Receiver<Result<Vec<u8>>>,
    finished: Receiver<()>,
    framer: Framer,
    label: String,
    peer: String,
    rx_bytes: usize,
    tx_bytes: usize,
}

/// Connect to `target`.
///
/// The RFCOMM connect runs on the main thread, which [`run_with_main_loop`] must
/// be spinning, so this returns only once the channel is really open.
pub fn connect(target: &RfcommTarget) -> Result<MacRfcomm> {
    let address = peer_address(&target.device_id)
        .ok_or_else(|| Error::Connect(format!("no address in \"{}\"", target.device_id)))?;
    let server_channel = channel(&target.device_id)
        .and_then(|c| u8::try_from(c).ok())
        .ok_or_else(|| Error::Connect(format!("no channel in \"{}\"", target.device_id)))?;

    let (request_tx, request_rx) = mpsc::channel::<Request>();
    let (chunk_tx, incoming) = mpsc::channel::<Result<Vec<u8>>>();
    let (stage_tx, stage_rx) = mpsc::channel::<Stage>();

    let (finished_tx, finished) = mpsc::channel::<()>();
    let job_address = address.clone();
    post(move || {
        if let Err(err) = serve(
            &job_address,
            server_channel,
            request_rx,
            chunk_tx,
            &stage_tx,
        ) {
            let _ = stage_tx.send(Stage::Done(Err(err)));
        }
        let _ = finished_tx.send(());
    })?;

    loop {
        match stage_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Stage::Progress(message)) => println!("  {message}"),
            Ok(Stage::Done(Ok(()))) => break,
            Ok(Stage::Done(Err(err))) => return Err(err),
            Err(RecvTimeoutError::Timeout) => {
                return Err(Error::Connect(
                    "the Bluetooth stack did not answer within 20 s".into(),
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Error::Connect(
                    "the Bluetooth main-thread job stopped".into(),
                ));
            }
        }
    }

    Ok(MacRfcomm {
        requests: request_tx,
        incoming,
        finished,
        framer: Framer::new(),
        label: target.name.clone(),
        peer: format!("{address} channel {server_channel}"),
        rx_bytes: 0,
        tx_bytes: 0,
    })
}

/// Open the channel and pump it until shutdown. Runs on the main thread.
fn serve(
    address: &str,
    server_channel: u8,
    requests: Receiver<Request>,
    chunks: Sender<Result<Vec<u8>>>,
    progress: &Sender<Stage>,
) -> Result<()> {
    let _ = progress.send(Stage::Progress("opening RFCOMM".into()));
    let device =
        unsafe { IOBluetoothDevice::deviceWithAddressString(Some(&NSString::from_str(address))) }
            .ok_or_else(|| Error::Connect(format!("{address} is not a known device")))?;
    let delegate = Delegate::new(chunks.clone());
    let mut opened: Option<Retained<IOBluetoothRFCOMMChannel>> = None;
    // A MOMENTUM 5 refuses the first RFCOMM open after the link has been idle
    // (kIOReturnError) and accepts the next one, on every channel it publishes.
    // Observed live on macOS; one retry is enough.
    let mut status = 0;
    for _ in 0..2 {
        status = unsafe {
            device.openRFCOMMChannelSync_withChannelID_delegate(
                Some(&mut opened),
                server_channel,
                Some(AsRef::<AnyObject>::as_ref(&*delegate)),
            )
        };
        if status == 0 && opened.is_some() {
            break;
        }
        let _ = progress.send(Stage::Progress(format!(
            "open refused ({:#010x}), retrying",
            status as u32
        )));
        spin(1.0);
    }
    let channel = match opened {
        Some(channel) if status == 0 => channel,
        _ => {
            return Err(Error::Connect(format!(
                "openRFCOMMChannel failed with IOReturn {:#010x}; the headset may be off, \
                 out of range, or another host or app holds the channel",
                status as u32
            )));
        }
    };
    let _ = progress.send(Stage::Progress("channel open".into()));
    let _ = progress.send(Stage::Done(Ok(())));

    loop {
        spin(SLICE);
        loop {
            match requests.try_recv() {
                Err(mpsc::TryRecvError::Empty) => break,
                Ok(Request::Write(mut bytes, ack)) => {
                    let outcome = match u16::try_from(bytes.len()) {
                        Ok(length)
                            if unsafe {
                                channel.writeSync_length(bytes.as_mut_ptr().cast(), length)
                            } == 0 =>
                        {
                            Ok(())
                        }
                        _ => Err(Error::Io(std::io::Error::other("RFCOMM write failed"))),
                    };
                    let failed = outcome.is_err();
                    ack.send(outcome).ok();
                    if failed {
                        break;
                    }
                }
                Ok(Request::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => {
                    unsafe { channel.closeChannel() };
                    spin(0.1);
                    return Ok(());
                }
            }
        }
        if !unsafe { channel.isOpen() } {
            let _ = chunks.send(Err(Error::Disconnected));
            return Ok(());
        }
    }
}

impl Link for MacRfcomm {
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
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = self.framer.next_frame() {
                return Ok(Some(frame));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
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
        format!("{} ({}, rfcomm, iobluetooth)", self.label, self.peer)
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

impl Drop for MacRfcomm {
    fn drop(&mut self) {
        // Unlike the Windows worker this one is waited for: a channel left open
        // when the process exits is a channel the headset keeps reserved.
        let _ = self.requests.send(Request::Shutdown);
        let _ = self.finished.recv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_uuid_with_or_without_decoration() {
        let plain = uuid_bytes("a2129ff3-081b-4c45-8afe-469d9c4842ec").unwrap();
        let braced = uuid_bytes("{A2129FF3-081B-4C45-8AFE-469D9C4842EC}").unwrap();
        assert_eq!(plain, braced);
        assert_eq!(plain[0], 0xa2, "bytes must be in canonical textual order");
        assert_eq!(plain[15], 0xec);
        assert!(matches!(
            uuid_bytes("hello"),
            Err(Error::ServiceNotFound(_))
        ));
    }

    #[test]
    fn filter_matches_name_or_address_ignoring_separators() {
        let address = "80:c3:ba:b7:2e:f6";
        assert!(matches_needle("80:C3:BA", "Khai moment", address));
        assert!(matches_needle("80-c3-ba-b7", "Khai moment", address));
        assert!(matches_needle("KHAI", "Khai moment", address));
        assert!(!matches_needle("jbl", "Khai moment", address));
        assert!(!matches_needle(":", "Khai moment", address));
    }

    #[test]
    fn device_id_round_trips() {
        let id = "80:c3:ba:b7:2e:f6#RFCOMM:00000002";
        assert_eq!(peer_address(id).as_deref(), Some("80:c3:ba:b7:2e:f6"));
        assert_eq!(channel(id), Some(2));
        assert_eq!(channel("nonsense"), None);
        assert_eq!(peer_address("nonsense"), None);
    }
}
