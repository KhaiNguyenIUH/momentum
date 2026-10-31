//! Hosting the Sennheiser control service **on this machine**.
//!
//! Smart Control dials the headset by service UUID, so a PC that answers that UUID
//! becomes the device as far as the app is concerned — the one remaining way to see
//! what the app *sends*, rather than only what the headset remembers.
//!
//! Nothing here reaches a phone yet. Windows answers a more basic question first:
//! does it allow this machine to host an RFCOMM service at all. If it refuses, the
//! rest of the idea is moot, and it is worth learning that in one command.

use crate::error::{Error, Result};
use windows::Devices::Bluetooth::Rfcomm::{RfcommServiceId, RfcommServiceProvider};
use windows_future::AsyncStatus;

/// Refusal or failure, with the Windows reason attached so the answer is actionable.
fn refused(what: &str, cause: impl std::fmt::Display) -> Error {
    Error::ServiceNotFound(format!("{what}: {cause}"))
}

/// Ask Windows for a provider for `uuid`, and report what it says.
///
/// `CreateAsync` is the whole test. It reserves the SDP record — the step that has
/// historically been the privileged one on Windows — and it advertises nothing: no
/// `StartAdvertising` call is made, so the machine does not become discoverable and
/// a headset that is switched on afterwards will not see us.
pub fn probe(uuid: &str) -> Result<String> {
    let guid = crate::rfcomm::parse_guid(uuid).map_err(|e| refused("bad uuid", e))?;
    let id = RfcommServiceId::FromUuid(guid).map_err(|e| refused("FromUuid", e))?;
    let operation =
        RfcommServiceProvider::CreateAsync(&id).map_err(|e| refused("CreateAsync", e))?;

    // No await machinery on purpose. The call is local, so the wait is a poll, and a
    // poll keeps this command free of a runtime it would otherwise need to start.
    let status = loop {
        let status = operation.Status().map_err(|e| refused("Status", e))?;
        if status != AsyncStatus::Started {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    if status != AsyncStatus::Completed {
        let code = operation
            .ErrorCode()
            .map(|code| format!("{code:?}"))
            .unwrap_or_else(|e| e.to_string());
        return Err(refused("the provider did not complete", format!("{status:?} ({code})")));
    }

    let provider = operation.GetResults().map_err(|e| refused("GetResults", e))?;
    let service_id = provider.ServiceId().map_err(|e| refused("ServiceId", e))?;
    let back = service_id.Uuid().map_err(|e| refused("Uuid", e))?;
    let attributes = provider
        .SdpRawAttributes()
        .and_then(|map| map.Size())
        .unwrap_or(0);

    Ok(format!(
        "the OS reserved the service {back:?}. its SDP record has {attributes} \
         attribute(s) we would own"
    ))
}
