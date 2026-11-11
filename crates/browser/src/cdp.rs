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

/// A base64 PNG of the viewport, or of the whole page.
pub async fn screenshot(cdp: &impl Cdp, full_page: bool, scale: f64) -> Result<String, String> {
    let params = if !full_page && scale == 1.0 {
        json!({ "format": "png" })
    } else {
        let metrics = cdp.call("Page.getLayoutMetrics", json!({})).await?;
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

    #[tokio::test]
    async fn viewport_screenshots_skip_layout_metrics() {
        let cdp = FakeCdp::answering(|_, _| Ok(json!({ "data": "png" })));
        assert_eq!(screenshot(&cdp, false, 1.0).await.unwrap(), "png");
        assert_eq!(cdp.methods(), ["Page.captureScreenshot"]);
    }

    #[tokio::test]
    async fn scaled_screenshots_clip_the_visual_viewport() {
        let cdp = FakeCdp::answering(|method, _| {
            Ok(match method {
                "Page.getLayoutMetrics" => json!({ "cssVisualViewport": {
                    "pageX": 0, "pageY": 120, "clientWidth": 393, "clientHeight": 852 } }),
                _ => json!({ "data": "png" }),
            })
        });
        screenshot(&cdp, false, 1.0 / 3.0).await.unwrap();
        let clip = &cdp.params("Page.captureScreenshot")[0]["clip"];
        assert_eq!(clip["y"], 120.0);
        assert_eq!(clip["width"], 393.0);
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
