use serde_json::{Value, json};

use crate::cdp::{Calls, Cdp, call_all, evaluate};

const MODIFIER_ALT: u32 = 1;
const MODIFIER_CONTROL: u32 = 2;
const MODIFIER_META: u32 = 4;
const MODIFIER_SHIFT: u32 = 8;

const SELECT_FUNCTION: &str = r#"function (wanted) {
  if (!(this instanceof HTMLSelectElement)) return { error: "the ref is not a <select> element" };
  const options = Array.from(this.options);
  const picked = options.filter((o) => wanted.some((w) => o.value === w || o.label === w || o.text.trim() === w));
  if (!picked.length) return { error: "no option matches", options: options.map((o) => o.label) };
  if (!this.multiple) picked.length = 1;
  for (const o of options) o.selected = picked.includes(o);
  this.dispatchEvent(new Event("input", { bubbles: true }));
  this.dispatchEvent(new Event("change", { bubbles: true }));
  return { selected: picked.map((o) => o.value) };
}"#;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Ref(i64),
    Point { x: f64, y: f64 },
}

impl Target {
    pub fn from_args(
        reference: Option<&str>,
        x: Option<f64>,
        y: Option<f64>,
    ) -> Result<Option<Target>, String> {
        match (reference, x, y) {
            (None, None, None) => Ok(None),
            (Some(reference), None, None) => crate::snapshot::parse_ref(reference)
                .map(|id| Some(Target::Ref(id)))
                .ok_or_else(|| {
                    format!("\"{reference}\" is not a ref; use one like \"e412\" from snapshot")
                }),
            (None, Some(x), Some(y)) if x.is_finite() && y.is_finite() => {
                Ok(Some(Target::Point { x, y }))
            }
            (None, Some(_), Some(_)) => Err("x and y must be numbers".into()),
            (None, _, _) => Err("give both x and y".into()),
            (Some(_), _, _) => Err("give either ref or x and y, not both".into()),
        }
    }

    pub fn required(
        reference: Option<&str>,
        x: Option<f64>,
        y: Option<f64>,
    ) -> Result<Target, String> {
        Target::from_args(reference, x, y)?
            .ok_or_else(|| "give a ref from snapshot, or x and y".into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(name: Option<&str>) -> Result<Button, String> {
        match name.map(str::to_ascii_lowercase).as_deref() {
            None | Some("left") => Ok(Button::Left),
            Some("right") => Ok(Button::Right),
            Some("middle") => Ok(Button::Middle),
            Some(other) => Err(format!(
                "unknown button \"{other}\"; use left, right or middle"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
        }
    }

    fn mask(self) -> u32 {
        match self {
            Button::Left => 1,
            Button::Right => 2,
            Button::Middle => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyPress {
    key: String,
    code: String,
    key_code: u32,
    text: Option<String>,
    modifiers: u32,
}

const NAMED_KEYS: &[(&str, &str, u32, Option<&str>)] = &[
    ("Enter", "Enter", 13, Some("\r")),
    ("Tab", "Tab", 9, None),
    ("Escape", "Escape", 27, None),
    ("Backspace", "Backspace", 8, None),
    ("Delete", "Delete", 46, None),
    ("Insert", "Insert", 45, None),
    ("ArrowUp", "ArrowUp", 38, None),
    ("ArrowDown", "ArrowDown", 40, None),
    ("ArrowLeft", "ArrowLeft", 37, None),
    ("ArrowRight", "ArrowRight", 39, None),
    ("Home", "Home", 36, None),
    ("End", "End", 35, None),
    ("PageUp", "PageUp", 33, None),
    ("PageDown", "PageDown", 34, None),
    (" ", "Space", 32, Some(" ")),
];

impl KeyPress {
    pub fn parse(spec: &str) -> Result<KeyPress, String> {
        let spec = spec.trim();
        let (modifier_names, key) = match spec.rsplit_once('+') {
            Some((modifiers, "")) => (modifiers.strip_suffix('+'), "+"),
            Some((modifiers, key)) => (Some(modifiers), key),
            None => (None, spec),
        };
        let mut modifiers = 0;
        for name in modifier_names
            .into_iter()
            .flat_map(|names| names.split('+'))
        {
            modifiers |= match name.to_ascii_lowercase().as_str() {
                "alt" => MODIFIER_ALT,
                "control" | "ctrl" => MODIFIER_CONTROL,
                "meta" | "cmd" | "win" => MODIFIER_META,
                "shift" => MODIFIER_SHIFT,
                _ => return Err(format!("unknown modifier \"{name}\" in \"{spec}\"")),
            };
        }
        let key = match key {
            "Space" | "space" => " ",
            "Esc" => "Escape",
            "Return" => "Enter",
            other => other,
        };
        let mut press = if let Some((name, code, key_code, text)) = NAMED_KEYS
            .iter()
            .find(|(name, ..)| name.eq_ignore_ascii_case(key))
        {
            KeyPress {
                key: name.to_string(),
                code: code.to_string(),
                key_code: *key_code,
                text: text.map(str::to_string),
                modifiers,
            }
        } else if let Some(number) = key
            .strip_prefix(['F', 'f'])
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| (1..=12).contains(n))
        {
            KeyPress {
                key: format!("F{number}"),
                code: format!("F{number}"),
                key_code: 111 + number,
                text: None,
                modifiers,
            }
        } else {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return Err(format!(
                    "unknown key \"{key}\"; use a single character or a name like Enter, Tab, Escape, ArrowDown"
                ));
            };
            let c = if modifiers & MODIFIER_SHIFT != 0 {
                c.to_ascii_uppercase()
            } else {
                c
            };
            let (code, key_code) = if c.is_ascii_alphabetic() {
                let upper = c.to_ascii_uppercase();
                (format!("Key{upper}"), upper as u32)
            } else if c.is_ascii_digit() {
                (format!("Digit{c}"), c as u32)
            } else {
                (String::new(), 0)
            };
            KeyPress {
                key: c.to_string(),
                code,
                key_code,
                text: Some(c.to_string()),
                modifiers,
            }
        };
        if modifiers & (MODIFIER_CONTROL | MODIFIER_ALT | MODIFIER_META) != 0 {
            press.text = None;
        }
        Ok(press)
    }

    fn events(&self) -> Calls {
        let mut down = json!({
            "type": if self.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": self.key,
            "code": self.code,
            "windowsVirtualKeyCode": self.key_code,
            "modifiers": self.modifiers,
        });
        if let Some(text) = &self.text {
            down["text"] = text.clone().into();
            down["unmodifiedText"] = text.clone().into();
        }
        let up = json!({
            "type": "keyUp",
            "key": self.key,
            "code": self.code,
            "windowsVirtualKeyCode": self.key_code,
            "modifiers": self.modifiers,
        });
        vec![
            ("Input.dispatchKeyEvent", down),
            ("Input.dispatchKeyEvent", up),
        ]
    }
}

pub fn quad_centre(quads: &Value) -> Option<(f64, f64)> {
    quads
        .get("quads")?
        .as_array()?
        .iter()
        .filter_map(|quad| {
            let points: Vec<f64> = quad.as_array()?.iter().filter_map(Value::as_f64).collect();
            let [x1, y1, x2, y2, x3, y3, x4, y4] = points.as_slice().try_into().ok()?;
            let area = ((x1 * y2 - x2 * y1)
                + (x2 * y3 - x3 * y2)
                + (x3 * y4 - x4 * y3)
                + (x4 * y1 - x1 * y4))
                .abs()
                / 2.0;
            (area >= 1.0).then(|| ((x1 + x2 + x3 + x4) / 4.0, (y1 + y2 + y3 + y4) / 4.0))
        })
        .next()
}

fn stale(id: i64) -> String {
    format!("ref e{id} is stale or not rendered; call snapshot again")
}

pub async fn resolve(cdp: &impl Cdp, target: Target) -> Result<(f64, f64), String> {
    let id = match target {
        Target::Point { x, y } => return Ok((x, y)),
        Target::Ref(id) => id,
    };
    cdp.call("DOM.scrollIntoViewIfNeeded", json!({ "backendNodeId": id }))
        .await
        .map_err(|_| stale(id))?;
    let quads = cdp
        .call("DOM.getContentQuads", json!({ "backendNodeId": id }))
        .await
        .map_err(|_| stale(id))?;
    quad_centre(&quads).ok_or_else(|| format!("ref e{id} is not visible: it has no size on screen"))
}

fn point_json(x: f64, y: f64) -> Value {
    json!({ "x": x.round(), "y": y.round() })
}

fn mouse(kind: &str, x: f64, y: f64) -> Value {
    json!({ "type": kind, "x": x, "y": y })
}

fn click_events(x: f64, y: f64, button: Button, double: bool, mobile: bool) -> Calls {
    let count = if double { 2 } else { 1 };
    if mobile {
        return (0..count)
            .flat_map(|_| {
                [
                    (
                        "Input.dispatchTouchEvent",
                        json!({ "type": "touchStart", "touchPoints": [{ "x": x, "y": y }] }),
                    ),
                    (
                        "Input.dispatchTouchEvent",
                        json!({ "type": "touchEnd", "touchPoints": [] }),
                    ),
                ]
            })
            .collect();
    }
    let mut calls = vec![("Input.dispatchMouseEvent", mouse("mouseMoved", x, y))];
    for click_count in 1..=count {
        for kind in ["mousePressed", "mouseReleased"] {
            let mut event = mouse(kind, x, y);
            event["button"] = button.name().into();
            event["clickCount"] = click_count.into();
            if kind == "mousePressed" {
                event["buttons"] = button.mask().into();
            }
            calls.push(("Input.dispatchMouseEvent", event));
        }
    }
    calls
}

async fn at_target(
    cdp: &impl Cdp,
    target: Target,
    events: impl FnOnce(f64, f64) -> Calls,
) -> Result<Value, String> {
    let (x, y) = resolve(cdp, target).await?;
    call_all(cdp, events(x, y)).await?;
    Ok(json!({ "at": point_json(x, y) }))
}

pub async fn click(
    cdp: &impl Cdp,
    target: Target,
    button: Button,
    double: bool,
    mobile: bool,
) -> Result<Value, String> {
    if mobile && button != Button::Left {
        return Err("mobile tabs take taps only; right and middle clicks are not available".into());
    }
    at_target(cdp, target, |x, y| {
        click_events(x, y, button, double, mobile)
    })
    .await
}

pub async fn hover(cdp: &impl Cdp, target: Target) -> Result<Value, String> {
    at_target(cdp, target, |x, y| {
        vec![("Input.dispatchMouseEvent", mouse("mouseMoved", x, y))]
    })
    .await
}

fn drag_events(from: (f64, f64), to: (f64, f64), steps: u32, mobile: bool) -> Calls {
    let steps = steps.max(1);
    let path = (1..=steps).map(|i| {
        let t = f64::from(i) / f64::from(steps);
        (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
    });
    if mobile {
        let touch = |kind: &str, points: Value| {
            (
                "Input.dispatchTouchEvent",
                json!({ "type": kind, "touchPoints": points }),
            )
        };
        let mut calls = vec![touch("touchStart", json!([{ "x": from.0, "y": from.1 }]))];
        calls.extend(path.map(|(x, y)| touch("touchMove", json!([{ "x": x, "y": y }]))));
        calls.push(touch("touchEnd", json!([])));
        return calls;
    }
    let held = |kind: &str, x: f64, y: f64| {
        let mut event = mouse(kind, x, y);
        event["button"] = Button::Left.name().into();
        event["buttons"] = Button::Left.mask().into();
        if kind != "mouseMoved" {
            event["clickCount"] = 1.into();
        }
        ("Input.dispatchMouseEvent", event)
    };
    let mut calls = vec![
        (
            "Input.dispatchMouseEvent",
            mouse("mouseMoved", from.0, from.1),
        ),
        held("mousePressed", from.0, from.1),
    ];
    calls.extend(path.map(|(x, y)| held("mouseMoved", x, y)));
    let mut release = held("mouseReleased", to.0, to.1);
    release.1["buttons"] = 0.into();
    calls.push(release);
    calls
}

/// Resolves `to` first: scrolling it into view could move `from`, which is where the press lands.
pub async fn drag(
    cdp: &impl Cdp,
    from: Target,
    to: Target,
    steps: u32,
    mobile: bool,
) -> Result<Value, String> {
    let end = resolve(cdp, to).await?;
    let start = resolve(cdp, from).await?;
    call_all(cdp, drag_events(start, end, steps, mobile)).await?;
    Ok(json!({ "from": point_json(start.0, start.1), "to": point_json(end.0, end.1) }))
}

pub async fn press_key(cdp: &impl Cdp, key: &KeyPress) -> Result<Value, String> {
    call_all(cdp, key.events()).await?;
    Ok(json!({}))
}

fn typing_events(text: &str, clear: bool, submit: bool) -> Calls {
    let mut calls = Vec::new();
    if clear {
        let select_all = KeyPress::parse("Control+a").expect("valid key");
        calls.extend(select_all.events());
        calls.extend(KeyPress::parse("Backspace").expect("valid key").events());
    }
    if !text.is_empty() {
        calls.push(("Input.insertText", json!({ "text": text })));
    }
    if submit {
        calls.extend(KeyPress::parse("Enter").expect("valid key").events());
    }
    calls
}

pub async fn type_text(
    cdp: &impl Cdp,
    target: Option<Target>,
    text: &str,
    clear: bool,
    submit: bool,
    mobile: bool,
) -> Result<Value, String> {
    match target {
        Some(target) => {
            at_target(cdp, target, |x, y| {
                let mut calls = click_events(x, y, Button::Left, false, mobile);
                calls.extend(typing_events(text, clear, submit));
                calls
            })
            .await
        }
        None => {
            call_all(cdp, typing_events(text, clear, submit)).await?;
            Ok(json!({}))
        }
    }
}

pub async fn scroll(
    cdp: &impl Cdp,
    target: Option<Target>,
    delta_x: f64,
    delta_y: f64,
) -> Result<Value, String> {
    let wheel = |x: f64, y: f64| {
        let mut event = mouse("mouseWheel", x, y);
        event["deltaX"] = delta_x.into();
        event["deltaY"] = delta_y.into();
        vec![("Input.dispatchMouseEvent", event)]
    };
    match target {
        Some(Target::Ref(id)) if delta_x == 0.0 && delta_y == 0.0 => {
            let (x, y) = resolve(cdp, Target::Ref(id)).await?;
            Ok(json!({ "at": point_json(x, y) }))
        }
        Some(target) => at_target(cdp, target, wheel).await,
        None => {
            let metrics = cdp.call("Page.getLayoutMetrics", json!({})).await.ok();
            let half = |key: &str| {
                metrics
                    .as_ref()
                    .and_then(|m| m["cssLayoutViewport"].get(key))
                    .and_then(Value::as_f64)
                    .map_or(100.0, |size| size / 2.0)
            };
            call_all(cdp, wheel(half("clientWidth"), half("clientHeight"))).await?;
            Ok(json!({}))
        }
    }
}

pub async fn select_option(cdp: &impl Cdp, id: i64, values: Vec<String>) -> Result<Value, String> {
    let resolved = cdp
        .call("DOM.resolveNode", json!({ "backendNodeId": id }))
        .await
        .map_err(|_| stale(id))?;
    let object_id = resolved["object"]["objectId"]
        .as_str()
        .ok_or_else(|| stale(id))?;
    let result = cdp
        .call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": SELECT_FUNCTION,
                "arguments": [{ "value": values }],
                "returnByValue": true,
            }),
        )
        .await?;
    let value = result["result"]["value"].clone();
    match value.get("error").and_then(Value::as_str) {
        Some(error) => Err(match value.get("options") {
            Some(options) => format!("{error}; the options are {options}"),
            None => error.to_string(),
        }),
        None => Ok(value),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WaitCondition {
    Text(String),
    TextGone(String),
    Visible(i64),
}

pub async fn check(cdp: &impl Cdp, condition: &WaitCondition) -> bool {
    let (text, present) = match condition {
        WaitCondition::Visible(id) => {
            return cdp
                .call("DOM.getContentQuads", json!({ "backendNodeId": id }))
                .await
                .is_ok_and(|quads| quad_centre(&quads).is_some());
        }
        WaitCondition::Text(text) => (text, true),
        WaitCondition::TextGone(text) => (text, false),
    };
    let expression = format!(
        "(document.body ? document.body.innerText : \"\").includes({})",
        serde_json::to_string(text).unwrap_or_default()
    );
    evaluate(cdp, &expression)
        .await
        .ok()
        .and_then(|v| v.as_bool())
        == Some(present)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_need_a_ref_or_both_coordinates() {
        assert_eq!(
            Target::from_args(Some("e12"), None, None),
            Ok(Some(Target::Ref(12)))
        );
        assert_eq!(
            Target::from_args(None, Some(1.5), Some(2.0)),
            Ok(Some(Target::Point { x: 1.5, y: 2.0 }))
        );
        assert_eq!(Target::from_args(None, None, None), Ok(None));
        assert!(Target::from_args(None, Some(1.0), None).is_err());
        assert!(Target::from_args(Some("e1"), Some(1.0), Some(2.0)).is_err());
        assert!(Target::from_args(Some("button"), None, None).is_err());
        assert!(Target::from_args(None, Some(f64::NAN), Some(1.0)).is_err());
        assert!(Target::required(None, None, None).is_err());
    }

    #[test]
    fn parses_named_keys_and_modifiers() {
        let enter = KeyPress::parse("Enter").unwrap();
        assert_eq!(enter.key_code, 13);
        assert_eq!(enter.text.as_deref(), Some("\r"));

        let select_all = KeyPress::parse("Ctrl+a").unwrap();
        assert_eq!(select_all.key, "a");
        assert_eq!(select_all.code, "KeyA");
        assert_eq!(select_all.key_code, 65);
        assert_eq!(select_all.modifiers, MODIFIER_CONTROL);
        assert_eq!(select_all.text, None);

        let back_tab = KeyPress::parse("Shift+Tab").unwrap();
        assert_eq!(back_tab.modifiers, MODIFIER_SHIFT);
        assert_eq!(back_tab.code, "Tab");

        let shifted = KeyPress::parse("Shift+x").unwrap();
        assert_eq!(shifted.key, "X");
        assert_eq!(shifted.text.as_deref(), Some("X"));

        assert_eq!(KeyPress::parse("escape").unwrap().key, "Escape");
        assert_eq!(KeyPress::parse("Space").unwrap().code, "Space");
        assert_eq!(KeyPress::parse("F5").unwrap().key_code, 116);
        assert_eq!(KeyPress::parse("7").unwrap().code, "Digit7");
        assert_eq!(
            KeyPress::parse("Control+Shift+Alt+Meta+k")
                .unwrap()
                .modifiers,
            15
        );
        assert_eq!(KeyPress::parse("Control++").unwrap().key, "+");
        assert_eq!(KeyPress::parse("+").unwrap().key, "+");
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(KeyPress::parse("Hyper+a").is_err());
        assert!(KeyPress::parse("Banana").is_err());
        assert!(KeyPress::parse("F13").is_err());
    }

    #[test]
    fn key_events_send_text_only_with_down() {
        let events = KeyPress::parse("a").unwrap().events();
        assert_eq!(events[0].1["type"], "keyDown");
        assert_eq!(events[0].1["text"], "a");
        assert_eq!(events[1].1["type"], "keyUp");
        assert!(events[1].1.get("text").is_none());
        assert_eq!(
            KeyPress::parse("Escape").unwrap().events()[0].1["type"],
            "rawKeyDown"
        );
    }

    #[test]
    fn finds_the_centre_of_the_first_visible_quad() {
        let quads = json!({ "quads": [
            [5, 5, 5, 5, 5, 5, 5, 5],
            [10, 20, 30, 20, 30, 40, 10, 40],
        ] });
        assert_eq!(quad_centre(&quads), Some((20.0, 30.0)));
        assert_eq!(quad_centre(&json!({ "quads": [] })), None);
        assert_eq!(quad_centre(&json!({})), None);
    }

    #[test]
    fn mobile_clicks_are_taps() {
        let taps = click_events(1.0, 2.0, Button::Left, true, true);
        assert_eq!(taps.len(), 4);
        assert!(
            taps.iter()
                .all(|(method, _)| *method == "Input.dispatchTouchEvent")
        );

        let clicks = click_events(1.0, 2.0, Button::Right, true, false);
        let kinds: Vec<_> = clicks
            .iter()
            .map(|(_, e)| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "mouseMoved",
                "mousePressed",
                "mouseReleased",
                "mousePressed",
                "mouseReleased"
            ]
        );
        assert_eq!(clicks[3].1["clickCount"], 2);
        assert_eq!(clicks[1].1["button"], "right");
        assert_eq!(clicks[1].1["buttons"], 2);
    }

    #[test]
    fn drags_hold_the_button_through_each_step() {
        let calls = drag_events((0.0, 0.0), (100.0, 50.0), 4, false);
        let kinds: Vec<_> = calls
            .iter()
            .map(|(_, e)| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "mouseMoved",
                "mousePressed",
                "mouseMoved",
                "mouseMoved",
                "mouseMoved",
                "mouseMoved",
                "mouseReleased"
            ]
        );
        assert!(calls[0].1.get("buttons").is_none());
        assert!(calls[1..6].iter().all(|(_, e)| e["buttons"] == 1));
        assert_eq!(
            (calls[2].1["x"].as_f64(), calls[2].1["y"].as_f64()),
            (Some(25.0), Some(12.5))
        );
        assert_eq!(
            (calls[5].1["x"].as_f64(), calls[5].1["y"].as_f64()),
            (Some(100.0), Some(50.0))
        );
        assert_eq!(calls[6].1["buttons"], 0);
        assert_eq!(calls[6].1["button"], "left");

        let touches = drag_events((0.0, 0.0), (10.0, 0.0), 2, true);
        let kinds: Vec<_> = touches
            .iter()
            .map(|(_, e)| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["touchStart", "touchMove", "touchMove", "touchEnd"]);
        assert_eq!(drag_events((0.0, 0.0), (1.0, 1.0), 0, false).len(), 4);
    }

    #[test]
    fn parses_buttons() {
        assert_eq!(Button::parse(None), Ok(Button::Left));
        assert_eq!(Button::parse(Some("Right")), Ok(Button::Right));
        assert!(Button::parse(Some("back")).is_err());
    }

    #[tokio::test]
    async fn clicking_a_ref_scrolls_it_in_then_presses_at_its_centre() {
        let cdp = crate::cdp::fake::FakeCdp::answering(|method, _| {
            Ok(match method {
                "DOM.getContentQuads" => json!({ "quads": [[10, 20, 30, 20, 30, 40, 10, 40]] }),
                _ => json!({}),
            })
        });
        let result = click(&cdp, Target::Ref(7), Button::Left, false, false)
            .await
            .unwrap();
        assert_eq!(result["at"], json!({ "x": 20.0, "y": 30.0 }));
        assert_eq!(
            cdp.methods(),
            [
                "DOM.scrollIntoViewIfNeeded",
                "DOM.getContentQuads",
                "Input.dispatchMouseEvent",
                "Input.dispatchMouseEvent",
                "Input.dispatchMouseEvent"
            ]
        );
    }

    #[tokio::test]
    async fn a_stale_ref_says_to_snapshot_again() {
        let cdp = crate::cdp::fake::FakeCdp::answering(|_, _| Err("No node".into()));
        let error = hover(&cdp, Target::Ref(3)).await.unwrap_err();
        assert!(error.contains("call snapshot again"));
    }

    #[tokio::test]
    async fn select_option_lists_the_options_when_none_match() {
        let cdp = crate::cdp::fake::FakeCdp::answering(|method, _| {
            Ok(match method {
                "DOM.resolveNode" => json!({ "object": { "objectId": "o1" } }),
                _ => {
                    json!({ "result": { "value": { "error": "no option matches", "options": ["A", "B"] } } })
                }
            })
        });
        let error = select_option(&cdp, 1, vec!["C".into()]).await.unwrap_err();
        assert_eq!(error, r#"no option matches; the options are ["A","B"]"#);
    }
}
