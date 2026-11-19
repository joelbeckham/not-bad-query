//! Local AWS Client VPN status for connections that require the tunnel.

use crate::{DriverError, ErrorKind, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Connected,
    Disconnected,
    Unavailable,
}

pub(crate) fn require_connected(status: Status) -> Result<()> {
    let message = match status {
        Status::Connected => return Ok(()),
        Status::Disconnected => "Connect to AWS Client VPN before opening this database.",
        Status::Unavailable => "Could not verify AWS Client VPN status for this database.",
    };
    Err(DriverError::new(ErrorKind::VpnRequired, message))
}

#[cfg(windows)]
pub use windows::status;

#[cfg(not(windows))]
pub fn status() -> Status {
    Status::Unavailable
}

#[cfg(windows)]
mod windows {
    use super::Status;
    use std::{ptr, slice};
    use windows_sys::Win32::NetworkManagement::{
        IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_ROW2},
        Ndis::{
            IF_OPER_STATUS, IfOperStatusUp, MediaConnectStateConnected, MediaConnectStateUnknown,
            NET_IF_MEDIA_CONNECT_STATE,
        },
    };

    fn adapter_status(
        description: &str,
        operational: IF_OPER_STATUS,
        media: NET_IF_MEDIA_CONNECT_STATE,
    ) -> Option<Status> {
        let description = description.to_ascii_lowercase();
        if !description.starts_with("aws client vpn tap-windows adapter")
            && !description.starts_with("aws vpn client tap-windows adapter")
        {
            return None;
        }
        Some(if operational != IfOperStatusUp {
            Status::Disconnected
        } else if media == MediaConnectStateConnected {
            Status::Connected
        } else if media == MediaConnectStateUnknown {
            Status::Unavailable
        } else {
            Status::Disconnected
        })
    }

    pub fn status() -> Status {
        let mut table = ptr::null_mut();
        // IP Helper allocates a variable-length table and reports its row count.
        // Keep all borrowed rows within the lifetime of that allocation.
        if unsafe { GetIfTable2(&mut table) } != 0 || table.is_null() {
            return Status::Unavailable;
        }
        let rows = unsafe {
            slice::from_raw_parts(
                ptr::addr_of!((*table).Table).cast::<MIB_IF_ROW2>(),
                (*table).NumEntries as usize,
            )
        };
        let mut result = Status::Unavailable;
        for row in rows {
            let end = row
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(row.Description.len());
            let description = String::from_utf16_lossy(&row.Description[..end]);
            match adapter_status(&description, row.OperStatus, row.MediaConnectState) {
                Some(Status::Connected) => {
                    result = Status::Connected;
                    break;
                }
                Some(Status::Disconnected) => result = Status::Disconnected,
                _ => {}
            }
        }
        unsafe {
            FreeMibTable(table.cast());
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::NetworkManagement::Ndis::{
            IfOperStatusDown, MediaConnectStateDisconnected,
        };

        #[test]
        fn recognizes_aws_adapters_and_requires_an_active_link() {
            let current = "AWS Client VPN TAP-Windows Adapter V9";
            let legacy = "AWS VPN Client TAP-Windows Adapter V9";
            assert_eq!(
                adapter_status(current, IfOperStatusUp, MediaConnectStateConnected),
                Some(Status::Connected)
            );
            assert_eq!(
                adapter_status(legacy, IfOperStatusDown, MediaConnectStateDisconnected),
                Some(Status::Disconnected)
            );
            assert_eq!(
                adapter_status(current, IfOperStatusUp, MediaConnectStateUnknown),
                Some(Status::Unavailable)
            );
            assert_eq!(
                adapter_status(
                    "TAP-Windows Adapter V9",
                    IfOperStatusUp,
                    MediaConnectStateConnected
                ),
                None
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marked_profiles_require_a_verified_link() {
        assert!(require_connected(Status::Connected).is_ok());
        for status in [Status::Disconnected, Status::Unavailable] {
            assert_eq!(
                require_connected(status).unwrap_err().kind,
                ErrorKind::VpnRequired
            );
        }
    }
}
