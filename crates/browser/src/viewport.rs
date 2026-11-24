use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cdp::{Calls, Cdp, call_all};

pub const MIN_SIZE: u32 = 200;
pub const MAX_SIZE: u32 = 3840;
/// The frame assumed for a tab that has not been shown yet.
pub const DEFAULT_FRAME: Frame = Frame {
    width: 1280.0,
    height: 800.0,
};
const MOBILE_SCALE_FACTOR: f64 = 3.0;
const USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";

pub const PRESETS: [&str; 5] = ["fit", "phone", "tablet", "laptop", "desktop"];
pub const PHONE: Viewport = Viewport::Fixed {
    width: 393,
    height: 852,
    mobile: true,
};
pub const TABLET: Viewport = Viewport::Fixed {
    width: 820,
    height: 1180,
    mobile: true,
};
pub const LAPTOP: Viewport = Viewport::Fixed {
    width: 1280,
    height: 800,
    mobile: false,
};
pub const DESKTOP: Viewport = Viewport::Fixed {
    width: 1440,
    height: 900,
    mobile: false,
};

/// The box a tab is drawn in, in CSS pixels of the app's window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Viewport {
    #[default]
    Fit,
    Fixed {
        width: u32,
        height: u32,
        mobile: bool,
    },
}

impl Viewport {
    /// A fixed viewport, refused when a side is outside `MIN_SIZE..=MAX_SIZE`.
    pub fn fixed(width: u32, height: u32, mobile: bool) -> Result<Viewport, String> {
        let viewport = Viewport::Fixed {
            width,
            height,
            mobile,
        };
        viewport.check()?;
        Ok(viewport)
    }

    pub fn check(&self) -> Result<(), String> {
        if let Viewport::Fixed { width, height, .. } = *self {
            for (side, size) in [("width", width), ("height", height)] {
                if !(MIN_SIZE..=MAX_SIZE).contains(&size) {
                    return Err(format!(
                        "{side} must be between {MIN_SIZE} and {MAX_SIZE} CSS pixels, not {size}"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn preset(name: &str) -> Result<Viewport, String> {
        match name.to_ascii_lowercase().as_str() {
            "fit" => Ok(Viewport::Fit),
            "phone" => Ok(PHONE),
            "tablet" => Ok(TABLET),
            "laptop" => Ok(LAPTOP),
            "desktop" => Ok(DESKTOP),
            _ => Err(format!(
                "unknown viewport preset {name:?}; use one of {}",
                PRESETS.join(", ")
            )),
        }
    }

    pub fn mobile(&self) -> bool {
        matches!(self, Viewport::Fixed { mobile: true, .. })
    }
}

/// How much a fixed viewport is shrunk to fit `frame`. Never enlarged. A side of the frame that
/// isn't a positive size doesn't constrain it.
pub fn scale_for(viewport: &Viewport, frame: Frame) -> f64 {
    let fit = |frame: f64, size: u32| {
        if frame > 0.0 {
            frame / f64::from(size)
        } else {
            f64::INFINITY
        }
    };
    match *viewport {
        Viewport::Fixed { width, height, .. } => fit(frame.width, width)
            .min(fit(frame.height, height))
            .min(1.0),
        Viewport::Fit => 1.0,
    }
}

pub fn metrics_call(viewport: &Viewport, frame: Frame) -> (&'static str, Value) {
    match *viewport {
        Viewport::Fit => ("Emulation.clearDeviceMetricsOverride", json!({})),
        Viewport::Fixed {
            width,
            height,
            mobile,
        } => {
            let orientation = if width <= height {
                "portraitPrimary"
            } else {
                "landscapePrimary"
            };
            (
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": width,
                    "height": height,
                    "deviceScaleFactor": if mobile { MOBILE_SCALE_FACTOR } else { 0.0 },
                    "mobile": mobile,
                    "screenWidth": width,
                    "screenHeight": height,
                    "screenOrientation": { "type": orientation, "angle": 0 },
                    "scale": scale_for(viewport, frame),
                    "dontSetVisibleSize": true,
                }),
            )
        }
    }
}

fn mobile_on() -> Calls {
    vec![
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
    ]
}

/// Turns touch and the phone user agent on for a mobile viewport, or off when leaving one.
/// Turning them off never fails: what didn't work comes back as notes.
pub async fn emulate_mobile(
    cdp: &impl Cdp,
    mobile: bool,
    was_mobile: bool,
) -> Result<Vec<String>, String> {
    if mobile {
        call_all(cdp, mobile_on()).await?;
        return Ok(Vec::new());
    }
    if !was_mobile {
        return Ok(Vec::new());
    }
    let off = [
        (
            "Emulation.setTouchEmulationEnabled",
            json!({ "enabled": false }),
            "touch emulation is still on",
        ),
        (
            "Emulation.setEmitTouchEventsForMouse",
            json!({ "enabled": false }),
            "mouse input still sends touch events",
        ),
        (
            "Emulation.setUserAgentOverride",
            json!({ "userAgent": "" }),
            "the phone user agent is still set",
        ),
    ];
    let mut notes = Vec::new();
    for (method, params, effect) in off {
        if let Err(e) = cdp.call(method, params).await {
            notes.push(format!("{effect} because {e}"));
        }
    }
    Ok(notes)
}

/// Sends `viewport`'s metrics through `metrics`, then matches touch and the user agent to it.
/// A page must have the phone user agent before its first navigation, or it loads with the
/// desktop one. The host sends the metrics, because it orders them with the resizes of the tab.
pub async fn apply(
    cdp: &impl Cdp,
    viewport: &Viewport,
    was_mobile: bool,
    metrics: impl Future<Output = Result<(), String>>,
) -> Result<Vec<String>, String> {
    metrics.await?;
    emulate_mobile(cdp, viewport.mobile(), was_mobile).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::fake::FakeCdp;

    fn frame(width: f64, height: f64) -> Frame {
        Frame { width, height }
    }

    async fn apply_through(
        cdp: &FakeCdp,
        viewport: &Viewport,
        was_mobile: bool,
    ) -> Result<Vec<String>, String> {
        apply(cdp, viewport, was_mobile, async {
            let (method, params) = metrics_call(viewport, DEFAULT_FRAME);
            cdp.call(method, params).await.map(drop)
        })
        .await
    }

    #[test]
    fn presets_are_found_by_name() {
        assert_eq!(Viewport::preset("fit"), Ok(Viewport::Fit));
        assert_eq!(Viewport::preset("Phone"), Ok(PHONE));
        assert_eq!(Viewport::preset("tablet"), Ok(TABLET));
        assert_eq!(Viewport::preset("laptop"), Ok(LAPTOP));
        assert_eq!(Viewport::preset("desktop"), Ok(DESKTOP));
        assert!(PHONE.mobile() && TABLET.mobile());
        assert!(!LAPTOP.mobile() && !DESKTOP.mobile() && !Viewport::Fit.mobile());
        assert!(
            Viewport::preset("watch")
                .unwrap_err()
                .contains("fit, phone, tablet, laptop, desktop")
        );
    }

    #[test]
    fn sizes_outside_the_range_are_refused() {
        assert_eq!(
            Viewport::fixed(200, 3840, false),
            Ok(Viewport::Fixed {
                width: 200,
                height: 3840,
                mobile: false,
            })
        );
        assert_eq!(
            Viewport::fixed(199, 800, false).unwrap_err(),
            "width must be between 200 and 3840 CSS pixels, not 199"
        );
        assert_eq!(
            Viewport::fixed(800, 3841, true).unwrap_err(),
            "height must be between 200 and 3840 CSS pixels, not 3841"
        );
        for preset in PRESETS {
            Viewport::preset(preset).unwrap().check().unwrap();
        }
    }

    #[test]
    fn serialises_with_a_kind_tag() {
        assert_eq!(
            serde_json::to_value(Viewport::Fit).unwrap(),
            json!({ "kind": "fit" })
        );
        assert_eq!(
            serde_json::to_value(PHONE).unwrap(),
            json!({ "kind": "fixed", "width": 393, "height": 852, "mobile": true })
        );
        for viewport in [Viewport::Fit, PHONE, DESKTOP] {
            let text = serde_json::to_string(&viewport).unwrap();
            assert_eq!(serde_json::from_str::<Viewport>(&text).unwrap(), viewport);
        }
        let unchecked: Viewport = serde_json::from_value(
            json!({ "kind": "fixed", "width": 10, "height": 10, "mobile": false }),
        )
        .unwrap();
        assert!(unchecked.check().is_err());
    }

    #[test]
    fn scale_shrinks_to_the_frame_but_never_enlarges() {
        assert_eq!(scale_for(&DESKTOP, frame(720.0, 900.0)), 0.5);
        assert_eq!(scale_for(&PHONE, frame(900.0, 900.0)), 1.0);
        assert_eq!(scale_for(&Viewport::Fit, frame(300.0, 300.0)), 1.0);
    }

    #[test]
    fn scale_fits_the_shorter_side_of_the_frame() {
        assert_eq!(scale_for(&PHONE, frame(1280.0, 426.0)), 0.5);
        assert_eq!(scale_for(&DESKTOP, frame(1440.0, 450.0)), 0.5);
        assert_eq!(scale_for(&DESKTOP, frame(360.0, 450.0)), 0.25);
    }

    #[test]
    fn a_side_without_a_size_does_not_constrain_the_scale() {
        assert_eq!(scale_for(&DESKTOP, frame(0.0, 450.0)), 0.5);
        assert_eq!(scale_for(&DESKTOP, frame(720.0, f64::NAN)), 0.5);
        assert_eq!(scale_for(&DESKTOP, frame(-1.0, 0.0)), 1.0);
        assert_eq!(scale_for(&DESKTOP, frame(f64::NAN, f64::NAN)), 1.0);
    }

    #[test]
    fn a_landscape_desktop_has_no_device_scale_factor() {
        let (method, params) = metrics_call(&DESKTOP, frame(720.0, 900.0));
        assert_eq!(method, "Emulation.setDeviceMetricsOverride");
        assert_eq!(
            params,
            json!({
                "width": 1440,
                "height": 900,
                "deviceScaleFactor": 0.0,
                "mobile": false,
                "screenWidth": 1440,
                "screenHeight": 900,
                "screenOrientation": { "type": "landscapePrimary", "angle": 0 },
                "scale": 0.5,
                "dontSetVisibleSize": true,
            })
        );
    }

    #[tokio::test]
    async fn a_phone_emulates_touch_and_the_phone_user_agent() {
        let cdp = FakeCdp::default();
        let notes = apply_through(&cdp, &PHONE, false).await.unwrap();
        assert!(notes.is_empty());
        assert_eq!(
            cdp.methods(),
            [
                "Emulation.setDeviceMetricsOverride",
                "Emulation.setTouchEmulationEnabled",
                "Emulation.setEmitTouchEventsForMouse",
                "Emulation.setUserAgentOverride",
            ]
        );
        let metrics = &cdp.params("Emulation.setDeviceMetricsOverride")[0];
        assert_eq!(metrics["deviceScaleFactor"], 3.0);
        assert_eq!(metrics["mobile"], true);
        assert_eq!(metrics["scale"], 800.0 / 852.0);
        assert_eq!(metrics["screenOrientation"]["type"], "portraitPrimary");
        assert_eq!(
            cdp.params("Emulation.setUserAgentOverride")[0]["platform"],
            "iPhone"
        );
    }

    #[tokio::test]
    async fn fit_from_a_desktop_viewport_only_clears_the_metrics() {
        let cdp = FakeCdp::default();
        apply_through(&cdp, &Viewport::Fit, false).await.unwrap();
        assert_eq!(cdp.methods(), ["Emulation.clearDeviceMetricsOverride"]);
    }

    #[tokio::test]
    async fn leaving_mobile_restores_touch_and_the_user_agent() {
        let cdp = FakeCdp::default();
        let notes = apply_through(&cdp, &Viewport::Fit, true).await.unwrap();
        assert!(notes.is_empty());
        assert_eq!(
            cdp.methods(),
            [
                "Emulation.clearDeviceMetricsOverride",
                "Emulation.setTouchEmulationEnabled",
                "Emulation.setEmitTouchEventsForMouse",
                "Emulation.setUserAgentOverride",
            ]
        );
        assert_eq!(
            cdp.params("Emulation.setTouchEmulationEnabled"),
            [json!({ "enabled": false })]
        );
        assert_eq!(
            cdp.params("Emulation.setUserAgentOverride"),
            [json!({ "userAgent": "" })]
        );
    }

    #[tokio::test]
    async fn a_failed_restore_is_a_note() {
        let cdp = FakeCdp::answering(|method, _| match method {
            "Emulation.setUserAgentOverride" => Err("no".into()),
            _ => Ok(json!({})),
        });
        let notes = apply_through(&cdp, &DESKTOP, true).await.unwrap();
        assert_eq!(notes, ["the phone user agent is still set because no"]);
        assert_eq!(cdp.methods().len(), 4);
    }

    #[tokio::test]
    async fn a_failed_metrics_override_fails() {
        let cdp = FakeCdp::answering(|_, _| Err("no".into()));
        assert!(apply_through(&cdp, &PHONE, false).await.is_err());
        assert_eq!(cdp.methods(), ["Emulation.setDeviceMetricsOverride"]);
    }
}
