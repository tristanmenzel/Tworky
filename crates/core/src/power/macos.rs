use std::ptr::NonNull;

use anyhow::{Result, bail};
use objc2_core_foundation::{CFArray, CFDictionary, CFRetained, CFString, CFType};

use super::{LockKind, PowerSource};

type IOPMAssertionID = u32;
type IOReturn = i32;

const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

// Ownership follows the Create/Copy rule: `Copy` functions return a +1 reference the caller
// releases (`CFRetained::from_raw`); `Get` functions return a reference owned by their argument,
// which must be retained (`CFRetained::retain`) and never released as-is.
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: &CFString,
        level: u32,
        name: &CFString,
        id: *mut IOPMAssertionID,
    ) -> IOReturn;
    fn IOPMAssertionRelease(id: IOPMAssertionID) -> IOReturn;
    fn IOPSCopyPowerSourcesInfo() -> Option<NonNull<CFType>>;
    fn IOPSCopyPowerSourcesList(blob: &CFType) -> Option<NonNull<CFArray<CFType>>>;
    fn IOPSGetPowerSourceDescription(
        blob: &CFType,
        source: &CFType,
    ) -> Option<NonNull<CFDictionary<CFString, CFType>>>;
    fn IOPSGetProvidingPowerSourceType(snapshot: &CFType) -> Option<NonNull<CFString>>;
}

pub struct SleepLock {
    id: IOPMAssertionID,
}

impl SleepLock {
    pub fn acquire(reason: &str) -> Result<SleepLock> {
        let assertion_type = CFString::from_static_str("PreventUserIdleSystemSleep");
        let name = CFString::from_str(reason);
        let mut id = 0;
        // SAFETY: both strings outlive the call, and the out pointer is to a local.
        let status = unsafe {
            IOPMAssertionCreateWithName(&assertion_type, K_IOPM_ASSERTION_LEVEL_ON, &name, &mut id)
        };
        if status != 0 {
            bail!("IOPMAssertionCreateWithName failed: {status:#x}");
        }
        Ok(SleepLock { id })
    }

    pub fn kind(&self) -> LockKind {
        LockKind::MacOs
    }

    pub fn refresh(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Drop for SleepLock {
    fn drop(&mut self) {
        // SAFETY: `id` is an assertion this lock created and releases only here.
        unsafe { IOPMAssertionRelease(self.id) };
    }
}

fn power_sources_info() -> Option<CFRetained<CFType>> {
    // SAFETY: a Copy function, so the caller owns the returned reference.
    unsafe { IOPSCopyPowerSourcesInfo().map(|info| CFRetained::from_raw(info)) }
}

pub fn power_source() -> PowerSource {
    let Some(info) = power_sources_info() else {
        return PowerSource::Unknown;
    };
    // SAFETY: a Get function on a live snapshot; the result is retained before use.
    let Some(providing) =
        (unsafe { IOPSGetProvidingPowerSourceType(&info).map(|ty| CFRetained::retain(ty)) })
    else {
        return PowerSource::Unknown;
    };
    match providing.to_string().as_str() {
        "AC Power" => PowerSource::Ac,
        "Battery Power" => PowerSource::Battery,
        _ => PowerSource::Unknown,
    }
}

pub fn has_battery() -> bool {
    let Some(info) = power_sources_info() else {
        return false;
    };
    // SAFETY: a Copy function, so the caller owns the returned array.
    let Some(sources) =
        (unsafe { IOPSCopyPowerSourcesList(&info).map(|list| CFRetained::from_raw(list)) })
    else {
        return false;
    };
    let type_key = CFString::from_static_str("Type");
    (0..sources.len())
        .filter_map(|i| sources.get(i))
        .any(|source| {
            // SAFETY: a Get function on a live snapshot; the result is retained before use.
            let description = unsafe {
                IOPSGetPowerSourceDescription(&info, &source).map(|d| CFRetained::retain(d))
            };
            description
                .and_then(|description| description.get(&type_key))
                .and_then(|kind| kind.downcast::<CFString>().ok())
                .is_some_and(|kind| kind.to_string() == "InternalBattery")
        })
}
