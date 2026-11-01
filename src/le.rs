//! What Windows believes about Bluetooth LE bonds.
//!
//! Pairing over classic Bluetooth and pairing over LE are two separate facts about
//! one physical device. The classic bond is with the BR/EDR address — the one
//! `rf-scan` prints and the one the RFCOMM control seat is built on. LE has its own
//! identity address and its own keys, and what a device actually advertises is a
//! **resolvable private address**: it changes on a timer, and only a host holding
//! the peer's identity resolving key — that is, a host it has paired with *over LE*
//! — can trace it back to a device.
//!
//! So "the PC is paired with the headset" answers the classic question and is
//! silent about this one, which is why the answer is a command rather than a
//! Settings screen.

use std::time::Duration;

use windows::Devices::Bluetooth::BluetoothLEDevice;
use windows::Devices::Enumeration::DeviceInformation;

use crate::error::{Error, Result};
use crate::rfcomm::{init_com, status};

/// List the LE peers Windows knows about, split by whether it holds a bond with
/// them. `needle` filters on the name Windows has; empty matches everything.
pub fn bonds(needle: &str) -> Result<()> {
    init_com()?;
    let needle = needle.to_lowercase();
    for paired in [true, false] {
        println!(
            "\n{}:",
            if paired {
                "Windows lists as paired (its pairing state for the peer — a classic bond counts)"
            } else {
                "Windows lists as not paired"
            }
        );
        let selector =
            BluetoothLEDevice::GetDeviceSelectorFromPairingState(paired).map_err(|e| {
                Error::RadioUnavailable(format!("GetDeviceSelectorFromPairingState: {e}"))
            })?;
        let finder = DeviceInformation::FindAllAsyncAqsFilter(&selector)
            .map_err(|e| Error::RadioUnavailable(format!("FindAllAsyncAqsFilter: {e}")))?;
        // The `await_sync!` macro in the RFCOMM module does this and is private to
        // it; the polling is four lines and the wording of the failures is the only
        // reason not to reach for it.
        loop {
            let code = finder.Status()?.0;
            if code == status::COMPLETED {
                break;
            }
            if code == status::CANCELED {
                return Err(Error::RadioUnavailable("enumeration cancelled".into()));
            }
            if code == status::ERROR {
                return Err(Error::RadioUnavailable(
                    "the radio refused the enumeration".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let found = finder
            .GetResults()
            .map_err(|e| Error::RadioUnavailable(format!("GetResults: {e}")))?;

        let mut shown = 0;
        for info in found {
            // Windows lists a peer and every service inside it as separate entries;
            // only the peer carries a bond. A service entry names its service UUID
            // in braces inside the id, a peer does not.
            let id = info.Id().map(|i| i.to_string()).unwrap_or_default();
            if id.contains('{') {
                continue;
            }
            let name = info.Name().map(|n| n.to_string()).unwrap_or_default();
            if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                continue;
            }
            // Deliberately no per-entry `paired=` column. `DeviceInformation.Pairing
            // .IsPaired` reports the pairing state of the *device container*, and it
            // answers false here for devices the selector has just put in the paired
            // group — the two APIs disagree, and only the selector is about the LE
            // peer. Its flag is Windows' pairing state for the peer, which a classic
            // bond also satisfies; that is stated at the heading, not faked per row.
            println!(
                "  {:<28}",
                if name.is_empty() { "(unnamed)" } else { &name }
            );
            shown += 1;
        }
        if shown == 0 {
            println!("  (nothing)");
        }
    }
    Ok(())
}
