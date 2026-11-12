use std::io;

use anyhow::{Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Power::{
    GetSystemPowerStatus, PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired,
    PowerSetRequest, SYSTEM_POWER_STATUS,
};
use windows_sys::Win32::System::SystemServices::POWER_REQUEST_CONTEXT_VERSION;
use windows_sys::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};

use super::{LockKind, PowerSource};

pub struct SleepLock {
    request: HANDLE,
}

// SAFETY: a power request belongs to the process, not the thread that created it, so any thread
// may clear and close it.
unsafe impl Send for SleepLock {}

impl SleepLock {
    pub fn acquire(reason: &str) -> Result<SleepLock> {
        let mut wide: Vec<u16> = reason.encode_utf16().chain([0]).collect();
        let context = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: wide.as_mut_ptr(),
            },
        };
        // SAFETY: `context` and the string it points to outlive the call, which copies the string.
        let request = unsafe { PowerCreateRequest(&context) };
        if request == INVALID_HANDLE_VALUE {
            bail!("PowerCreateRequest failed: {}", io::Error::last_os_error());
        }
        // SAFETY: `request` is the valid handle just created, and is closed on failure.
        unsafe {
            if PowerSetRequest(request, PowerRequestSystemRequired) == 0 {
                let error = io::Error::last_os_error();
                CloseHandle(request);
                bail!("PowerSetRequest failed: {error}");
            }
        }
        Ok(SleepLock { request })
    }

    pub fn kind(&self) -> LockKind {
        LockKind::Windows
    }

    pub fn refresh(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Drop for SleepLock {
    fn drop(&mut self) {
        // SAFETY: `request` is a handle this lock owns and set; nothing uses it after this.
        unsafe {
            PowerClearRequest(self.request, PowerRequestSystemRequired);
            CloseHandle(self.request);
        }
    }
}

fn system_power_status() -> Option<SYSTEM_POWER_STATUS> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: the out pointer is to a local.
    (unsafe { GetSystemPowerStatus(&mut status) } != 0).then_some(status)
}

pub fn power_source() -> PowerSource {
    match system_power_status().map(|status| status.ACLineStatus) {
        Some(0) => PowerSource::Battery,
        Some(1) => PowerSource::Ac,
        _ => PowerSource::Unknown,
    }
}

/// `BatteryFlag` is 128 for no system battery and 255 when Windows can't tell.
pub fn has_battery() -> bool {
    system_power_status().is_some_and(|status| !matches!(status.BatteryFlag, 128 | 255))
}
