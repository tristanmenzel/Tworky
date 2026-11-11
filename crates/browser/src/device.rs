use serde_json::json;

use crate::cdp::{Cdp, call_all};

pub const NAME: &str = "iPhone 15";
pub const WIDTH: f64 = 393.0;
pub const HEIGHT: f64 = 852.0;
pub const SCALE_FACTOR: f64 = 3.0;
const USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";

/// Must run before the first navigation, or the first page loads with the desktop user agent.
pub async fn emulate(cdp: &impl Cdp) -> Result<(), String> {
    call_all(
        cdp,
        vec![
            (
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": 0,
                    "height": 0,
                    "deviceScaleFactor": SCALE_FACTOR,
                    "mobile": true,
                    "screenWidth": WIDTH,
                    "screenHeight": HEIGHT,
                    "screenOrientation": { "type": "portraitPrimary", "angle": 0 },
                }),
            ),
            (
                "Emulation.setTouchEmulationEnabled",
                json!({ "enabled": true, "maxTouchPoints": 5 }),
            ),
            (
                "Emulation.setEmitTouchEventsForMouse",
                json!({ "enabled": true, "configuration": "mobile" }),
            ),
            (
                "Emulation.setUserAgentOverride",
                json!({ "userAgent": USER_AGENT, "platform": "iPhone" }),
            ),
        ],
    )
    .await
}
