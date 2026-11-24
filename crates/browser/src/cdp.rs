use std::future::Future;

use serde_json::{Value, json};

/// A Chrome DevTools Protocol connection to one page. The WebView2 host sends over
/// `CallDevToolsProtocolMethod`, the Chrome host over a WebSocket.
pub trait Cdp: Send + Sync {
    fn call(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, String>> + Send;
}

pub type Calls = Vec<(&'static str, Value)>;

pub async fn call_all(cdp: &impl Cdp, calls: Calls) -> Result<(), String> {
    for (method, params) in calls {
        cdp.call(method, params).await?;
    }
    Ok(())
}

/// Runs `expression` in the page and returns its value, failing if it threw.
pub async fn evaluate(cdp: &impl Cdp, expression: &str) -> Result<Value, String> {
    let result = cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )
        .await?;
    evaluated(result)
}

pub async fn evaluate_awaiting(
    cdp: &impl Cdp,
    context: i64,
    expression: &str,
) -> Result<Value, String> {
    let result = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "contextId": context,
                "awaitPromise": true,
                "returnByValue": true,
            }),
        )
        .await?;
    evaluated(result)
}

fn evaluated(result: Value) -> Result<Value, String> {
    if let Some(details) = result.get("exceptionDetails") {
        let message = details["exception"]["description"]
            .as_str()
            .or_else(|| details["text"].as_str())
            .unwrap_or("the script threw");
        return Err(message.to_string());
    }
    Ok(result["result"]["value"].clone())
}

fn number(value: &Value, key: &str) -> Result<f64, String> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("layout metrics are missing {key}"))
}

fn clip(
    rect: &Value,
    x: &str,
    y: &str,
    width: &str,
    height: &str,
    scale: f64,
) -> Result<Value, String> {
    Ok(json!({
        "x": if x.is_empty() { 0.0 } else { number(rect, x)? },
        "y": if y.is_empty() { 0.0 } else { number(rect, y)? },
        "width": number(rect, width)?,
        "height": number(rect, height)?,
        "scale": scale,
    }))
}

/// Device pixels per CSS pixel, from the layout metrics rather than the page's
/// `window.devicePixelRatio`, which page script can redefine.
fn device_pixel_ratio(metrics: &Value) -> Option<f64> {
    let device = metrics["visualViewport"]["clientWidth"].as_f64()?;
    let css = metrics["cssVisualViewport"]["clientWidth"].as_f64()?;
    Some(device / css).filter(|ratio| ratio.is_finite() && *ratio > 0.0)
}

/// A base64 PNG of the viewport, or of the whole page, in CSS pixels.
pub async fn screenshot(cdp: &impl Cdp, full_page: bool) -> Result<String, String> {
    let metrics = cdp.call("Page.getLayoutMetrics", json!({})).await;
    let scale = metrics
        .as_ref()
        .ok()
        .and_then(device_pixel_ratio)
        .map_or(1.0, |ratio| 1.0 / ratio);
    let params = if !full_page && scale == 1.0 {
        json!({ "format": "png" })
    } else {
        let metrics = metrics?;
        if full_page {
            let size = metrics
                .get("cssContentSize")
                .ok_or("layout metrics are missing cssContentSize")?;
            json!({
                "format": "png",
                "captureBeyondViewport": true,
                "clip": clip(size, "", "", "width", "height", scale)?,
            })
        } else {
            let viewport = metrics
                .get("cssVisualViewport")
                .ok_or("layout metrics are missing cssVisualViewport")?;
            json!({
                "format": "png",
                "clip": clip(viewport, "pageX", "pageY", "clientWidth", "clientHeight", scale)?,
            })
        }
    };
    let shot = cdp.call("Page.captureScreenshot", params).await?;
    shot.get("data")
        .and_then(Value::as_str)
        .filter(|data| !data.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "screenshot returned no data".to_string())
}

#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use super::*;

    type Responder = Box<dyn Fn(&str, &Value) -> Result<Value, String> + Send + Sync>;

    /// Records every call and answers from `respond`, or with `{}`.
    pub struct FakeCdp {
        pub calls: Mutex<Vec<(String, Value)>>,
        respond: Responder,
    }

    impl Default for FakeCdp {
        fn default() -> Self {
            Self::answering(|_, _| Ok(json!({})))
        }
    }

    impl FakeCdp {
        pub fn answering(
            respond: impl Fn(&str, &Value) -> Result<Value, String> + Send + Sync + 'static,
        ) -> Self {
            FakeCdp {
                calls: Mutex::new(Vec::new()),
                respond: Box::new(respond),
            }
        }

        pub fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }

        pub fn params(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| m == method)
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    impl Cdp for FakeCdp {
        async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            let answer = (self.respond)(method, &params);
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            answer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeCdp;
    use super::*;

    fn metrics(device_width: f64) -> Value {
        json!({
            "visualViewport": { "clientWidth": device_width, "clientHeight": 2556 },
            "cssVisualViewport": {
                "pageX": 0, "pageY": 120, "clientWidth": 393, "clientHeight": 852 },
            "cssContentSize": { "width": 393, "height": 3000 },
        })
    }

    fn answering_metrics(device_width: f64) -> FakeCdp {
        FakeCdp::answering(move |method, _| {
            Ok(match method {
                "Page.getLayoutMetrics" => metrics(device_width),
                _ => json!({ "data": "png" }),
            })
        })
    }

    #[tokio::test]
    async fn screenshots_at_one_device_pixel_per_css_pixel_are_not_clipped() {
        let cdp = answering_metrics(393.0);
        assert_eq!(screenshot(&cdp, false).await.unwrap(), "png");
        assert_eq!(
            cdp.params("Page.captureScreenshot"),
            [json!({ "format": "png" })]
        );
    }

    #[tokio::test]
    async fn screenshots_are_scaled_to_css_pixels_from_the_layout_metrics() {
        let cdp = answering_metrics(1179.0);
        screenshot(&cdp, false).await.unwrap();
        assert!(!cdp.methods().contains(&"Runtime.evaluate".to_string()));
        let clip = &cdp.params("Page.captureScreenshot")[0]["clip"];
        assert_eq!(clip["scale"], 1.0 / 3.0);
        assert_eq!(clip["y"], 120.0);
        assert_eq!(clip["width"], 393.0);
    }

    #[tokio::test]
    async fn full_page_screenshots_are_scaled_too() {
        let cdp = answering_metrics(589.5);
        screenshot(&cdp, true).await.unwrap();
        let params = &cdp.params("Page.captureScreenshot")[0];
        assert_eq!(params["captureBeyondViewport"], true);
        assert_eq!(params["clip"]["scale"], 1.0 / 1.5);
        assert_eq!(params["clip"]["height"], 3000.0);
    }

    #[tokio::test]
    async fn screenshots_assume_a_ratio_of_one_without_layout_metrics() {
        let cdp = FakeCdp::answering(|method, _| match method {
            "Page.getLayoutMetrics" => Err("no".into()),
            _ => Ok(json!({ "data": "png" })),
        });
        screenshot(&cdp, false).await.unwrap();
        assert_eq!(
            cdp.params("Page.captureScreenshot"),
            [json!({ "format": "png" })]
        );

        let partial = FakeCdp::answering(|method, _| {
            Ok(match method {
                "Page.getLayoutMetrics" => json!({ "cssVisualViewport": {
                    "pageX": 0, "pageY": 0, "clientWidth": 1440, "clientHeight": 900 } }),
                _ => json!({ "data": "png" }),
            })
        });
        screenshot(&partial, false).await.unwrap();
        assert_eq!(
            partial.params("Page.captureScreenshot"),
            [json!({ "format": "png" })]
        );
    }

    #[tokio::test]
    async fn evaluate_awaiting_runs_in_the_given_context() {
        let cdp = FakeCdp::answering(|_, _| Ok(json!({ "result": { "value": 3 } })));
        assert_eq!(evaluate_awaiting(&cdp, 7, "1 + 2").await.unwrap(), 3);
        let params = &cdp.params("Runtime.evaluate")[0];
        assert_eq!(params["contextId"], 7);
        assert_eq!(params["awaitPromise"], true);
        assert_eq!(params["returnByValue"], true);
    }

    #[tokio::test]
    async fn evaluate_reports_exceptions() {
        let cdp = FakeCdp::answering(|_, _| {
            Ok(
                json!({ "result": {}, "exceptionDetails": { "text": "Uncaught",
                "exception": { "description": "ReferenceError: x is not defined" } } }),
            )
        });
        assert_eq!(
            evaluate(&cdp, "x").await.unwrap_err(),
            "ReferenceError: x is not defined"
        );
    }
}
