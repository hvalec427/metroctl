//! The Components tab's model: one app's React component tree, as the React
//! DevTools backend inside the app describes it (`operations`), plus the
//! selected component and what `inspectElement` returned for it.
//!
//! The wire format is react-devtools-shared's bridge protocol (version 2,
//! react-devtools-core 6.x): `operations` is a flat integer array — renderer
//! id, root id, a string table, then tree operations.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const OP_ADD: i64 = 1;
const OP_REMOVE: i64 = 2;
const OP_REORDER: i64 = 3;
const OP_BASE_DURATION: i64 = 4;
const OP_ERRORS_WARNINGS: i64 = 5;
const OP_REMOVE_ROOT: i64 = 6;
const OP_SUBTREE_MODE: i64 = 7;

const TYPE_ROOT: i64 = 11;
const TYPE_FORWARD_REF: i64 = 6;
const TYPE_MEMO: i64 = 8;

/// Dehydrated paths fetched right after an inspection, so a component's props
/// read a level or two deeper than the backend's first answer.
const MAX_HYDRATE: usize = 24;
/// The selected component is re-inspected this often to pick up new props.
const POLL: Duration = Duration::from_secs(1);

struct Node {
    kind: i64,
    name: String,
    key: Option<String>,
    parent: i64,
    children: Vec<i64>,
    renderer: i64,
}

/// One line of the tree as shown.
pub struct Row {
    pub id: i64,
    pub depth: usize,
    pub text: String,
}

/// A prop edit: the element, the prop's path, and the value it had before.
struct Edit {
    id: i64,
    path: Vec<Value>,
    was: Option<Value>,
}

/// What to ask the backend about the selected component.
pub struct InspectRequest {
    pub id: i64,
    pub renderer: i64,
    pub path: Option<Value>,
    pub force: bool,
}

#[derive(Default)]
pub struct Tree {
    nodes: HashMap<i64, Node>,
    roots: Vec<i64>,
    collapsed: HashSet<i64>,
    pub selected: Option<i64>,
    /// The last full answer for `selected`, with hydrated paths merged in.
    inspected: Option<(i64, Value)>,
    /// The inspection failed or the component is gone.
    inspect_error: Option<String>,
    requested: Option<i64>,
    last_request: Option<Instant>,
    hydrate: Vec<Value>,
    /// The tab is open and this app's backend talks to us.
    pub attached: bool,
    /// Waiting for a tap in the app (`startInspectingHost`).
    pub picking: bool,
    /// Why there is no tree (no React DevTools backend in this app).
    pub unavailable: Option<String>,
    /// The tree couldn't be decoded (an unsupported protocol change).
    pub error: Option<String>,
    /// Prop edits, kept through re-renders by the app.
    edits: Vec<Edit>,
}

impl Tree {
    /// Forget the tree (a reload, or a new connection to the backend).
    pub fn reset(&mut self) {
        let (attached, unavailable) = (self.attached, self.unavailable.take());
        *self = Tree::default();
        self.attached = attached;
        self.unavailable = unavailable;
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.nodes.len() <= self.roots.len()
    }

    /// Apply one `operations` message.
    pub fn apply(&mut self, ops: &[i64]) -> Result<(), String> {
        if ops.len() < 3 {
            return Ok(());
        }
        let renderer = ops[0];
        let root = ops[1];
        let mut i = 2;
        let table_len = ops[i] as usize;
        i += 1;
        let table_end = i + table_len;
        let mut strings: Vec<Option<String>> = vec![None];
        while i < table_end.min(ops.len()) {
            let len = ops[i] as usize;
            i += 1;
            let s: String = ops[i..(i + len).min(ops.len())]
                .iter()
                .filter_map(|&c| char::from_u32(c as u32))
                .collect();
            strings.push(Some(s));
            i += len;
        }
        let string = |id: i64| strings.get(id as usize).cloned().flatten();
        let arg = |i: &mut usize| -> Result<i64, String> {
            let v = ops.get(*i).copied().ok_or("operations ended early")?;
            *i += 1;
            Ok(v)
        };
        while i < ops.len() {
            match arg(&mut i)? {
                OP_ADD => {
                    let id = arg(&mut i)?;
                    let kind = arg(&mut i)?;
                    if kind == TYPE_ROOT {
                        i += 4; // strict mode, profiling flags, supports strict mode, owner metadata
                        if !self.roots.contains(&id) {
                            self.roots.push(id);
                        }
                        self.nodes.insert(
                            id,
                            Node {
                                kind,
                                name: "Root".into(),
                                key: None,
                                parent: 0,
                                children: Vec::new(),
                                renderer,
                            },
                        );
                    } else {
                        let parent = arg(&mut i)?;
                        let _owner = arg(&mut i)?;
                        let name = string(arg(&mut i)?).unwrap_or_else(|| "Anonymous".into());
                        let key = string(arg(&mut i)?);
                        self.nodes.insert(
                            id,
                            Node {
                                kind,
                                name,
                                key,
                                parent,
                                children: Vec::new(),
                                renderer,
                            },
                        );
                        if let Some(p) = self.nodes.get_mut(&parent) {
                            p.children.push(id);
                        }
                    }
                }
                OP_REMOVE => {
                    let n = arg(&mut i)?;
                    for _ in 0..n {
                        let id = arg(&mut i)?;
                        self.remove(id);
                    }
                }
                OP_REORDER => {
                    let id = arg(&mut i)?;
                    let n = arg(&mut i)? as usize;
                    let kids = ops.get(i..i + n).ok_or("operations ended early")?.to_vec();
                    i += n;
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.children = kids;
                    }
                }
                OP_BASE_DURATION => i += 2,
                OP_ERRORS_WARNINGS => i += 3,
                OP_REMOVE_ROOT => {
                    self.remove(root);
                    self.roots.retain(|&r| r != root);
                }
                OP_SUBTREE_MODE => i += 2,
                op => return Err(format!("unknown tree operation {op}")),
            }
        }
        if self.selected.is_some_and(|s| !self.nodes.contains_key(&s)) {
            self.selected = None;
        }
        Ok(())
    }

    fn remove(&mut self, id: i64) {
        if let Some(node) = self.nodes.remove(&id) {
            if let Some(p) = self.nodes.get_mut(&node.parent) {
                p.children.retain(|&c| c != id);
            }
        }
        self.collapsed.remove(&id);
    }

    /// The tree as shown: roots hidden, collapsed subtrees skipped. With a
    /// `filter`, only the components whose name contains it (any case), flat.
    pub fn rows(&self, filter: &str) -> Vec<Row> {
        self.rows_shifted(filter, 0)
    }

    /// `rows`, indented `shift` levels less: React Native trees nest a hundred
    /// deep, so the view follows the cursor sideways.
    pub fn rows_shifted(&self, filter: &str, shift: usize) -> Vec<Row> {
        let filter = filter.to_lowercase();
        let mut out = Vec::new();
        let mut stack: Vec<(i64, usize)> = Vec::new();
        for &r in self.roots.iter().rev() {
            if let Some(root) = self.nodes.get(&r) {
                for &c in root.children.iter().rev() {
                    stack.push((c, 0));
                }
            }
        }
        while let Some((id, depth)) = stack.pop() {
            let Some(n) = self.nodes.get(&id) else {
                continue;
            };
            let open = filter.is_empty() && !self.collapsed.contains(&id);
            if filter.is_empty() || n.name.to_lowercase().contains(&filter) {
                let marker = match (n.children.is_empty(), open || !filter.is_empty()) {
                    (true, _) => ' ',
                    (false, true) => '▾',
                    (false, false) => '▸',
                };
                let indent = if filter.is_empty() {
                    depth.saturating_sub(shift)
                } else {
                    0
                };
                let key = n
                    .key
                    .as_ref()
                    .map(|k| format!(" key={k:?}"))
                    .unwrap_or_default();
                // Unless the name says so already (`ForwardRef(Inner)`, `Memo(…)`).
                let badge = match n.kind {
                    TYPE_MEMO if !n.name.starts_with("Memo(") => " (memo)",
                    TYPE_FORWARD_REF if !n.name.starts_with("ForwardRef(") => " (forwardRef)",
                    _ => "",
                };
                out.push(Row {
                    id,
                    depth,
                    text: format!("{}{marker} {}{key}{badge}", " ".repeat(indent), n.name),
                });
            }
            if open || !filter.is_empty() {
                for &c in n.children.iter().rev() {
                    stack.push((c, depth + 1));
                }
            }
        }
        out
    }

    fn renderer(&self, id: i64) -> i64 {
        self.nodes.get(&id).map_or(1, |n| n.renderer)
    }

    pub fn name(&self, id: i64) -> Option<&str> {
        self.nodes.get(&id).map(|n| n.name.as_str())
    }

    /// The parent shown in the tree (not a hidden root).
    pub fn parent(&self, id: i64) -> Option<i64> {
        let p = self.nodes.get(&id)?.parent;
        (p != 0 && !self.roots.contains(&p)).then_some(p)
    }

    pub fn first_child(&self, id: i64) -> Option<i64> {
        self.nodes.get(&id)?.children.first().copied()
    }

    pub fn has_children(&self, id: i64) -> bool {
        self.nodes.get(&id).is_some_and(|n| !n.children.is_empty())
    }

    pub fn is_collapsed(&self, id: i64) -> bool {
        self.collapsed.contains(&id)
    }

    pub fn set_collapsed(&mut self, id: i64, collapsed: bool) {
        if collapsed && self.has_children(id) {
            self.collapsed.insert(id);
        } else {
            self.collapsed.remove(&id);
        }
    }

    /// Select `id` and open every collapsed ancestor so it's on screen.
    pub fn reveal(&mut self, id: i64) {
        let mut p = self.parent(id);
        while let Some(a) = p {
            self.collapsed.remove(&a);
            p = self.parent(a);
        }
        self.selected = Some(id);
    }

    /// The next `inspectElement` to send, if any: a fresh one when the
    /// selection changed, a path to hydrate, or the periodic refresh.
    pub fn next_request(&mut self) -> Option<InspectRequest> {
        let id = self.selected?;
        let renderer = self.renderer(id);
        if self.requested != Some(id) {
            self.requested = Some(id);
            self.last_request = Some(Instant::now());
            self.hydrate.clear();
            self.inspect_error = None;
            return Some(InspectRequest {
                id,
                renderer,
                path: None,
                force: true,
            });
        }
        if let Some(path) = self.hydrate.pop() {
            return Some(InspectRequest {
                id,
                renderer,
                path: Some(path),
                force: false,
            });
        }
        if self.last_request.is_none_or(|t| t.elapsed() >= POLL) {
            self.last_request = Some(Instant::now());
            return Some(InspectRequest {
                id,
                renderer,
                path: None,
                force: false,
            });
        }
        None
    }

    /// An `inspectedElement` message.
    pub fn on_inspected(&mut self, msg: &Value) {
        let id = msg["id"].as_i64();
        if id.is_none() || id != self.selected {
            return; // an answer about an element no longer selected
        }
        let id = id.unwrap();
        match msg["type"].as_str() {
            Some("full-data") => {
                let value = msg["value"].clone();
                self.hydrate = hydrate_paths(&value);
                self.inspected = Some((id, value));
                self.inspect_error = None;
            }
            Some("hydrated-path") => {
                if let (Some((_, value)), Some(path)) =
                    (self.inspected.as_mut(), msg["path"].as_array())
                {
                    merge_at(value, path, msg["value"]["data"].clone());
                }
            }
            Some("no-change") => {}
            Some("not-found") => self.inspect_error = Some("this component is gone".into()),
            Some("error") => {
                self.inspect_error = Some(
                    msg["message"]
                        .as_str()
                        .unwrap_or("couldn't inspect this component")
                        .into(),
                )
            }
            _ => {}
        }
    }

    /// The selected element and its renderer, to edit.
    pub fn edit_target(&self) -> Option<(i64, i64)> {
        let id = self.selected?;
        Some((id, self.renderer(id)))
    }

    /// What the edit prompt starts with for the prop at `path`: the path and,
    /// when it's plain data, its current value as JSON.
    pub fn edit_prefill(&self, path: &[Value]) -> String {
        let value = self
            .prop(path)
            .filter(|v| plain(v))
            .map(Value::to_string)
            .unwrap_or_default();
        format!("{} = {value}", path_text(path))
    }

    /// Remember a prop edit (its line gets marked), and re-inspect soon.
    /// False when it puts back the value the prop had before the first edit,
    /// which unmarks it.
    pub fn edited(&mut self, id: i64, path: &[Value], value: &Value) -> bool {
        self.last_request = None;
        let pos = self.edits.iter().position(|e| e.id == id && e.path == path);
        let was = match pos {
            Some(i) => self.edits[i].was.clone(),
            None => self.prop(path).cloned(),
        };
        if let Some(i) = pos {
            self.edits.remove(i);
        }
        if was.as_ref() == Some(value) {
            return false;
        }
        self.edits.push(Edit {
            id,
            path: path.to_vec(),
            was,
        });
        true
    }

    /// The selected component's prop at `path`, as last inspected.
    fn prop(&self, path: &[Value]) -> Option<&Value> {
        let props = self
            .inspected
            .as_ref()
            .filter(|(id, _)| Some(*id) == self.selected)
            .and_then(|(_, v)| section_data(&v["props"]))?;
        path.iter().try_fold(props, |v, k| match k {
            Value::Number(n) => v.get(n.as_u64()? as usize),
            Value::String(s) => v.get(s),
            _ => None,
        })
    }

    /// Where the selected component is defined in the project ("file:line"),
    /// once inspected.
    pub fn source(&self) -> Option<&str> {
        self.inspected_str("signalboxSource")
    }

    /// Where the selected component is used: the JSX line in the project
    /// that created it ("file:line").
    pub fn usage(&self) -> Option<&str> {
        self.inspected_str("signalboxUsage")
    }

    fn inspected_str(&self, field: &str) -> Option<&str> {
        let (id, v) = self.inspected.as_ref()?;
        (Some(*id) == self.selected).then(|| v[field].as_str())?
    }

    /// The detail pane for the selected component.
    pub fn detail_lines(&self) -> Vec<String> {
        self.detail().text
    }

    /// The prop shown on line `line` of the detail pane, if it shows one.
    pub fn prop_at(&self, line: usize) -> Option<Vec<Value>> {
        self.detail().props.into_iter().nth(line).flatten()
    }

    fn detail(&self) -> Lines {
        let mut out = Lines::default();
        let Some(id) = self.selected else {
            return out;
        };
        let name = self.name(id).unwrap_or("?").to_string();
        out.push(format!("<{name}>  #{id}"), None);
        if let Some(e) = &self.inspect_error {
            out.push(format!("  {e}"), None);
            return out;
        }
        let Some((_, v)) = self.inspected.as_ref().filter(|(i, _)| *i == id) else {
            out.push("  inspecting…".into(), None);
            return out;
        };
        if let Some(k) = v["key"].as_str() {
            out.push(format!("  key {k:?}"), None);
        }
        for (title, field) in [("Props", "props"), ("State", "state")] {
            if let Some(data) = section_data(&v[field]) {
                let at = (field == "props").then_some(&[][..]);
                out.push(format!("── {title} ──"), None);
                let before = out.len();
                fmt_entries(data, 1, at, &mut out);
                if out.len() == before {
                    out.push("  (none)".into(), None);
                }
            }
        }
        if let Some(hooks) = section_data(&v["hooks"]).and_then(|h| h.as_array()) {
            if !hooks.is_empty() {
                out.push("── Hooks ──".into(), None);
                fmt_hooks(hooks, 1, &mut out);
            }
        }
        if let Some(data) = section_data(&v["context"]) {
            out.push("── Context ──".into(), None);
            fmt_value(None, data, 1, None, &mut out);
        }
        for (title, field) in [("Errors", "errors"), ("Warnings", "warnings")] {
            if let Some(list) = v[field].as_array().filter(|l| !l.is_empty()) {
                out.push(format!("── {title} ──"), None);
                for e in list {
                    let n = e[1].as_i64().unwrap_or(1);
                    let times = if n > 1 {
                        format!(" (×{n})")
                    } else {
                        String::new()
                    };
                    out.push(format!("  {}{times}", e[0].as_str().unwrap_or("")), None);
                }
            }
        }
        if let Some(owners) = v["owners"].as_array().filter(|o| !o.is_empty()) {
            out.push("── Rendered by ──".into(), None);
            for o in owners {
                out.push(
                    format!("  {}", o["displayName"].as_str().unwrap_or("?")),
                    None,
                );
            }
        }
        // Only locations in the project: a bundle address can't be opened.
        if self.usage().is_some() || self.source().is_some() {
            out.push("── Source ──".into(), None);
        }
        if let Some(s) = self.usage() {
            let owner = v["owners"][0]["displayName"]
                .as_str()
                .unwrap_or("its parent");
            out.push(format!("  used in {owner} ({s})"), None);
        }
        if let Some(s) = self.source() {
            out.push(format!("  defined in {name} ({s})"), None);
        }
        self.mark_edits(id, &mut out);
        out
    }

    /// Mark the lines of edited props, with the value each had before.
    fn mark_edits(&self, id: i64, out: &mut Lines) {
        for e in self.edits.iter().filter(|e| e.id == id) {
            let Some(i) = out.props.iter().position(|p| p.as_ref() == Some(&e.path)) else {
                continue;
            };
            let was = match &e.was {
                Some(v) => placeholder(v).unwrap_or_else(|| short(v)),
                None => "unset".into(),
            };
            let line = &mut out.text[i];
            let indent = line.len() - line.trim_start().len();
            if indent >= 2 {
                line.replace_range(indent - 2..indent, "✎ ");
            }
            line.push_str(&format!("  (was {was})"));
        }
    }
}

/// The detail pane's lines, each with the prop it shows (Props lines only),
/// so the line under the cursor can be edited.
#[derive(Default)]
struct Lines {
    text: Vec<String>,
    props: Vec<Option<Vec<Value>>>,
}

impl Lines {
    fn push(&mut self, line: String, prop: Option<Vec<Value>>) {
        self.text.push(line);
        self.props.push(prop);
    }

    fn len(&self) -> usize {
        self.text.len()
    }
}

/// A value on one line, cut short.
fn short(v: &Value) -> String {
    let s = v.to_string();
    match s.char_indices().nth(40) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

/// A prop path as typed: `style.0.color`.
fn path_text(path: &[Value]) -> String {
    path.iter()
        .map(|p| match p {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Data an edit can start from: no functions, elements or other previews.
fn plain(v: &Value) -> bool {
    match v {
        Value::Object(o) => {
            !o.contains_key("inspectable") && placeholder(v).is_none() && o.values().all(plain)
        }
        Value::Array(a) => a.iter().all(plain),
        _ => true,
    }
}

/// An edit typed as `path = value`: a dotted prop path (`style.color`,
/// `items.0`; a leading `props.` is optional) and a JSON value, or else the
/// text as a string.
pub fn parse_edit(input: &str) -> Option<(Vec<Value>, Value)> {
    let (path, value) = input.split_once('=')?;
    let path = path.trim();
    let path = path.strip_prefix("props.").unwrap_or(path);
    let segments: Vec<Value> = path
        .split('.')
        .map(str::trim)
        .map(|s| match s.parse::<u64>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::from(s),
        })
        .collect();
    if segments.iter().any(|s| s.as_str() == Some("")) {
        return None;
    }
    let value = value.trim();
    let value = serde_json::from_str(value).unwrap_or_else(|_| Value::from(value));
    Some((segments, value))
}

/// A cleaned section (`{data, cleaned, unserializable}`), or the raw value
/// from older backends.
fn section_data(v: &Value) -> Option<&Value> {
    if v.is_null() {
        return None;
    }
    match v.get("data") {
        Some(d) if v.get("cleaned").is_some() => (!d.is_null()).then_some(d),
        _ => Some(v),
    }
}

/// Paths the backend left dehydrated that are worth one more request,
/// shallowest first (they're popped from the end).
fn hydrate_paths(value: &Value) -> Vec<Value> {
    let mut paths: Vec<Value> = Vec::new();
    for field in ["props", "state", "hooks", "context"] {
        let section = &value[field];
        let Some(cleaned) = section["cleaned"].as_array() else {
            continue;
        };
        for p in cleaned {
            let Some(p) = p.as_array() else { continue };
            if p.len() > 3 {
                continue;
            }
            let target = p.iter().try_fold(&section["data"], |v, k| match k {
                Value::String(s) => v.get(s),
                Value::Number(n) => n.as_u64().and_then(|i| v.get(i as usize)),
                _ => None,
            });
            if target.is_some_and(|t| t["inspectable"] == true) {
                let mut full = vec![Value::String(field.into())];
                full.extend(p.iter().cloned());
                paths.push(Value::Array(full));
            }
        }
    }
    paths.sort_by_key(|p| std::cmp::Reverse(p.as_array().map_or(0, |a| a.len())));
    let skip = paths.len().saturating_sub(MAX_HYDRATE);
    paths.drain(..skip);
    paths
}

/// Put a hydrated value back where its placeholder was. `path` starts with
/// the section (`props`, `hooks`…), whose values sit under `data`.
fn merge_at(value: &mut Value, path: &[Value], data: Value) {
    let Some((section, rest)) = path.split_first() else {
        return;
    };
    let mut cur = match section.as_str() {
        Some(s) => &mut value[s]["data"],
        None => return,
    };
    for k in rest {
        cur = match k {
            Value::String(s) => match cur.get_mut(s.as_str()) {
                Some(c) => c,
                None => return,
            },
            Value::Number(n) => match n.as_u64().and_then(|i| cur.get_mut(i as usize)) {
                Some(c) => c,
                None => return,
            },
            _ => return,
        };
    }
    *cur = data;
}

/// Keys the backend adds to a dehydrated placeholder.
const META: [&str; 8] = [
    "type",
    "inspectable",
    "name",
    "preview_short",
    "preview_long",
    "readonly",
    "size",
    "unserializable",
];

/// A placeholder for a value the backend didn't send in full: its preview.
fn placeholder(v: &Value) -> Option<String> {
    let o = v.as_object()?;
    if let Some(p) = o.get("preview_long").and_then(|p| p.as_str()) {
        // Hydrated in place (class instances keep their meta keys): show the fields.
        let fields = o.keys().any(|k| !META.contains(&k.as_str()));
        if !fields {
            return Some(p.to_string());
        }
        return None;
    }
    match o.get("type").and_then(|t| t.as_str()) {
        Some(t @ ("undefined" | "nan" | "infinity" | "symbol" | "bigint" | "date"))
            if o.len() <= 3 =>
        {
            Some(
                o.get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or(match t {
                        "nan" => "NaN",
                        "infinity" => "Infinity",
                        other => other,
                    })
                    .to_string(),
            )
        }
        _ => None,
    }
}

/// `at`: where `data` is in the props, for lines that show props.
fn fmt_entries(data: &Value, depth: usize, at: Option<&[Value]>, out: &mut Lines) {
    match data.as_object() {
        Some(o) => {
            for (k, v) in o {
                fmt_value(Some(k), v, depth, child(at, k.as_str().into()), out);
            }
        }
        None => fmt_value(None, data, depth, at.map(<[Value]>::to_vec), out),
    }
}

fn child(at: Option<&[Value]>, key: Value) -> Option<Vec<Value>> {
    at.map(|p| p.iter().cloned().chain([key]).collect())
}

/// One value as JS-ish lines: scalars inline, objects and arrays nested.
fn fmt_value(
    label: Option<&str>,
    v: &Value,
    depth: usize,
    at: Option<Vec<Value>>,
    out: &mut Lines,
) {
    let pad = "  ".repeat(depth);
    let lead = label.map(|l| format!("{l}: ")).unwrap_or_default();
    if out.len() > 2000 {
        return;
    }
    if let Some(p) = placeholder(v) {
        out.push(format!("{pad}{lead}{p}"), at);
        return;
    }
    match v {
        Value::Object(o) => {
            let fields: Vec<(&String, &Value)> = o
                .iter()
                .filter(|(k, _)| !META.contains(&k.as_str()))
                .collect();
            let tag = o
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|n| !n.is_empty() && o.contains_key("preview_long"))
                .map(|n| format!("{n} "))
                .unwrap_or_default();
            if fields.is_empty() {
                out.push(format!("{pad}{lead}{tag}{{}}"), at);
                return;
            }
            out.push(format!("{pad}{lead}{tag}{{"), at.clone());
            for (k, v) in fields {
                let here = child(at.as_deref(), k.as_str().into());
                fmt_value(Some(k), v, depth + 1, here, out);
            }
            out.push(format!("{pad}}}"), None);
        }
        Value::Array(a) if a.is_empty() => out.push(format!("{pad}{lead}[]"), at),
        Value::Array(a) => {
            out.push(format!("{pad}{lead}["), at.clone());
            for (i, item) in a.iter().enumerate() {
                fmt_value(None, item, depth + 1, child(at.as_deref(), i.into()), out);
            }
            out.push(format!("{pad}]"), None);
        }
        Value::String(s) => out.push(format!("{pad}{lead}{s:?}"), at),
        other => out.push(format!("{pad}{lead}{other}"), at),
    }
}

/// Hooks in call order; custom hooks show their built-in hooks nested.
fn fmt_hooks(hooks: &[Value], depth: usize, out: &mut Lines) {
    for h in hooks {
        let name = h["name"].as_str().unwrap_or("Hook");
        let subs = h["subHooks"].as_array().filter(|s| !s.is_empty());
        match subs {
            Some(subs) => {
                out.push(format!("{}{name}:", "  ".repeat(depth)), None);
                fmt_hooks(subs, depth + 1, out);
            }
            None => fmt_value(Some(name), &h["value"], depth, None, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An `operations` array: renderer 1, root 1, the string table, then `ops`
    /// (which name strings by their 1-based index into `strings`).
    fn ops(strings: &[&str], ops: &[i64]) -> Vec<i64> {
        let mut table = Vec::new();
        for s in strings {
            let codes: Vec<i64> = s.chars().map(|c| c as i64).collect();
            table.push(codes.len() as i64);
            table.extend(codes);
        }
        let mut out = vec![1, 1, table.len() as i64];
        out.extend(table);
        out.extend_from_slice(ops);
        out
    }

    #[test]
    fn edits_parse_as_a_path_and_a_json_value_or_text() {
        assert_eq!(
            parse_edit(r#"children = "Hi""#),
            Some((vec![json!("children")], json!("Hi")))
        );
        assert_eq!(
            parse_edit("props.style.0.color=red"),
            Some((vec![json!("style"), json!(0), json!("color")], json!("red")))
        );
        assert_eq!(
            parse_edit(r#"style = {"opacity": 0.5}"#),
            Some((vec![json!("style")], json!({"opacity": 0.5})))
        );
        assert_eq!(
            parse_edit("disabled = true"),
            Some((vec![json!("disabled")], json!(true)))
        );
        assert_eq!(
            parse_edit("title = a = b"),
            Some((vec![json!("title")], json!("a = b")))
        );
        assert_eq!(parse_edit("no equals sign"), None);
        assert_eq!(parse_edit(" = 1"), None);
        assert_eq!(parse_edit("a..b = 1"), None);
    }

    #[test]
    fn each_props_line_knows_its_prop() {
        let mut t = tree();
        t.selected = Some(3);
        t.on_inspected(&json!({"id": 3, "type": "full-data", "value": {
            "props": {"data": {
                "onPress": {"type": "function", "name": "onPress", "inspectable": false,
                    "preview_short": "onPress() {}", "preview_long": "onPress() {}"},
                "style": [{"color": "red"}, {"margin": 4}],
                "title": "Hi",
            }, "cleaned": [], "unserializable": []},
            "hooks": {"data": [{"name": "State", "value": 1, "subHooks": []}], "cleaned": [], "unserializable": []},
        }}));
        let lines = t.detail_lines();
        let at = |text: &str| {
            let i = lines.iter().position(|l| l.trim() == text).unwrap();
            t.prop_at(i).map(|p| path_text(&p))
        };
        assert_eq!(at("onPress: onPress() {}").as_deref(), Some("onPress"));
        assert_eq!(at("style: [").as_deref(), Some("style"));
        assert_eq!(at("color: \"red\"").as_deref(), Some("style.0.color"));
        assert_eq!(at("margin: 4").as_deref(), Some("style.1.margin"));
        assert_eq!(at("title: \"Hi\"").as_deref(), Some("title"));
        assert_eq!(at("]"), None);
        assert_eq!(at("── Props ──"), None);
        assert_eq!(at("State: 1"), None);
        // Prefilled with the value when it's data, else left to type.
        assert_eq!(
            t.edit_prefill(&[json!("style"), json!(0)]),
            r#"style.0 = {"color":"red"}"#
        );
        assert_eq!(
            t.edit_prefill(&[json!("style")]),
            r#"style = [{"color":"red"},{"margin":4}]"#
        );
        assert_eq!(t.edit_prefill(&[json!("onPress")]), "onPress = ");
    }

    #[test]
    fn edited_props_are_marked_with_what_they_were() {
        let mut t = tree();
        t.selected = Some(3);
        t.on_inspected(&json!({"id": 3, "type": "full-data", "value": {
            "props": {"data": {"children": "Hello", "style": [{"opacity": 1}]}, "cleaned": [], "unserializable": []}}}));
        assert_eq!(
            t.edit_prefill(&[json!("children")]),
            r#"children = "Hello""#
        );
        assert!(t.edited(3, &[json!("children")], &json!("Bye")));
        assert!(t.edited(3, &[json!("style"), json!(0), json!("opacity")], &json!(0)));
        // The app answers with the new values; the first value is kept.
        t.on_inspected(&json!({"id": 3, "type": "full-data", "value": {
            "props": {"data": {"children": "Bye", "style": [{"opacity": 0}]}, "cleaned": [], "unserializable": []}}}));
        assert!(t.edited(3, &[json!("children")], &json!("Ciao")));
        t.on_inspected(&json!({"id": 3, "type": "full-data", "value": {
            "props": {"data": {"children": "Ciao", "style": [{"opacity": 0}]}, "cleaned": [], "unserializable": []}}}));
        let lines = t.detail_lines();
        assert!(lines.contains(&r#"✎ children: "Ciao"  (was "Hello")"#.to_string()));
        assert!(lines.contains(&"    ✎ opacity: 0  (was 1)".to_string()));
        // Putting the old value back unmarks it.
        assert!(!t.edited(3, &[json!("children")], &json!("Hello")));
        t.on_inspected(&json!({"id": 3, "type": "full-data", "value": {
            "props": {"data": {"children": "Hello", "style": [{"opacity": 0}]}, "cleaned": [], "unserializable": []}}}));
        assert!(t
            .detail_lines()
            .contains(&r#"  children: "Hello""#.to_string()));
    }

    /// Root 1 › App 2 › (Header 3, List 4 › Item key="a" 5, Item key="b" 6)
    fn tree() -> Tree {
        let mut t = Tree::default();
        let o = ops(
            &["App", "Header", "List", "Item", "a", "b"],
            &[
                OP_ADD, 1, TYPE_ROOT, 0, 0, 1, 1, //
                OP_ADD, 2, 5, 1, 0, 1, 0, //
                OP_ADD, 3, 5, 2, 2, 2, 0, //
                OP_ADD, 4, TYPE_MEMO, 2, 2, 3, 0, //
                OP_ADD, 5, 5, 4, 4, 4, 5, //
                OP_ADD, 6, 5, 4, 4, 4, 6, //
            ],
        );
        t.apply(&o).unwrap();
        t
    }

    fn texts(t: &Tree, filter: &str) -> Vec<String> {
        t.rows(filter).into_iter().map(|r| r.text).collect()
    }

    #[test]
    fn decodes_the_tree_without_its_root() {
        let t = tree();
        assert_eq!(
            texts(&t, ""),
            vec![
                "▾ App",
                "   Header",
                " ▾ List (memo)",
                "    Item key=\"a\"",
                "    Item key=\"b\"",
            ]
        );
        assert_eq!(t.parent(2), None);
        assert_eq!(t.parent(5), Some(4));
        assert_eq!(t.first_child(4), Some(5));
    }

    #[test]
    fn removes_reorders_and_decodes_unicode_names() {
        let mut t = tree();
        t.apply(&ops(&[], &[OP_REORDER, 4, 2, 6, 5, OP_REMOVE, 1, 3]))
            .unwrap();
        assert_eq!(
            texts(&t, ""),
            vec![
                "▾ App",
                " ▾ List (memo)",
                "    Item key=\"b\"",
                "    Item key=\"a\""
            ]
        );
        t.apply(&ops(&["Ünïcode 😀"], &[OP_ADD, 7, 5, 2, 2, 1, 0]))
            .unwrap();
        assert!(texts(&t, "").contains(&"   Ünïcode 😀".to_string()));
        // Ops that only carry numbers are skipped over.
        t.apply(&ops(
            &[],
            &[
                OP_BASE_DURATION,
                2,
                10,
                OP_ERRORS_WARNINGS,
                2,
                1,
                0,
                OP_SUBTREE_MODE,
                2,
                1,
            ],
        ))
        .unwrap();
        assert_eq!(
            t.apply(&ops(&[], &[99])).unwrap_err(),
            "unknown tree operation 99"
        );
    }

    #[test]
    fn removing_the_root_empties_the_tree() {
        let mut t = tree();
        t.selected = Some(5);
        t.apply(&ops(&[], &[OP_REMOVE, 5, 6, 5, 4, 3, 2, OP_REMOVE_ROOT]))
            .unwrap();
        assert!(t.rows("").is_empty() && t.is_empty());
        assert_eq!(t.selected, None);
    }

    #[test]
    fn collapses_filters_and_reveals() {
        let mut t = tree();
        t.set_collapsed(4, true);
        assert_eq!(texts(&t, ""), vec!["▾ App", "   Header", " ▸ List (memo)"]);
        t.set_collapsed(3, true); // a leaf can't collapse
        assert!(!t.is_collapsed(3));
        // A filter searches the whole tree, collapsed or not, flat.
        assert_eq!(
            texts(&t, "item"),
            vec!["  Item key=\"a\"", "  Item key=\"b\""]
        );
        t.set_collapsed(2, true);
        t.reveal(6);
        assert_eq!(t.selected, Some(6));
        assert_eq!(texts(&t, "").len(), 5);
    }

    #[test]
    fn asks_once_per_selection_then_hydrates_then_polls() {
        let mut t = tree();
        assert!(t.next_request().is_none());
        t.selected = Some(5);
        let r = t.next_request().unwrap();
        assert_eq!(
            (r.id, r.renderer, r.force, r.path.is_none()),
            (5, 1, true, true)
        );
        assert!(t.next_request().is_none(), "polls only after a second");
        t.on_inspected(&json!({"id": 5, "type": "full-data", "value": {
            "props": {"data": {
                "style": {"type": "object", "inspectable": true, "preview_long": "{flex: 1}", "preview_short": "{…}", "name": ""},
                "onPress": {"type": "function", "inspectable": false, "preview_long": "onPress() {}", "preview_short": "onPress() {}", "name": "onPress"},
            }, "cleaned": [["style"], ["onPress"]], "unserializable": []},
        }}));
        let r = t.next_request().unwrap();
        assert_eq!(r.path, Some(json!(["props", "style"])));
        assert!(!r.force);
        t.on_inspected(
            &json!({"id": 5, "type": "hydrated-path", "path": ["props", "style"],
            "value": {"data": {"flex": 1}, "cleaned": [], "unserializable": []}}),
        );
        assert!(t.detail_lines().contains(&"    flex: 1".to_string()));
        // A new selection starts over.
        t.selected = Some(6);
        assert!(t.next_request().unwrap().force);
    }

    #[test]
    fn ignores_answers_about_other_components() {
        let mut t = tree();
        t.selected = Some(5);
        t.next_request();
        t.on_inspected(&json!({"id": 6, "type": "full-data", "value": {"props": {"data": {"x": 1}, "cleaned": []}}}));
        assert_eq!(t.detail_lines(), vec!["<Item>  #5", "  inspecting…"]);
        t.on_inspected(&json!({"id": 5, "type": "not-found"}));
        assert_eq!(
            t.detail_lines(),
            vec!["<Item>  #5", "  this component is gone"]
        );
    }

    #[test]
    fn formats_props_state_hooks_owners_and_source() {
        let mut t = tree();
        t.selected = Some(5);
        t.next_request();
        t.on_inspected(&json!({"id": 5, "type": "full-data", "value": {
            "key": "a",
            "props": {"data": {
                "title": "Hi",
                "count": 3,
                "tags": ["x", null],
                "empty": {},
                "onPress": {"type": "function", "inspectable": false, "preview_long": "onPress() {}", "preview_short": "onPress() {}", "name": "onPress"},
                "missing": {"type": "undefined"},
                "agent": {"type": "class_instance", "inspectable": true, "unserializable": true, "readonly": true,
                          "preview_long": "Agent", "preview_short": "Agent", "name": "Agent", "_busy": false},
            }, "cleaned": [], "unserializable": [["agent"]]},
            "state": null,
            "hooks": {"data": [
                {"id": 0, "name": "State", "value": true, "subHooks": []},
                {"id": null, "name": "Navigation", "value": {"type": "undefined"}, "subHooks": [
                    {"id": 1, "name": "Context", "value": {"type": "object", "inspectable": true, "preview_long": "{route: {…}}", "preview_short": "{…}", "name": ""}, "subHooks": []},
                ]},
            ], "cleaned": [], "unserializable": []},
            "context": null,
            "errors": [["Each child should have a key", 2]],
            "warnings": [],
            "owners": [{"displayName": "List", "id": 4}, {"displayName": "App", "id": 2}],
            "source": {"sourceURL": "http://localhost:8081/index.bundle", "line": 120, "column": 4},
            "signalboxSource": "/app/src/Item.tsx:12",
            "signalboxUsage": "/app/src/List.tsx:30",
        }}));
        assert_eq!(
            t.detail_lines(),
            vec![
                "<Item>  #5",
                "  key \"a\"",
                // Alphabetical, as React DevTools lists them.
                "── Props ──",
                "  agent: Agent {",
                "    _busy: false",
                "  }",
                "  count: 3",
                "  empty: {}",
                "  missing: undefined",
                "  onPress: onPress() {}",
                "  tags: [",
                "    \"x\"",
                "    null",
                "  ]",
                "  title: \"Hi\"",
                "── Hooks ──",
                "  State: true",
                "  Navigation:",
                "    Context: {route: {…}}",
                "── Errors ──",
                "  Each child should have a key (×2)",
                "── Rendered by ──",
                "  List",
                "  App",
                "── Source ──",
                "  used in List (/app/src/List.tsx:30)",
                "  defined in Item (/app/src/Item.tsx:12)",
            ]
        );
    }

    #[test]
    fn shows_a_source_only_inside_the_project() {
        let mut t = tree();
        t.selected = Some(5);
        t.next_request();
        t.on_inspected(&json!({"id": 5, "type": "full-data", "value": {
            "source": {"sourceURL": "address at http://localhost:8081/index.bundle", "line": 1, "column": 9},
        }}));
        assert!(!t.detail_lines().iter().any(|l| l.contains("Source")));
    }

    #[test]
    fn reset_keeps_the_attachment() {
        let mut t = tree();
        t.attached = true;
        t.selected = Some(5);
        t.reset();
        assert!(t.attached && t.is_empty() && t.selected.is_none());
    }
}
