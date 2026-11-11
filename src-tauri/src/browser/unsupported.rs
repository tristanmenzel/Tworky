use std::sync::Arc;

use serde_json::Value;
use tauri::AppHandle;
use tworky_browser::cdp::Cdp;

use super::{Bounds, Tab};

const UNSUPPORTED: &str = "the built-in browser is Windows-only for now";

pub struct TabCdp;

impl TabCdp {
    pub fn new(_: &AppHandle, _: u64) -> Self {
        TabCdp
    }
}

impl Cdp for TabCdp {
    async fn call(&self, _: &str, _: Value) -> Result<Value, String> {
        Err(UNSUPPORTED.to_string())
    }
}

pub async fn create(_: &AppHandle, _: &Arc<Tab>, _: &str) -> Result<(), String> {
    Err(UNSUPPORTED.into())
}

pub async fn destroy(_: &AppHandle, _: u64) {}

pub async fn set_bounds(_: &AppHandle, _: u64, _: Bounds) -> Result<(), String> {
    Ok(())
}

pub async fn show(_: &AppHandle, _: Option<u64>, _: u64, _: u64) -> Result<(), String> {
    Ok(())
}

pub async fn delete_profile(_: &AppHandle, _: &str) -> Result<(), String> {
    Ok(())
}

pub async fn history(_: &AppHandle, _: u64, _: &str) -> Result<(), String> {
    Err(UNSUPPORTED.into())
}
