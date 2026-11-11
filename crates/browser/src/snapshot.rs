use std::collections::HashMap;

use serde_json::Value;

const MAX_NAME_CHARS: usize = 200;
const PASSWORD_MASK: char = '\u{2022}';

const TRANSPARENT_ROLES: &[&str] = &[
    "generic",
    "none",
    "presentation",
    "LineBreak",
    "LabelText",
    "paragraph",
    "Section",
    "group",
    "ListMarker",
    "MenuListPopup",
];

const NAMED_BY_CONTENT: &[&str] = &[
    "button",
    "link",
    "heading",
    "option",
    "MenuListOption",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "checkbox",
    "radio",
    "switch",
    "treeitem",
    "cell",
    "columnheader",
    "rowheader",
    "label",
    "legend",
    "caption",
];

const EDITABLE_ROLES: &[&str] = &["textbox", "searchbox", "spinbutton"];

const STATES: &[&str] = &[
    "checked", "pressed", "expanded", "selected", "disabled", "focused", "required",
];

pub struct Outline {
    pub text: String,
    pub truncated: bool,
}

struct Node<'a> {
    ignored: bool,
    role: &'a str,
    name: &'a str,
    value: Option<&'a str>,
    properties: Vec<(&'a str, &'a Value)>,
    children: Vec<&'a str>,
    backend_id: Option<i64>,
}

impl<'a> Node<'a> {
    fn parse(raw: &'a Value) -> Option<(&'a str, Node<'a>)> {
        let id = raw.get("nodeId")?.as_str()?;
        let text = |key: &str| {
            raw.get(key)
                .and_then(|field| field.get("value"))
                .and_then(Value::as_str)
        };
        let properties = raw
            .get("properties")
            .and_then(Value::as_array)
            .map(|properties| {
                properties
                    .iter()
                    .filter_map(|property| {
                        Some((
                            property.get("name")?.as_str()?,
                            property.get("value")?.get("value")?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let children = raw
            .get("childIds")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        Some((
            id,
            Node {
                ignored: raw.get("ignored").and_then(Value::as_bool).unwrap_or(false),
                role: text("role").unwrap_or(""),
                name: text("name").unwrap_or("").trim(),
                value: text("value"),
                properties,
                children,
                backend_id: raw.get("backendDOMNodeId").and_then(Value::as_i64),
            },
        ))
    }

    fn is_text(&self) -> bool {
        self.role == "StaticText"
    }

    fn property(&self, name: &str) -> Option<&Value> {
        self.properties
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    }
}

pub fn outline(tree: &Value, max_chars: usize) -> Outline {
    let raw_nodes = tree
        .get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let nodes: HashMap<&str, Node> = raw_nodes.iter().filter_map(Node::parse).collect();
    let mut lines = Vec::new();
    if let Some(root) = raw_nodes
        .first()
        .and_then(|node| node.get("nodeId"))
        .and_then(Value::as_str)
    {
        render(&nodes, root, 0, &mut lines);
    }

    let mut text = String::new();
    let mut truncated = false;
    for line in lines {
        if text.chars().count() + line.chars().count() + 1 > max_chars {
            truncated = true;
            break;
        }
        text.push_str(&line);
        text.push('\n');
    }
    Outline { text, truncated }
}

fn render(nodes: &HashMap<&str, Node>, id: &str, depth: usize, lines: &mut Vec<String>) {
    let Some(node) = nodes.get(id) else {
        return;
    };
    if node.role == "InlineTextBox" || (node.is_text() && is_masked(node.name)) {
        return;
    }
    let shown = !node.ignored
        && node.role != "RootWebArea"
        && !(TRANSPARENT_ROLES.contains(&node.role) && node.name.is_empty())
        && !(node.is_text() && node.name.is_empty());
    let child_depth = if shown {
        lines.push(line(node, depth));
        depth + 1
    } else {
        depth
    };
    if EDITABLE_ROLES.contains(&node.role) {
        return;
    }
    let skip_text = shown && !node.name.is_empty() && NAMED_BY_CONTENT.contains(&node.role);
    for child in &node.children {
        if skip_text && nodes.get(child).is_some_and(Node::is_text) {
            continue;
        }
        render(nodes, child, child_depth, lines);
    }
}

fn line(node: &Node, depth: usize) -> String {
    let role = if node.is_text() { "text" } else { node.role };
    let mut line = format!("{}- {role}", "  ".repeat(depth));
    if !node.name.is_empty() {
        line.push(' ');
        line.push_str(&quote(node.name));
    }
    if let Some(level) = node.property("level").and_then(Value::as_i64) {
        line.push_str(&format!(" [level={level}]"));
    }
    match node.value.filter(|value| !value.is_empty()) {
        Some(value) if is_masked(value) => line.push_str(" [password]"),
        Some(value) if !node.is_text() => {
            line.push_str(&format!(" [value={}]", quote(value)));
        }
        _ => {}
    }
    for state in STATES {
        match node.property(state) {
            Some(Value::Bool(true)) => line.push_str(&format!(" [{state}]")),
            Some(Value::String(token)) if token == "true" => {
                line.push_str(&format!(" [{state}]"));
            }
            Some(Value::String(token)) if token == "mixed" => {
                line.push_str(&format!(" [{state}=mixed]"));
            }
            _ => {}
        }
    }
    if node.role == "link"
        && let Some(url) = node.property("url").and_then(Value::as_str)
    {
        line.push_str(&format!(" [url={url}]"));
    }
    if let Some(id) = node.backend_id {
        line.push_str(&format!(" [ref=e{id}]"));
    }
    line
}

fn is_masked(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c == PASSWORD_MASK)
}

fn quote(text: &str) -> String {
    let mut text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > MAX_NAME_CHARS {
        text = text.chars().take(MAX_NAME_CHARS).collect::<String>() + "…";
    }
    serde_json::to_string(&text).unwrap_or_default()
}

pub fn parse_ref(reference: &str) -> Option<i64> {
    reference
        .trim()
        .strip_prefix('e')
        .and_then(|id| id.parse().ok())
        .filter(|id| *id > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, role: &str, name: &str, backend: i64, children: &[&str]) -> Value {
        json!({
            "nodeId": id,
            "ignored": false,
            "role": { "type": "role", "value": role },
            "name": { "type": "computedString", "value": name },
            "childIds": children,
            "backendDOMNodeId": backend,
        })
    }

    fn tree(nodes: Vec<Value>) -> Value {
        json!({ "nodes": nodes })
    }

    #[test]
    fn outlines_meaningful_nodes_and_flattens_wrappers() {
        let mut heading = node("3", "heading", "Welcome", 30, &["4"]);
        heading["properties"] =
            json!([{ "name": "level", "value": { "type": "integer", "value": 1 } }]);
        let tree = tree(vec![
            node("1", "RootWebArea", "Page", 10, &["2"]),
            node("2", "generic", "", 20, &["3", "5", "7"]),
            heading,
            node("4", "StaticText", "Welcome", 40, &[]),
            node("5", "button", "Save", 50, &["6"]),
            node("6", "StaticText", "Save", 60, &[]),
            node("7", "StaticText", "Some   words", 70, &[]),
        ]);
        let outline = outline(&tree, 10_000);
        assert_eq!(
            outline.text,
            "- heading \"Welcome\" [level=1] [ref=e30]\n- button \"Save\" [ref=e50]\n- text \"Some words\" [ref=e70]\n"
        );
        assert!(!outline.truncated);
    }

    #[test]
    fn nests_children_of_shown_nodes() {
        let tree = tree(vec![
            node("1", "RootWebArea", "", 1, &["2"]),
            node("2", "navigation", "Main", 2, &["3"]),
            node("3", "link", "Home", 3, &[]),
        ]);
        assert_eq!(
            outline(&tree, 10_000).text,
            "- navigation \"Main\" [ref=e2]\n  - link \"Home\" [ref=e3]\n"
        );
    }

    #[test]
    fn skips_ignored_nodes_but_keeps_their_children() {
        let mut ignored = node("2", "div", "", 2, &["3"]);
        ignored["ignored"] = json!(true);
        let tree = tree(vec![
            node("1", "RootWebArea", "", 1, &["2"]),
            ignored,
            node("3", "button", "Go", 3, &[]),
        ]);
        assert_eq!(outline(&tree, 10_000).text, "- button \"Go\" [ref=e3]\n");
    }

    #[test]
    fn hides_inline_text_boxes() {
        let tree = tree(vec![
            node("1", "RootWebArea", "", 1, &["2"]),
            node("2", "StaticText", "Hello", 2, &["3"]),
            node("3", "InlineTextBox", "Hello", 3, &[]),
        ]);
        assert_eq!(outline(&tree, 10_000).text, "- text \"Hello\" [ref=e2]\n");
    }

    #[test]
    fn redacts_masked_password_values() {
        let mask = "\u{2022}\u{2022}\u{2022}\u{2022}";
        let mut password = node("2", "textbox", "Password", 2, &["4"]);
        password["value"] = json!({ "type": "string", "value": mask });
        let mut email = node("3", "textbox", "Email", 3, &["6"]);
        email["value"] = json!({ "type": "string", "value": "a@b.c" });
        let tree = tree(vec![
            node("1", "RootWebArea", "", 1, &["2", "3", "8"]),
            password,
            email,
            node("4", "generic", "", 4, &["5"]),
            node("5", "StaticText", mask, 5, &[]),
            node("6", "StaticText", "a@b.c", 6, &[]),
            node("8", "StaticText", mask, 8, &[]),
        ]);
        let text = outline(&tree, 10_000).text;
        assert_eq!(
            text,
            "- textbox \"Password\" [password] [ref=e2]\n- textbox \"Email\" [value=\"a@b.c\"] [ref=e3]\n"
        );
        assert!(!text.contains('\u{2022}'));
    }

    #[test]
    fn reports_states_and_link_urls() {
        let mut checkbox = node("2", "checkbox", "Agree", 2, &[]);
        checkbox["properties"] = json!([
            { "name": "checked", "value": { "type": "tristate", "value": "true" } },
            { "name": "disabled", "value": { "type": "boolean", "value": true } },
            { "name": "focusable", "value": { "type": "booleanOrUndefined", "value": true } },
        ]);
        let mut link = node("3", "link", "Docs", 3, &[]);
        link["properties"] = json!([{ "name": "url", "value": { "type": "string", "value": "https://x.test/docs" } }]);
        let tree = tree(vec![
            node("1", "RootWebArea", "", 1, &["2", "3"]),
            checkbox,
            link,
        ]);
        assert_eq!(
            outline(&tree, 10_000).text,
            "- checkbox \"Agree\" [checked] [disabled] [ref=e2]\n- link \"Docs\" [url=https://x.test/docs] [ref=e3]\n"
        );
    }

    #[test]
    fn truncates_at_a_line_boundary() {
        let children: Vec<String> = (2..50).map(|id| id.to_string()).collect();
        let child_ids: Vec<&str> = children.iter().map(String::as_str).collect();
        let mut nodes = vec![node("1", "RootWebArea", "", 1, &child_ids)];
        for id in &children {
            nodes.push(node(id, "button", "Button", id.parse().unwrap(), &[]));
        }
        let outline = outline(&tree(nodes), 100);
        assert!(outline.truncated);
        assert!(outline.text.chars().count() <= 100);
        assert!(outline.text.ends_with('\n'));
    }

    #[test]
    fn parses_refs() {
        assert_eq!(parse_ref("e412"), Some(412));
        assert_eq!(parse_ref(" e7 "), Some(7));
        assert_eq!(parse_ref("412"), None);
        assert_eq!(parse_ref("e0"), None);
        assert_eq!(parse_ref("ex"), None);
    }
}
