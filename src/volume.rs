//! Playback volume, which is *not* part of the GAIA control channel.
//!
//! Headphone volume travels over AVRCP as "absolute volume", and Windows owns
//! that negotiation: the endpoint's master volume and the headset's own volume
//! are kept in step by the OS. So the way to set the headset's volume is to set
//! the volume of its audio endpoint — the same thing the tray slider does, and it
//! moves the headset rather than a Windows-side fader in front of a mute.
//!
//! Volume is done here because it is the one thing users expect from a headphone
//! tool that the headset's private channel does not implement. It deliberately
//! does not pretend to be part of the protocol: it lives in its own module and
//! talks to the OS, not to the headset.

use std::thread;
use std::time::Duration;

use windows::core::{GUID, PCWSTR};
use windows::Devices::Enumeration::{DeviceClass, DeviceInformation};
use windows::Media::Devices::{AudioDeviceRole, MediaDevice};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use crate::error::{Error, Result};
use crate::rfcomm::{init_com, status};

/// `CLSID_MMDeviceEnumerator`, `{BCDE0395-E52F-467C-8E3D-C4579291692E}`.
///
/// The metadata exports no constant for this COM class, so it is spelled out
/// from the SDK header. `GUID::from_values` takes the fields in their memory
/// layout, which is why the first three are not in textual order.
const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID::from_values(
    0xBCDE_0395,
    0xE52F,
    0x467C,
    [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
);

/// One playback endpoint, with the name Windows shows the user.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Friendly name, e.g. `MOMENTUM 5`.
    pub name: String,
    /// Endpoint id, e.g. `{0.0.0.00000000}.{guid}\multimedia:{...}`.
    pub id: String,
    /// Master volume, 0.0..=1.0 as the endpoint reports it.
    pub level: f32,
    pub muted: bool,
    /// The endpoint Windows plays through unless told otherwise.
    pub is_default: bool,
}

impl Endpoint {
    /// The level in the 0..=100 the rest of the CLI speaks.
    pub fn percent(&self) -> u8 {
        (f64::from(self.level.clamp(0.0, 1.0)) * 100.0).round() as u8
    }
}

/// Enumerator for audio endpoints; COM must be initialised first.
fn enumerator() -> Result<IMMDeviceEnumerator> {
    init_com()?;
    // SAFETY: COM is initialised above and this is the real CLSID.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&CLSID_MM_DEVICE_ENUMERATOR, None, CLSCTX_ALL) }
            .map_err(|e| Error::Audio(format!("cannot reach the audio endpoint API: {e}")))?;
    Ok(enumerator)
}

/// Endpoint ids are passed to this API as wide strings; the vector owns the
/// buffer, so return both.
fn as_pcwstr(id: &str) -> (Vec<u16>, PCWSTR) {
    let utf16: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    let ptr = PCWSTR::from_raw(utf16.as_ptr());
    (utf16, ptr)
}

/// Activate the volume interface of one endpoint.
fn volume_interface(enumerator: &IMMDeviceEnumerator, id: &str) -> Result<IAudioEndpointVolume> {
    let (_keep, pcw) = as_pcwstr(id);
    // SAFETY: `pcw` points into `_keep`, which outlives this call.
    let device = unsafe { enumerator.GetDevice(pcw) }?;
    // SAFETY: CLSCTX_ALL and no activation parameters is how every caller
    // activates an audio endpoint.
    let volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| Error::Audio(format!("{id} has no volume interface: {e}")))?;
    Ok(volume)
}

/// The endpoint id inside a WinRT device interface path.
///
/// WinRT names audio devices by their device interface path,
/// `\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{guid}#{e6327cad-…}`, while
/// `IMMDeviceEnumerator::GetDevice` wants the `{0.0.0.00000000}.{guid}` in the
/// middle — the same string `MediaDevice::GetDefaultAudioRenderId` returns.
fn endpoint_id(path: &str) -> Option<&str> {
    path.split_once("MMDEVAPI#")?
        .1
        .split('#')
        .next()
        .filter(|id| id.starts_with("{0.0.0.0"))
}

/// Every playback endpoint, named, with the default one flagged.
///
/// The endpoint API knows devices only by an opaque id and carries no friendly
/// name, so WinRT's audio-render device class supplies the names and the endpoint
/// id is carved out of its device interface path. Anything without a volume
/// interface is skipped rather than reported as an error.
pub fn endpoints() -> Result<Vec<Endpoint>> {
    let enumerator = enumerator()?;
    // WinRT hands back the same device interface path form as the enumeration,
    // so it goes through the same mapping. A machine with no default device is
    // not an error here; it simply means nothing gets flagged.
    let default_id = MediaDevice::GetDefaultAudioRenderId(AudioDeviceRole::Default)
        .ok()
        .and_then(|id| endpoint_id(&id.to_string()).map(str::to_string))
        .unwrap_or_default();

    let finder = DeviceInformation::FindAllAsyncDeviceClass(DeviceClass::AudioRender)
        .map_err(|e| Error::Audio(format!("cannot list audio devices: {e}")))?;
    loop {
        match finder.Status()? {
            code if code.0 == status::COMPLETED => break,
            code if code.0 == status::CANCELED || code.0 == status::ERROR => {
                return Err(Error::Audio("Windows would not list its audio devices".into()));
            }
            _ => thread::sleep(Duration::from_millis(1)),
        }
    }
    let found = finder.GetResults()?;

    let mut list = Vec::new();
    for info in found {
        let raw = info.Id()?.to_string();
        let Some(id) = endpoint_id(&raw) else {
            continue;
        };
        let Ok(volume) = volume_interface(&enumerator, id) else {
            continue; // no volume interface, so not a playback endpoint
        };
        let level = unsafe { volume.GetMasterVolumeLevelScalar() }?;
        let muted = unsafe { volume.GetMute() }?.as_bool();
        list.push(Endpoint {
            name: info.Name()?.to_string(),
            is_default: !default_id.is_empty() && id == default_id.as_str(),
            id: id.to_string(),
            level,
            muted,
        });
    }
    list.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(list)
}

/// The endpoint to act on: `filter` matches a name or id substring, and an empty
/// filter means the default playback device.
pub fn select(filter: &str) -> Result<Endpoint> {
    let list = endpoints()?;
    if list.is_empty() {
        return Err(Error::ServiceNotFound(
            "no audio endpoints could be read".into(),
        ));
    }
    if filter.is_empty() {
        return list
            .into_iter()
            .find(|endpoint| endpoint.is_default)
            .ok_or_else(|| Error::ServiceNotFound("there is no default playback device".into()));
    }
    let needle = filter.to_lowercase();
    list.into_iter()
        .find(|endpoint| {
            endpoint.name.to_lowercase().contains(&needle)
                || endpoint.id.to_lowercase().contains(&needle)
        })
        .ok_or_else(|| Error::ServiceNotFound(format!("no playback device matching \"{filter}\"")))
}

/// Set the master volume of `endpoint` to `level` (0.0..=1.0).
///
/// Returns the level the endpoint reports afterwards: read-back is the only
/// evidence that the driver accepted the value, since some drivers quantise it.
pub fn set_level(endpoint: &Endpoint, level: f32) -> Result<f32> {
    let enumerator = enumerator()?;
    let volume = volume_interface(&enumerator, &endpoint.id)?;
    // SAFETY: a null event-context GUID is the documented way to change the
    // volume without attributing the change to a particular UI.
    unsafe { volume.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null()) }?;
    Ok(unsafe { volume.GetMasterVolumeLevelScalar() }?)
}

/// Mute or unmute `endpoint`, returning the state the endpoint reports.
///
/// Muting is an endpoint property, not a volume of zero: the level survives it,
/// which is why `Endpoint::muted` is carried alongside the level rather than
/// folded into it.
pub fn set_muted(endpoint: &Endpoint, muted: bool) -> Result<bool> {
    let enumerator = enumerator()?;
    let volume = volume_interface(&enumerator, &endpoint.id)?;
    // SAFETY: as in [`set_level`], a null event-context GUID attributes the
    // change to no particular UI.
    unsafe { volume.SetMute(muted, std::ptr::null()) }?;
    Ok(unsafe { volume.GetMute() }?.as_bool())
}

#[cfg(test)]
mod tests {
    use super::endpoint_id;

    /// A device interface path read from this machine's audio devices.
    const REAL_PATH: &str = "\\\\?\\SWD#MMDEVAPI#{0.0.0.00000000}.{ec5e2eb5-2506-4bf0-\
                             a4e1-baadb440eaf4}#{e6327cad-dcec-4949-ae8a-991e976a79d2}";

    #[test]
    fn carves_the_endpoint_id_out_of_the_interface_path() {
        assert_eq!(
            endpoint_id(REAL_PATH),
            Some("{0.0.0.00000000}.{ec5e2eb5-2506-4bf0-a4e1-baadb440eaf4}"),
            "this is the form GetDevice accepts and GetDefaultAudioRenderId returns"
        );
    }

    #[test]
    fn ignores_paths_that_are_not_audio_endpoints() {
        assert_eq!(endpoint_id("\\\\?\\SWD#MTP#foo"), None);
        assert_eq!(endpoint_id("MMDEVAPI#{1.2.3}.{4}"), None);
        assert_eq!(endpoint_id(""), None);
    }
}
