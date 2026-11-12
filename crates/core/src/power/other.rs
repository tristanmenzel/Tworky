use anyhow::Result;

use super::{LockKind, PowerSource};

pub struct SleepLock;

impl SleepLock {
    pub fn acquire(_reason: &str) -> Result<SleepLock> {
        Ok(SleepLock)
    }

    pub fn kind(&self) -> LockKind {
        LockKind::None
    }

    pub fn refresh(&mut self) -> Result<()> {
        Ok(())
    }
}

pub fn power_source() -> PowerSource {
    PowerSource::Unknown
}

pub fn has_battery() -> bool {
    false
}
