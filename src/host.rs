//! Hosting the Sennheiser control service **on this machine**.
//!
//! Smart Control dials the headset by service UUID, so a PC that answers that UUID
//! becomes the device as far as the app is concerned — the one remaining way to see
//! what the app *sends*, rather than only what the headset remembers.
//!
//! Two stages, because Windows answers a more basic question first. [`probe`] asks
//! whether this machine may hold the service at all and advertises nothing; [`serve`]
//! binds it and waits for a client. Splitting them matters: the first is harmless and
//! can be run next to a connected headset, and if Windows refuses there, the rest of
//! the idea is moot.

use crate::error::{Error, Result};
use crate::proto::frame::FrameKind;
use crate::{Frame, GAIA_SERVICE_UUID};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows::Devices::Bluetooth::Rfcomm::{RfcommServiceId, RfcommServiceProvider};
use windows::Foundation::TypedEventHandler;
use windows::Networking::Sockets::{
    SocketProtectionLevel, StreamSocket, StreamSocketListener,
    StreamSocketListenerConnectionReceivedEventArgs,
};
use windows::Storage::Streams::{DataReader, DataWriter};
use windows::core::Ref;
use windows_future::{AsyncStatus, IAsyncAction, IAsyncOperation};

/// Refusal or failure, with the Windows reason attached so the answer is actionable.
fn refused(what: &str, cause: impl std::fmt::Display) -> Error {
    Error::ServiceNotFound(format!("{what}: {cause}"))
}

/// A WinRT async call that can be asked how it is getting on.
trait Pollable {
    fn status(&self) -> windows::core::Result<AsyncStatus>;
}

impl Pollable for IAsyncAction {
    fn status(&self) -> windows::core::Result<AsyncStatus> {
        self.Status()
    }
}

impl<T: windows::core::RuntimeType + 'static> Pollable for IAsyncOperation<T> {
    fn status(&self) -> windows::core::Result<AsyncStatus> {
        self.Status()
    }
}

/// Wait for a WinRT call to finish without dragging an async runtime into a tool that
/// has no other use for one. These calls are local and finish in milliseconds; the
/// poll interval is a politeness, not a tuning decision.
fn settle(operation: &impl Pollable, what: &str) -> Result<()> {
    loop {
        let status = operation
            .status()
            .map_err(|e| refused(&format!("{what}: Status"), e))?;
        if status != AsyncStatus::Started {
            return if status == AsyncStatus::Completed {
                Ok(())
            } else {
                Err(refused(what, format!("the call ended {status:?}")))
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Ask Windows for a provider for `uuid`, and report what it says.
///
/// `CreateAsync` reserves the SDP record — historically the privileged step on
/// Windows — and advertises nothing: no `StartAdvertising` call is made, so the
/// machine does not become discoverable and a headset switched on afterwards will not
/// see us.
pub fn probe(uuid: &str) -> Result<String> {
    let id = service_id(uuid)?;
    let operation = RfcommServiceProvider::CreateAsync(&id)
        .map_err(|e| refused("CreateAsync — can this PC host an RFCOMM service", e))?;
    settle(&operation, "CreateAsync")?;
    let provider = operation.GetResults().map_err(|e| refused("GetResults", e))?;
    let back = provider
        .ServiceId()
        .and_then(|id| id.Uuid())
        .map_err(|e| refused("ServiceId", e))?;
    let attributes = provider
        .SdpRawAttributes()
        .and_then(|map| map.Size())
        .unwrap_or(0);

    Ok(format!(
        "the OS reserved the service {back:?}. its SDP record has {attributes} \
         attribute(s) we would own"
    ))
}

/// What [`serve`] should do.
pub struct Serve<'a> {
    /// The name to publish in SDP, so the phone lists us as something a human can
    /// tap. The paired device's own name is the computer's, which we do not control.
    pub name: &'a str,
    /// Replies to give instead of an empty ack, as `(command, payload)`. The app will
    /// ask for things and wait for data; whatever we have actually captured goes here,
    /// and everything else is acknowledged empty so the log keeps flowing.
    pub replies: &'a [(u16, Vec<u8>)],
}

/// Bind the control service, advertise it, and log whatever a client sends.
///
/// Every frame is printed as it arrives, which is the entire point: it is the first
/// time this project gets to see a controller's side of the conversation. The replies
/// are deliberately dull — an ack, or a captured payload when one was given — because
/// inventing a headset's answers would only teach us what our own invention looks
/// like.
///
/// This makes the machine discoverable. It is reversible by stopping the command,
/// which drops the provider and stops advertising.
pub fn serve(options: &Serve) -> Result<()> {
    let id = service_id(GAIA_SERVICE_UUID)?;
    let provider = {
        let operation = RfcommServiceProvider::CreateAsync(&id)
            .map_err(|e| refused("CreateAsync", e))?;
        settle(&operation, "CreateAsync")?;
        operation.GetResults().map_err(|e| refused("GetResults", e))?
    };

    let listener = StreamSocketListener::new().map_err(|e| refused("listener", e))?;
    let (tx, rx) = mpsc::channel();
    // Named out loud because the delegate's two type parameters are not inferable
    // from `new` alone, and an annotation is kinder than a type-error trail.
    let handler: TypedEventHandler<
        StreamSocketListener,
        StreamSocketListenerConnectionReceivedEventArgs,
    > = TypedEventHandler::new(
        move |_: Ref<StreamSocketListener>,
              args: Ref<StreamSocketListenerConnectionReceivedEventArgs>| {
            // `Ref` is the WinRT borrowed-argument wrapper: it can be null, so it
            // hands out an Option rather than pretending every callback has arguments.
            if let Some(args) = args.as_ref()
                && let Ok(socket) = args.Socket()
            {
                let _ = tx.send(socket);
            }
            Ok(())
        },
    );
    listener
        .ConnectionReceived(&handler)
        .map_err(|e| refused("ConnectionReceived", e))?;

    let service_name = id.AsString().map_err(|e| refused("AsString", e))?;
    let bind = listener
        .BindServiceNameWithProtectionLevelAsync(
            &service_name,
            SocketProtectionLevel::BluetoothEncryptionAllowNullAuthentication,
        )
        .map_err(|e| refused("BindServiceNameAsync", e))?;
    settle(&bind, "BindServiceNameAsync")?;

    provider
        .StartAdvertisingWithRadioDiscoverability(&listener, true)
        .map_err(|e| refused("StartAdvertising", e))?;

    println!(
        "advertising \"{}\" as service {service_name}. the radio is discoverable, so \
         pair from the phone now. waiting for one connection.",
        options.name
    );
    let socket = rx
        .recv_timeout(Duration::from_secs(600))
        .map_err(|_| refused("waiting", "no client connected within ten minutes"))?;
    println!(
        "a client connected: {}",
        socket
            .Information()
            .and_then(|info| info.RemoteHostName())
            .map(|name| name.ToString().unwrap_or_default().to_string())
            .unwrap_or_else(|_| "unknown address".to_string())
    );

    talk(&socket, options)?;
    provider.StopAdvertising().ok();
    listener.Close().ok();
    Ok(())
}

/// Read frames from one client and answer them, until the client goes away.
fn talk(socket: &StreamSocket, options: &Serve) -> Result<()> {
    let input = socket.InputStream().map_err(|e| refused("InputStream", e))?;
    let output = socket.OutputStream().map_err(|e| refused("OutputStream", e))?;
    let reader = DataReader::CreateDataReader(&input).map_err(|e| refused("DataReader", e))?;
    let writer = DataWriter::CreateDataWriter(&output).map_err(|e| refused("DataWriter", e))?;
    // Without this, LoadAsync waits for a whole buffer and a six-byte frame would
    // sit unread until something filled the rest.
    reader
        .SetInputStreamOptions(windows::Storage::Streams::InputStreamOptions::Partial)
        .map_err(|e| refused("InputStreamOptions", e))?;

    let mut framer = crate::proto::frame::Framer::new();
    let started = Instant::now();
    loop {
        let load = reader.LoadAsync(4096).map_err(|e| refused("LoadAsync", e))?;
        settle(&load, "LoadAsync")?;
        let read = load.GetResults().map_err(|e| refused("LoadAsync", e))?;
        if read == 0 {
            println!("the client closed the control channel.");
            return Ok(());
        }
        let available = reader
            .UnconsumedBufferLength()
            .map_err(|e| refused("UnconsumedBufferLength", e))? as usize;
        let mut chunk = vec![0u8; available];
        reader
            .ReadBytes(&mut chunk)
            .map_err(|e| refused("ReadBytes", e))?;
        framer.push(&chunk);

        while let Some(frame) = framer.next_frame() {
            let known = crate::cmd::name(frame.cmd);
            println!(
                "  t+{:6.1}s  >> 0x{:04x} {:<22} {:02x?}",
                started.elapsed().as_secs_f32(),
                frame.cmd,
                if known == "unknown" {
                    format!("0x{:04x}", frame.cmd)
                } else {
                    known.to_string()
                },
                frame.payload,
            );
            if frame.kind != FrameKind::Command {
                continue;
            }
            let payload = options
                .replies
                .iter()
                .find(|(asked, _)| *asked == frame.base_cmd())
                .map(|(_, payload)| payload.clone())
                .unwrap_or_default();
            let reply = Frame::command(frame.cmd | 0x0100, payload);
            let bytes = reply.encode();
            writer
                .WriteBytes(&bytes)
                .map_err(|e| refused("WriteBytes", e))?;
            let stored = writer.StoreAsync().map_err(|e| refused("StoreAsync", e))?;
            settle(&stored, "StoreAsync")?;
            stored.GetResults().map_err(|e| refused("StoreAsync", e))?;
            let flushed = writer.FlushAsync().map_err(|e| refused("FlushAsync", e))?;
            settle(&flushed, "FlushAsync")?;
            flushed.GetResults().map_err(|e| refused("FlushAsync", e))?;
        }
    }
}

/// The Sennheiser control service, as Windows names it.
fn service_id(uuid: &str) -> Result<RfcommServiceId> {
    let guid = crate::rfcomm::parse_guid(uuid).map_err(|e| refused("bad uuid", e))?;
    RfcommServiceId::FromUuid(guid).map_err(|e| refused("FromUuid", e))
}
