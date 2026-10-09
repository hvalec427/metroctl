//! The React Native log viewer as an embeddable component. Holds the per-device
//! Logs / Network / Perf state, reduces `RnEvent`s from `RnClient`, and renders
//! itself into an arbitrary `Rect` (the full screen for `logs --rn`, a pane in
//! `metroctl`). Extracted from the old `rntui` so both share one implementation.

use crate::rnclient::{format_js, ConnCmd, FRAMEWORK_FRAME, LogEntry, NetRecord, PausedInfo, RnClient, RnEvent, Status, TargetInfo};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::collections::HashMap;
use std::process::{Command, Stdio};

const MAX_LINES: usize = 5000;
const MAX_NET: usize = 1000;
const PERF_HISTORY: usize = 60;
const METHODS: [&str; 8] = ["ALL", "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

// ── pure helpers ─────────────────────────────────────────────────────────────

pub fn graphql_operation(rec: &NetRecord) -> Option<String> {
    let body = rec.req_body.as_ref()?;
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    fn pick(o: &serde_json::Value) -> Option<String> {
        if let Some(n) = o.get("operationName").and_then(|n| n.as_str()) {
            return Some(n.to_string());
        }
        if let Some(q) = o.get("query").and_then(|q| q.as_str()) {
            // first `query|mutation|subscription <Name>`
            for kw in ["query", "mutation", "subscription"] {
                if let Some(idx) = q.find(kw) {
                    let rest = q[idx + kw.len()..].trim_start();
                    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
            return Some("anonymous".into());
        }
        None
    }
    if let Some(arr) = v.as_array() {
        let ops: Vec<String> = arr.iter().filter_map(pick).collect();
        return if ops.is_empty() { None } else { Some(ops.join(", ")) };
    }
    pick(&v)
}

fn fmt_duration(ms: Option<f64>) -> String {
    match ms {
        None => String::new(),
        Some(ms) if ms >= 1000.0 => format!("{:.1}s", ms / 1000.0),
        Some(ms) => format!("{}ms", ms.round() as i64),
    }
}

fn human_bytes(n: i64) -> String {
    let f = n as f64;
    if f >= 1_048_576.0 {
        format!("{:.1} MB", f / 1_048_576.0)
    } else if f >= 1024.0 {
        format!("{:.1} KB", f / 1024.0)
    } else {
        format!("{n} B")
    }
}

pub fn net_summary(rec: &NetRecord) -> String {
    let status = if rec.failed.is_some() {
        "FAIL".into()
    } else if rec.is_ws {
        "WS".into()
    } else {
        rec.status.map(|s| s.to_string()).unwrap_or_else(|| "···".into())
    };
    let dur = fmt_duration(rec.duration_ms);
    let op = graphql_operation(rec);
    let mut s = format!("{status}  {} {}", rec.method, rec.url);
    if !dur.is_empty() {
        s.push_str(&format!("  {dur}"));
    }
    if let Some(op) = op {
        s.push_str(&format!("  {op}"));
    }
    if rec.is_ws && !rec.ws_frames.is_empty() {
        s.push_str(&format!("  {} frames", rec.ws_frames.len()));
    }
    if let Some(sz) = rec.size {
        s.push_str(&format!("  {}", human_bytes(sz)));
    }
    s
}

fn net_error(rec: &NetRecord) -> bool {
    rec.failed.is_some() || rec.status.map(|s| s >= 400).unwrap_or(false)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn to_curl(rec: &NetRecord) -> String {
    let method = rec.method.to_uppercase();
    let head = method == "HEAD";
    let mut parts = vec![if head { format!("curl -I {}", sh_quote(&rec.url)) } else { format!("curl -X {method} {}", sh_quote(&rec.url)) }];
    for (k, v) in &rec.req_headers {
        parts.push(format!("-H {}", sh_quote(&format!("{k}: {v}"))));
    }
    if let (Some(b), false) = (&rec.req_body, head) {
        parts.push(format!("--data {}", sh_quote(b)));
    }
    parts.join(" \\\n  ")
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}… ({} more chars)", &s[..i], s[i..].chars().count()),
        None => s.to_string(),
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n', '\t'], " ")
}

fn pretty_or_raw(body: &str) -> Vec<String> {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| body.to_string()).lines().map(|l| format!("  {l}")).collect(),
        Err(_) => body.lines().map(|l| format!("  {l}")).collect(),
    }
}

fn log_detail_lines(e: &LogEntry, show_framework: bool) -> Vec<String> {
    let mut out = if let Some(tree) = &e.expanded {
        let mut out = vec![e.text.clone(), String::new()];
        out.extend(tree.clone());
        out
    } else {
        match serde_json::from_str::<serde_json::Value>(&e.text) {
            Ok(v) => format_js(&v, "").lines().map(String::from).collect(),
            Err(_) => e.text.lines().map(String::from).collect(),
        }
    };
    if let Some(stack) = &e.stack {
        out.push(String::new());
        out.push("── Stack ──".into());
        let hidden = stack.iter().filter(|l| l.starts_with(FRAMEWORK_FRAME)).count();
        out.extend(stack.iter().filter(|l| show_framework || !l.starts_with(FRAMEWORK_FRAME)).cloned());
        if hidden > 0 && !show_framework {
            out.push(format!("  ({hidden} framework frames hidden — F to show)"));
        }
    }
    out
}

fn net_detail_lines(rec: &NetRecord) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("{} {}", rec.method, rec.url));
    let mut status = format!("Status: {}", rec.status.map(|s| s.to_string()).unwrap_or_else(|| "(pending)".into()));
    if let Some(d) = rec.duration_ms {
        status.push_str(&format!("  ·  {}", fmt_duration(Some(d))));
    }
    if let Some(m) = &rec.mime_type {
        status.push_str(&format!("  ·  {m}"));
    }
    if let Some(sz) = rec.size {
        status.push_str(&format!("  ·  {}", human_bytes(sz)));
    }
    out.push(status);
    if let Some(e) = &rec.failed {
        out.push(format!("Error: {e}"));
    }
    if let Some(op) = graphql_operation(rec) {
        out.push(format!("GraphQL: {op}"));
    }
    // WebSocket: show the frame log instead of request/response bodies.
    if rec.is_ws {
        out.push(String::new());
        out.push(format!("── Frames ({}) ──", rec.ws_frames.len()));
        if rec.ws_frames.is_empty() {
            out.push("  (none yet)".into());
        }
        for (sent, payload) in &rec.ws_frames {
            let arrow = if *sent { "↑" } else { "↓" };
            for (i, line) in payload.lines().enumerate() {
                out.push(if i == 0 { format!("  {arrow} {line}") } else { format!("    {line}") });
            }
        }
        return out;
    }
    let section = |out: &mut Vec<String>, title: &str, headers: &[(String, String)]| {
        out.push(String::new());
        out.push(format!("── {title} ──"));
        if headers.is_empty() {
            out.push("  (none)".into());
        } else {
            for (k, v) in headers {
                out.push(format!("  {k}: {v}"));
            }
        }
    };
    let body = |out: &mut Vec<String>, title: &str, b: &Option<String>| {
        out.push(String::new());
        out.push(format!("── {title} ──"));
        match b {
            None => out.push("  (not captured)".into()),
            Some(s) if s.is_empty() => out.push("  (empty)".into()),
            Some(s) => out.extend(pretty_or_raw(s)),
        }
    };
    section(&mut out, "Request headers", &rec.req_headers);
    body(&mut out, "Request body", &rec.req_body);
    section(&mut out, "Response headers", &rec.res_headers);
    body(&mut out, "Response body", &rec.res_body);
    out
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let s = one_line(s);
    if s.chars().count() <= width || width == 0 {
        return vec![s];
    }
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

// ── per-device state ─────────────────────────────────────────────────────────

#[derive(Default)]
struct PerfState {
    fps: Option<i64>,
    heap_used: Option<i64>,
    heap_total: Option<i64>,
    history: Vec<i64>,
}

struct Device {
    logs: Vec<LogEntry>,
    net: Vec<NetRecord>,
    status: Status,
    network_supported: bool,
    was_disconnected: bool,
    has_context: bool, // seen this connection's first executionContextCreated
    perf: PerfState,
    /// `Some` while the JS VM is suspended at a breakpoint/step on this device.
    paused: Option<PausedInfo>,
    // Totals ever added, so entries keep stable sequence numbers (for `since`
    // over the control socket) across trimming and clears: logs[i] has seq
    // `log_seq - logs.len() + i`.
    log_seq: u64,
    net_seq: u64,
}

impl Default for Device {
    fn default() -> Self {
        Device { logs: Vec::new(), net: Vec::new(), status: Status::Connecting, network_supported: true, was_disconnected: false, has_context: false, perf: PerfState::default(), paused: None, log_seq: 0, net_seq: 0 }
    }
}

impl Device {
    fn clear(&mut self) {
        self.logs.clear();
        self.net.clear();
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Logs,
    Network,
    Perf,
}

#[derive(PartialEq)]
enum Mode {
    Normal,
    Search,
    Filter,
}

pub struct RnView {
    devices: HashMap<String, Device>,
    targets: Vec<TargetInfo>,
    active: Option<String>,
    name_filter: Option<String>,
    tab: Tab,
    sel: [usize; 3],
    follow: bool,
    detail: bool,
    maximized: bool,
    opened_by_max: bool,
    detail_scroll: usize,
    detail_hit: Option<usize>, // active match line in the pane (for n/N + current highlight)
    detail_width: usize,       // pane width as last rendered (so n/N wrap matches)
    view_h: usize,             // body height as last rendered
    search: String,
    filter: String,
    mode: Mode,
    input: String,
    clear_on_restart: bool,
    errors_only: bool,
    show_framework: bool, // include collapsed React/RN frames in stacks
    method_idx: usize,
    flash: Option<String>,
    focused: bool, // when embedded: dim the footer unless the logs pane is active
    embedded: bool, // inside the dashboard, whose status bar already lists R/q
    // Vim-style linewise visual selection. The anchor is a list-row index when
    // the list is active, or a wrapped-preview-line index when maximized.
    visual: Option<usize>,
    detail_cursor: usize, // current line in the maximized preview
    root: Option<String>, // project root, to resolve relative stack-frame paths
}

impl RnView {
    pub fn new(name: Option<&str>) -> RnView {
        RnView {
            devices: HashMap::new(),
            targets: Vec::new(),
            active: None,
            name_filter: name.map(|s| s.to_lowercase()),
            tab: Tab::Logs,
            sel: [0, 0, 0],
            follow: true,
            detail: false,
            maximized: false,
            opened_by_max: false,
            detail_scroll: 0,
            detail_hit: None,
            detail_width: 80,
            view_h: 20,
            search: String::new(),
            filter: String::new(),
            mode: Mode::Normal,
            input: String::new(),
            clear_on_restart: false,
            errors_only: false,
            show_framework: false,
            method_idx: 0,
            flash: None,
            focused: true,
            embedded: false,
            visual: None,
            detail_cursor: 0,
            root: None,
        }
    }

    /// Project root used to resolve relative stack-frame paths when opening a
    /// frame in nvim (`o`).
    pub fn set_root(&mut self, root: Option<String>) {
        self.root = root;
    }

    fn eff_sel(&self, len: usize) -> usize {
        if self.follow {
            len.saturating_sub(1)
        } else {
            self.sel[self.tab_idx()].min(len.saturating_sub(1))
        }
    }

    /// When embedded in the dashboard, controls whether the footer is shown
    /// bright (active pane) or blank (another pane has focus). Standalone
    /// `logs --rn` leaves this true.
    pub fn set_embedded(&mut self, embedded: bool) {
        self.embedded = embedded;
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// True while capturing a search/filter query — the embedding dashboard must
    /// route every key here instead of acting on its own shortcuts.
    pub fn is_capturing_input(&self) -> bool {
        self.mode != Mode::Normal
    }

    /// The device key currently in focus (for a CDP reload fallback).
    pub fn active_target(&self) -> Option<String> {
        self.active.clone()
    }

    /// A one-line paused banner for the active device when its VM is suspended,
    /// so the embedding dashboard can surface it in its own status bar.
    pub fn active_paused(&self) -> Option<String> {
        let d = self.devices.get(self.active.as_ref()?)?;
        let p = d.paused.as_ref()?;
        Some(format!("⏸ PAUSED {}:{} ({})", p.file, p.line, p.reason))
    }

    /// The device a control-socket query is about: the first whose key or label
    /// contains `name`, else the active one.
    fn query_device(&self, name: Option<&str>) -> Option<(&str, &Device)> {
        let key = match name.map(str::to_lowercase) {
            Some(n) => self.targets.iter().find(|t| t.key.to_lowercase().contains(&n) || t.label.to_lowercase().contains(&n)).map(|t| t.key.as_str()),
            None => self.active.as_deref(),
        }?;
        self.devices.get_key_value(key).map(|(k, d)| (k.as_str(), d))
    }

    /// Connected apps, for the control socket's `status`.
    pub fn apps_json(&self) -> serde_json::Value {
        let apps: Vec<serde_json::Value> = self
            .targets
            .iter()
            .map(|t| {
                let d = self.devices.get(&t.key);
                serde_json::json!({
                    "device": t.label,
                    "status": format!("{:?}", d.map(|d| d.status.clone()).unwrap_or(Status::Connecting)).to_lowercase(),
                    "paused": d.and_then(|d| d.paused.as_ref()).map(|p| format!("{}:{} ({})", p.file, p.line, p.reason)),
                    "logs": d.map_or(0, |d| d.logs.len()),
                    "requests": d.map_or(0, |d| d.net.len()),
                })
            })
            .collect();
        serde_json::Value::Array(apps)
    }

    /// JS console entries from `since` (a seq), newest `limit` that match.
    /// `level`: "error" = errors only, "warn" = warnings and errors.
    pub fn logs_json(&self, device: Option<&str>, level: Option<&str>, filter: Option<&str>, since: u64, limit: usize) -> serde_json::Value {
        let Some((key, d)) = self.query_device(device) else {
            return serde_json::json!({ "error": "no app connected to Metro" });
        };
        let filter = filter.map(str::to_lowercase);
        let first = d.log_seq - d.logs.len() as u64;
        let mut hits: Vec<serde_json::Value> = d
            .logs
            .iter()
            .enumerate()
            .map(|(i, e)| (first + i as u64, e))
            .filter(|(seq, e)| {
                *seq >= since
                    && match level {
                        Some("error") => e.level == "error",
                        Some("warn") => e.level == "error" || e.level == "warn" || e.level == "warning",
                        _ => true,
                    }
                    && filter.as_ref().is_none_or(|f| e.text.to_lowercase().contains(f))
            })
            .map(|(seq, e)| {
                let mut v = serde_json::json!({ "seq": seq, "level": e.level, "text": truncate(&e.text, 4000) });
                // Logged objects arrive as a preview ("Object"); include the fetched tree.
                if let Some(tree) = &e.expanded {
                    let mut lines: Vec<String> = tree.iter().take(60).map(|l| truncate(l, 500)).collect();
                    if tree.len() > 60 {
                        lines.push(format!("… {} more lines", tree.len() - 60));
                    }
                    v["details"] = serde_json::json!(lines);
                }
                if let Some(stack) = &e.stack {
                    // Project-relative paths: shorter, and what an agent edits.
                    let prefix = self.root.as_ref().map(|r| format!("{}/", r.trim_end_matches('/')));
                    let own: Vec<String> = stack
                        .iter()
                        .filter(|l| !l.starts_with(FRAMEWORK_FRAME))
                        .take(15)
                        .map(|l| match &prefix {
                            Some(p) => l.replace(p.as_str(), ""),
                            None => l.clone(),
                        })
                        .collect();
                    v["stack"] = serde_json::json!(own);
                }
                v
            })
            .collect();
        let total = hits.len();
        hits.drain(..total.saturating_sub(limit));
        serde_json::json!({ "device": key, "next": d.log_seq, "matched": total, "entries": hits })
    }

    /// Network requests from `since` (a seq), newest `limit` that match.
    pub fn net_json(&self, device: Option<&str>, failed_only: bool, filter: Option<&str>, since: u64, limit: usize) -> serde_json::Value {
        let Some((key, d)) = self.query_device(device) else {
            return serde_json::json!({ "error": "no app connected to Metro" });
        };
        let filter = filter.map(str::to_lowercase);
        let first = d.net_seq - d.net.len() as u64;
        let mut hits: Vec<serde_json::Value> = d
            .net
            .iter()
            .enumerate()
            .map(|(i, r)| (first + i as u64, r))
            .filter(|(seq, r)| {
                *seq >= since
                    && (!failed_only || net_error(r))
                    && filter.as_ref().is_none_or(|f| r.url.to_lowercase().contains(f) || graphql_operation(r).is_some_and(|o| o.to_lowercase().contains(f)))
            })
            .map(|(seq, r)| {
                serde_json::json!({
                    "seq": seq,
                    "id": r.id,
                    "method": r.method,
                    "url": r.url,
                    "status": r.status,
                    "failed": r.failed,
                    "ms": r.duration_ms.map(|d| d.round() as i64),
                    "graphql": graphql_operation(r),
                })
            })
            .collect();
        let total = hits.len();
        hits.drain(..total.saturating_sub(limit));
        serde_json::json!({ "device": key, "next": d.net_seq, "matched": total, "requests": hits })
    }

    /// One request in full (headers and bodies), as text.
    pub fn request_text(&self, device: Option<&str>, id: &str) -> Option<String> {
        let (_, d) = self.query_device(device)?;
        let r = d.net.iter().find(|r| r.id == id)?;
        Some(net_detail_lines(r).iter().map(|l| truncate(l, 20_000)).collect::<Vec<_>>().join("\n"))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        render_view(self, frame, area);
    }

    /// Returns true to quit.
    pub fn on_key(&mut self, key: KeyEvent, client: &RnClient) -> bool {
        handle_key(self, key, client)
    }

    fn tab_idx(&self) -> usize {
        match self.tab {
            Tab::Logs => 0,
            Tab::Network => 1,
            Tab::Perf => 2,
        }
    }

    fn dev(&mut self, key: &str) -> &mut Device {
        self.devices.entry(key.to_string()).or_default()
    }

    pub fn on_event(&mut self, ev: RnEvent) {
        match ev {
            RnEvent::Targets(list) => {
                for t in &list {
                    self.devices.entry(t.key.clone()).or_default();
                }
                if self.active.is_none() && !list.is_empty() {
                    let matched = self.name_filter.as_ref().and_then(|n| list.iter().find(|t| t.key.to_lowercase().contains(n)));
                    self.active = Some(matched.map(|t| t.key.clone()).unwrap_or_else(|| list[0].key.clone()));
                }
                self.targets = list;
            }
            RnEvent::Status(key, s) => {
                let clear = self.clear_on_restart;
                let d = self.dev(&key);
                if s == Status::Disconnected {
                    d.was_disconnected = true;
                    d.paused = None; // a gone VM can't still be suspended
                }
                if s == Status::Connected {
                    if d.was_disconnected && clear {
                        d.clear();
                    }
                    d.was_disconnected = false;
                    d.has_context = false;
                }
                d.status = s;
            }
            RnEvent::Log(key, e) => {
                let d = self.dev(&key);
                d.logs.push(e);
                d.log_seq += 1;
                if d.logs.len() > MAX_LINES {
                    d.logs.remove(0);
                }
            }
            RnEvent::Net(key, rec) => {
                let d = self.dev(&key);
                if let Some(existing) = d.net.iter_mut().find(|r| r.id == rec.id) {
                    *existing = rec;
                } else {
                    d.net.push(rec);
                    d.net_seq += 1;
                    if d.net.len() > MAX_NET {
                        d.net.remove(0);
                    }
                }
                d.network_supported = true;
            }
            RnEvent::Network(key, ok) => {
                self.dev(&key).network_supported = ok;
            }
            RnEvent::Paused(key, info) => {
                self.dev(&key).paused = Some(info);
            }
            RnEvent::Resumed(key) => {
                self.dev(&key).paused = None;
            }
            // A reload (Metro `r`, ⇧R, dev menu) swaps the JS context: RN sends
            // contextsCleared, and a fresh executionContextCreated after the
            // first one on a connection means the same. Fast Refresh keeps the
            // context, so it doesn't clear.
            RnEvent::ContextCleared(key) => {
                if self.clear_on_restart {
                    self.dev(&key).clear();
                }
            }
            RnEvent::ContextCreated(key) => {
                let clear = self.clear_on_restart;
                let d = self.dev(&key);
                if d.has_context && clear {
                    d.clear();
                }
                d.has_context = true;
            }
            RnEvent::Perf(key, s) => {
                let d = self.dev(&key);
                if let Some(f) = s.fps {
                    d.perf.history.push(f);
                    if d.perf.history.len() > PERF_HISTORY {
                        d.perf.history.remove(0);
                    }
                }
                d.perf = PerfState { fps: s.fps, heap_used: s.heap_used, heap_total: s.heap_total, history: d.perf.history.clone() };
            }
        }
    }

    // Visible items for the current tab/device as display strings + error flag.
    fn rows(&self) -> Vec<(String, bool, String)> {
        let d = match self.active.as_ref().and_then(|k| self.devices.get(k)) {
            Some(d) => d,
            None => return Vec::new(),
        };
        let f = self.filter.to_lowercase();
        match self.tab {
            Tab::Logs => d
                .logs
                .iter()
                .filter(|e| f.is_empty() || e.text.to_lowercase().contains(&f))
                .map(|e| (one_line(&e.text), e.level == "error" || e.level == "assert", e.level.clone()))
                .collect(),
            Tab::Network => {
                let method = METHODS[self.method_idx];
                d.net
                    .iter()
                    .filter(|r| {
                        let text = net_summary(r).to_lowercase();
                        (f.is_empty() || text.contains(&f))
                            && (!self.errors_only || net_error(r))
                            && (method == "ALL" || r.method.to_uppercase() == method)
                    })
                    .map(|r| (net_summary(r), net_error(r), "net".into()))
                    .collect()
            }
            Tab::Perf => Vec::new(),
        }
    }

    fn detail_wrapped(&self) -> Vec<String> {
        self.detail_source().iter().flat_map(|l| wrap(l, self.detail_width.max(1))).collect()
    }

    fn detail_source(&self) -> Vec<String> {
        self.row_detail(self.eff_sel(self.rows().len()))
    }

    /// Full detail lines for the row at `idx` in the current tab's filtered list.
    fn row_detail(&self, idx: usize) -> Vec<String> {
        let d = match self.active.as_ref().and_then(|k| self.devices.get(k)) {
            Some(d) => d,
            None => return Vec::new(),
        };
        match self.tab {
            Tab::Logs => {
                let list: Vec<&LogEntry> = d.logs.iter().filter(|e| self.filter.is_empty() || e.text.to_lowercase().contains(&self.filter.to_lowercase())).collect();
                list.get(idx).map(|e| log_detail_lines(e, self.show_framework)).unwrap_or_default()
            }
            Tab::Network => {
                let method = METHODS[self.method_idx];
                let f = self.filter.to_lowercase();
                let list: Vec<&NetRecord> = d
                    .net
                    .iter()
                    .filter(|r| (f.is_empty() || net_summary(r).to_lowercase().contains(&f)) && (!self.errors_only || net_error(r)) && (method == "ALL" || r.method.to_uppercase() == method))
                    .collect();
                list.get(idx).map(|r| net_detail_lines(r)).unwrap_or_default()
            }
            Tab::Perf => Vec::new(),
        }
    }
}

// ── rendering ────────────────────────────────────────────────────────────────

fn bar_style() -> Style {
    Style::default().bg(Color::Rgb(59, 66, 82)).fg(Color::White)
}

fn level_color(level: &str) -> Style {
    match level {
        "error" | "assert" => Style::default().fg(Color::Red),
        "warning" | "warn" => Style::default().fg(Color::Yellow),
        "info" => Style::default().fg(Color::Cyan),
        "debug" | "verbose" => Style::default().fg(Color::DarkGray),
        "net" => Style::default().fg(Color::White),
        _ => Style::default().fg(Color::White),
    }
}

fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n > width {
        s.chars().take(width).collect()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// Background for rows/lines inside a visual selection range.
fn sel_style() -> Style {
    Style::default().bg(Color::Rgb(38, 79, 120)).fg(Color::White)
}

fn in_range(range: Option<(usize, usize)>, i: usize) -> bool {
    range.is_some_and(|(lo, hi)| i >= lo && i <= hi)
}

/// Build a styled line, highlighting search matches (current row → yellow,
/// others → cyan).
fn row_line(text: &str, width: usize, base: Style, search: &str, current: bool) -> Line<'static> {
    let padded = pad(&one_line(text), width);
    if search.is_empty() {
        return Line::from(Span::styled(padded, base));
    }
    let hl = if current {
        Style::default().bg(Color::LightYellow).fg(Color::Black)
    } else {
        Style::default().bg(Color::LightCyan).fg(Color::Black)
    };
    let lower = padded.to_lowercase();
    let needle = search.to_lowercase();
    let mut spans: Vec<Span> = Vec::new();
    let mut i = 0;
    let bytes: Vec<char> = padded.chars().collect();
    let lchars: Vec<char> = lower.chars().collect();
    let nchars: Vec<char> = needle.chars().collect();
    while i < bytes.len() {
        if !nchars.is_empty() && i + nchars.len() <= lchars.len() && lchars[i..i + nchars.len()] == nchars[..] {
            let m: String = bytes[i..i + nchars.len()].iter().collect();
            spans.push(Span::styled(m, hl));
            i += nchars.len();
        } else {
            // accumulate a run of non-match
            let start = i;
            i += 1;
            while i < bytes.len() && !(i + nchars.len() <= lchars.len() && !nchars.is_empty() && lchars[i..i + nchars.len()] == nchars[..]) {
                i += 1;
            }
            let run: String = bytes[start..i].iter().collect();
            spans.push(Span::styled(run, base));
        }
    }
    Line::from(spans)
}

fn perf_lines(view: &RnView, h: usize) -> Vec<Line<'static>> {
    let d = view.active.as_ref().and_then(|k| view.devices.get(k));
    let mut out: Vec<Line> = vec![Line::raw(""), Line::raw("  JS thread")];
    match d.and_then(|d| d.perf.fps) {
        None => out.push(Line::styled("    FPS  — (measuring…)", Style::default().fg(Color::DarkGray))),
        Some(fps) => {
            let width = 30usize;
            let filled = ((fps.min(60) as f64 / 60.0) * width as f64).round() as usize;
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(width.saturating_sub(filled)));
            let color = if fps >= 50 { Color::Green } else if fps >= 30 { Color::Yellow } else { Color::Red };
            out.push(Line::styled(format!("    FPS  {fps:>2} / 60   {bar}"), Style::default().fg(color)));
        }
    }
    if let Some(d) = d {
        if !d.perf.history.is_empty() {
            let spark = "▁▂▃▄▅▆▇█";
            let chars: Vec<char> = spark.chars().collect();
            let s: String = d.perf.history.iter().map(|v| chars[((*v as f64 / 60.0) * 7.0).round().clamp(0.0, 7.0) as usize]).collect();
            out.push(Line::raw(format!("    last {}s  {s}", d.perf.history.len())));
        }
        out.push(Line::raw(""));
        out.push(Line::raw("  Memory (JS heap)"));
        match d.perf.heap_used {
            Some(u) => {
                let used = format!("{:.1} MB", u as f64 / 1048576.0);
                let total = d.perf.heap_total.map(|t| format!(" / {:.1} MB", t as f64 / 1048576.0)).unwrap_or_default();
                out.push(Line::raw(format!("    used {used}{total}")));
            }
            None => out.push(Line::styled("    not reported by this runtime", Style::default().fg(Color::DarkGray))),
        }
    }
    out.push(Line::raw(""));
    out.push(Line::styled("  JS-thread FPS = how fast JS services frames (≤60). Native/UI FPS isn’t exposed over CDP.", Style::default().fg(Color::DarkGray)));
    while out.len() < h {
        out.push(Line::raw(""));
    }
    out.truncate(h);
    out
}

fn render_view(view: &mut RnView, frame: &mut Frame, area: Rect) {
    let cols = area.width as usize;
    let rows = area.height as usize;
    if rows < 3 {
        return;
    }
    let has_bar = view.targets.len() > 1;
    let h = rows.saturating_sub(2 + if has_bar { 1 } else { 0 }).max(1);
    let is_logs = view.tab == Tab::Logs;
    // Record the pane geometry so n/N wrap/scroll match what's drawn.
    view.view_h = h;
    view.detail_width = if view.maximized { cols } else { cols.saturating_sub(((cols as f64) * 0.45) as usize + 1) };

    let d = view.active.as_ref().and_then(|k| view.devices.get(k));
    let who = view.active.clone().unwrap_or_else(|| "…".into());
    let status = match d.map(|d| &d.status) {
        Some(Status::Connected) => "connected",
        Some(Status::Connecting) => "connecting",
        Some(Status::Disconnected) => "disconnected",
        None => "…",
    };
    let logs_n = d.map(|d| d.logs.len()).unwrap_or(0);
    let net_n = d.map(|d| d.net.len()).unwrap_or(0);
    let tabbar = {
        let lbl = |t: Tab, s: String| if view.tab == t { format!("[{s}]") } else { format!(" {s} ") };
        format!("{} {} {}", lbl(Tab::Logs, format!("Logs ({logs_n})")), lbl(Tab::Network, format!("Network ({net_n})")), lbl(Tab::Perf, "Perf".into()))
    };
    let restart = if view.clear_on_restart { "clears logs on reload" } else { "keeps logs on reload" };
    let mut net_filters = String::new();
    if !is_logs && (view.errors_only || view.method_idx != 0) {
        let mut parts = Vec::new();
        if view.errors_only {
            parts.push("errors".to_string());
        }
        if view.method_idx != 0 {
            parts.push(METHODS[view.method_idx].to_string());
        }
        net_filters = format!(" · {}", parts.join("+"));
    }
    let filter_str = if view.filter.is_empty() { String::new() } else { format!(" · filter:\"{}\"", view.filter) };
    // App/device engine metadata for the active target (e.g. "Hermes").
    let meta = view.targets.iter().find(|t| Some(&t.key) == view.active.as_ref()).and_then(|t| t.meta.clone());
    let meta_str = meta.map(|m| format!(" · {m}")).unwrap_or_default();
    // While paused at a breakpoint the top bar becomes the debugger banner;
    // otherwise it's the normal status/tab line.
    let paused = d.and_then(|d| d.paused.as_ref());
    let head = match paused {
        Some(p) => format!(" ⏸ PAUSED {}:{} — F5 continue · F10 over · F11 into · ⇧F11 out · F6 pause", p.file, p.line),
        // Autoscroll state (space toggles it, like every scrollable list).
        None => format!(" {who} · {status}{meta_str}{filter_str}{net_filters} · {restart}{}   {tabbar}", if view.follow { "" } else { " · paused" }),
    };
    let head_style = if paused.is_some() {
        Style::default().bg(Color::Rgb(191, 97, 106)).fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        bar_style()
    };

    // Blank the key footer when another pane has focus (embedded in the dashboard).
    let foot = if view.focused { footer(view, is_logs) } else { String::new() };
    let foot_style = if view.focused { bar_style() } else { Style::default() };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::styled(pad(&head, cols), head_style));
    if has_bar {
        let db = format!(" Devices: {}", view.targets.iter().enumerate().map(|(i, t)| if Some(&t.key) == view.active.as_ref() { format!("[{}:{}]", i + 1, t.label) } else { format!(" {}:{} ", i + 1, t.label) }).collect::<Vec<_>>().join(" "));
        lines.push(Line::styled(pad(&db, cols), bar_style()));
    }

    let body = render_body(view, cols, h, d);
    lines.extend(body);
    while lines.len() < rows - 1 {
        lines.push(Line::raw(""));
    }
    lines.truncate(rows - 1);
    lines.push(Line::styled(pad(&foot, cols), foot_style));

    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_body(view: &RnView, cols: usize, h: usize, d: Option<&Device>) -> Vec<Line<'static>> {
    if view.tab == Tab::Perf {
        return perf_lines(view, h);
    }
    let rows = view.rows();
    let len = rows.len();
    if view.tab == Tab::Network && d.map(|d| !d.network_supported && d.net.is_empty()).unwrap_or(false) {
        let mut out = vec![Line::styled(" Network isn’t exposed over CDP by this React Native version.", Style::default().fg(Color::Yellow))];
        while out.len() < h {
            out.push(Line::raw(""));
        }
        return out;
    }
    let eff_sel = if view.follow { len.saturating_sub(1) } else { view.sel[view.tab_idx()].min(len.saturating_sub(1)) };
    // Visual-select range over list rows — only when the list is the active
    // cursor (no detail panel). With a panel open, visual targets the panel.
    let list_vis = if view.detail { None } else { view.visual.map(|a| (a.min(eff_sel), a.max(eff_sel))) };

    if view.detail && len > 0 {
        let dl = view.detail_wrapped();
        let d_start = view.detail_scroll.min(dl.len().saturating_sub(h));
        if view.maximized {
            // Visual-select range over preview lines, cursor = detail_cursor.
            let cursor = view.detail_cursor.min(dl.len().saturating_sub(1));
            let prev_vis = view.visual.map(|a| (a.min(cursor), a.max(cursor)));
            return (0..h)
                .map(|i| {
                    let abs = d_start + i;
                    match dl.get(abs) {
                        Some(l) => {
                            let base = if abs == cursor {
                                Style::default().add_modifier(Modifier::REVERSED)
                            } else if in_range(prev_vis, abs) {
                                sel_style()
                            } else {
                                Style::default()
                            };
                            row_line(l, cols, base, &view.search, Some(abs) == view.detail_hit)
                        }
                        None => Line::raw(""),
                    }
                })
                .collect();
        }
        // split: list | detail
        let right_w = view.detail_width;
        let left_w = cols.saturating_sub(right_w + 1);
        let list_start = eff_sel.saturating_sub(h / 2).min(len.saturating_sub(h));
        // Panel cursor + visual range, shown on the right side.
        let dcursor = view.detail_cursor.min(dl.len().saturating_sub(1));
        let prev_vis = view.visual.map(|a| (a.min(dcursor), a.max(dcursor)));
        return (0..h)
            .map(|i| {
                let idx = list_start + i;
                let mut spans: Vec<Span> = Vec::new();
                if let Some((text, err, level)) = rows.get(idx) {
                    let base = if idx == eff_sel {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else if in_range(list_vis, idx) {
                        sel_style()
                    } else if *err {
                        Style::default().fg(Color::Red)
                    } else {
                        level_color(level)
                    };
                    let line = row_line(text, left_w, base, &view.search, false);
                    spans.extend(line.spans);
                } else {
                    spans.push(Span::raw(" ".repeat(left_w)));
                }
                spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
                let abs = d_start + i;
                let dstyle = if abs == dcursor {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else if in_range(prev_vis, abs) {
                    sel_style()
                } else {
                    Style::default()
                };
                let dline = dl.get(abs).cloned().unwrap_or_default();
                let dl_line = row_line(&dline, right_w, dstyle, &view.search, Some(abs) == view.detail_hit);
                spans.extend(dl_line.spans);
                Line::from(spans)
            })
            .collect();
    }

    // full list
    let start = if view.follow { len.saturating_sub(h) } else { eff_sel.saturating_sub(h / 2).min(len.saturating_sub(h)) };
    (0..h)
        .map(|i| {
            let idx = start + i;
            match rows.get(idx) {
                Some((text, err, level)) => {
                    let current = idx == eff_sel;
                    let base = if current {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else if in_range(list_vis, idx) {
                        sel_style()
                    } else if *err {
                        Style::default().fg(Color::Red)
                    } else {
                        level_color(level)
                    };
                    row_line(text, cols, base, &view.search, current)
                }
                None => Line::raw(""),
            }
        })
        .collect()
}

fn footer(view: &RnView, is_logs: bool) -> String {
    match view.mode {
        Mode::Search => format!("search: {}▏", view.input),
        Mode::Filter => format!("filter: {}▏", view.input),
        Mode::Normal => {
            // Standalone only: the dashboard shows R/q as global keys.
            let (reload, quit) = if view.embedded { ("", "") } else { (" · R reload", " · q quit") };
            // Device switching only matters with more than one connected.
            let devs = if view.targets.len() > 1 { " 1-9 devices ·" } else { "" };
            if let Some(f) = &view.flash {
                return format!(" {f}");
            }
            if view.visual.is_some() {
                let what = if view.maximized { "lines" } else { "rows" };
                return format!(" VISUAL ({what}) · y yank · V/esc cancel");
            }
            if view.tab == Tab::Perf {
                return format!("{devs} live JS FPS + heap{reload}{quit}");
            }
            if view.detail {
                let nn = if view.search.is_empty() { "" } else { " · n/N" };
                let z = if view.maximized { "z split" } else { "z max" };
                let copy = if is_logs { " · F frames" } else { " · c curl" };
                return format!(" ⏎ close · {z} · {{}} sect · / search{nn} · V select · y copy{copy} · o nvim{quit}");
            }
            let nn = if view.search.is_empty() { "" } else { " · n/N" };
            let netf = if is_logs { "" } else { " · e errors · m method" };
            let restart = if view.clear_on_restart { "clear" } else { "keep" };
            format!("{devs} / search{nn} · f filter · ⏎ preview · z max · V select · y copy{netf} · c clear · p reload:{restart}{reload}{quit}")
        }
    }
}

// ── key handling ─────────────────────────────────────────────────────────────

fn match_indexes(view: &RnView) -> Vec<usize> {
    let n = view.search.to_lowercase();
    view.rows().iter().enumerate().filter(|(_, (t, _, _))| t.to_lowercase().contains(&n)).map(|(i, _)| i).collect()
}

fn jump(view: &mut RnView, dir: i32) {
    if view.search.is_empty() {
        return;
    }
    let hits = match_indexes(view);
    if hits.is_empty() {
        return;
    }
    let n = view.rows().len();
    let base = if view.follow { n.saturating_sub(1) } else { view.sel[view.tab_idx()] };
    let next = if dir == 1 { hits.iter().find(|&&i| i > base).copied().unwrap_or(hits[0]) } else { hits.iter().rev().find(|&&i| i < base).copied().unwrap_or(*hits.last().unwrap()) };
    view.follow = false;
    view.sel[view.tab_idx()] = next;
}

fn jump_first(view: &mut RnView) {
    if view.search.is_empty() {
        return;
    }
    if view.detail {
        detail_jump_first(view);
        return;
    }
    let hits = match_indexes(view);
    if hits.is_empty() {
        return;
    }
    let base = if view.follow { 0 } else { view.sel[view.tab_idx()] };
    view.follow = false;
    view.sel[view.tab_idx()] = hits.iter().find(|&&i| i >= base).copied().unwrap_or(hits[0]);
}

const DETAIL_CONTEXT: usize = 3;

fn detail_matches(view: &RnView) -> Vec<usize> {
    if view.search.is_empty() {
        return Vec::new();
    }
    let n = view.search.to_lowercase();
    view.detail_wrapped().iter().enumerate().filter(|(_, l)| l.to_lowercase().contains(&n)).map(|(i, _)| i).collect()
}

fn detail_jump(view: &mut RnView, dir: i32) {
    let hits = detail_matches(view);
    if hits.is_empty() {
        return;
    }
    let base = view.detail_hit.unwrap_or(view.detail_scroll);
    let next = if dir == 1 {
        hits.iter().find(|&&i| i > base).copied().unwrap_or(hits[0])
    } else {
        hits.iter().rev().find(|&&i| i < base).copied().unwrap_or(*hits.last().unwrap())
    };
    view.detail_hit = Some(next);
    view.detail_scroll = next.saturating_sub(DETAIL_CONTEXT);
}

fn detail_jump_first(view: &mut RnView) {
    let hits = detail_matches(view);
    if hits.is_empty() {
        return;
    }
    let from = view.detail_hit.unwrap_or(view.detail_scroll);
    let next = hits.iter().find(|&&i| i >= from).copied().unwrap_or(hits[0]);
    view.detail_hit = Some(next);
    view.detail_scroll = next.saturating_sub(DETAIL_CONTEXT);
}

fn copy_selection(view: &mut RnView) {
    let lines = view.detail_source();
    if lines.is_empty() {
        return;
    }
    let text = lines.join("\n");
    view.flash = Some(if copy_to_clipboard(&text) { format!("copied ({} chars)", text.len()) } else { "copy failed".into() });
}

/// Copy `text` to the clipboard, flashing how many lines were yanked.
fn yank(view: &mut RnView, text: String) {
    if text.is_empty() {
        return;
    }
    let n = text.lines().count();
    view.flash = Some(if copy_to_clipboard(&text) { format!("yanked {n} line{}", if n == 1 { "" } else { "s" }) } else { "copy failed".into() });
}

/// Scroll the maximized preview so `detail_cursor` stays on screen.
fn keep_cursor_visible(view: &mut RnView) {
    let h = view.view_h.max(1);
    if view.detail_cursor < view.detail_scroll {
        view.detail_scroll = view.detail_cursor;
    } else if view.detail_cursor >= view.detail_scroll + h {
        view.detail_scroll = view.detail_cursor + 1 - h;
    }
}

/// Move the preview cursor to the next/prev section header (`── … ──`) — the
/// Stack divider in logs, or the headers/body dividers in a network entry.
fn jump_section(view: &mut RnView, dir: i32) {
    let lines = view.detail_wrapped();
    let is_header = |s: &str| s.trim_start().starts_with("──");
    let cur = view.detail_cursor;
    let target = if dir > 0 {
        lines.iter().enumerate().skip(cur + 1).find(|(_, l)| is_header(l)).map(|(i, _)| i)
    } else {
        // Previous header, or the top of the panel if there's none above.
        lines.iter().enumerate().take(cur).filter(|(_, l)| is_header(l)).last().map(|(i, _)| i).or(Some(0))
    };
    if let Some(i) = target {
        view.detail_cursor = i;
        // Pin the header to the top so its content is visible below it
        // (clamped so the last page doesn't scroll past the end).
        view.detail_scroll = i.min(lines.len().saturating_sub(view.view_h.max(1)));
        view.detail_hit = None;
    }
}

/// Reset the preview cursor/scroll to the top — call when the side-by-side list
/// selection changes so the panel starts fresh on the new entry.
fn reset_detail_cursor(view: &mut RnView) {
    if view.detail {
        view.detail_cursor = 0;
        view.detail_scroll = 0;
        view.detail_hit = None;
    }
}

/// Yank the current visual selection (preview lines when maximized, else list
/// rows). Returns false if there was no active selection.
fn yank_visual(view: &mut RnView) -> bool {
    let anchor = match view.visual {
        Some(a) => a,
        None => return false,
    };
    let text = if view.detail {
        let lines = view.detail_wrapped();
        let cursor = view.detail_cursor.min(lines.len().saturating_sub(1));
        let (lo, hi) = (anchor.min(cursor), anchor.max(cursor));
        lines.get(lo..=hi).map(|s| s.join("\n")).unwrap_or_default()
    } else {
        // Copy each selected row's full detail (request/response, log + object
        // tree), not just the summary line.
        let len = view.rows().len();
        let eff = view.eff_sel(len);
        let (lo, hi) = (anchor.min(eff), anchor.max(eff).min(len.saturating_sub(1)));
        (lo..=hi).map(|i| view.row_detail(i).join("\n")).collect::<Vec<_>>().join("\n\n")
    };
    yank(view, text);
    view.visual = None;
    true
}

fn copy_curl(view: &mut RnView) {
    if view.tab != Tab::Network {
        return;
    }
    let d = match view.active.as_ref().and_then(|k| view.devices.get(k)) {
        Some(d) => d,
        None => return,
    };
    let method = METHODS[view.method_idx];
    let f = view.filter.to_lowercase();
    let list: Vec<&NetRecord> = d.net.iter().filter(|r| (f.is_empty() || net_summary(r).to_lowercase().contains(&f)) && (!view.errors_only || net_error(r)) && (method == "ALL" || r.method.to_uppercase() == method)).collect();
    let sel = view.sel[view.tab_idx()].min(list.len().saturating_sub(1));
    if let Some(rec) = list.get(sel) {
        let curl = to_curl(rec);
        view.flash = Some(if copy_to_clipboard(&curl) { "copied curl".into() } else { "copy failed".into() });
    }
}

/// Send a debugger command to the active device, but only while it's suspended
/// (so continue/step keys are inert when nothing is paused).
fn send_if_paused(view: &RnView, client: &RnClient, key: &Option<String>, cmd: ConnCmd) {
    if let Some(k) = key {
        if view.devices.get(k).map(|d| d.paused.is_some()).unwrap_or(false) {
            client.send(k, cmd);
        }
    }
}

// ── open a stack frame in the running nvim ───────────────────────────────────

/// Pull a `file:line` (or `file:line:col`) out of a rendered stack line — e.g.
/// the `(src/foo.ts:42)` in a frame.
fn parse_location(line: &str) -> Option<(String, usize)> {
    let candidate = match (line.rfind('('), line.rfind(')')) {
        (Some(a), Some(b)) if b > a + 1 => &line[a + 1..b],
        _ => line.trim(),
    };
    let mut it = candidate.rsplitn(3, ':');
    let last = it.next()?;
    let mid = it.next()?;
    let (file, line_str) = if mid.parse::<usize>().is_ok() {
        (it.next()?, mid) // file:line:col
    } else if last.parse::<usize>().is_ok() {
        (mid, last) // file:line
    } else {
        return None;
    };
    let line_no: usize = line_str.parse().ok()?;
    let file = file.trim();
    if file.is_empty() {
        return None;
    }
    Some((file.to_string(), line_no))
}

/// Resolve a (possibly project-relative) source path against the project root.
fn resolve_path(file: &str, root: Option<&str>) -> String {
    if file.starts_with('/') {
        return file.to_string();
    }
    match root {
        Some(r) => format!("{}/{file}", r.trim_end_matches('/')),
        None => file.to_string(),
    }
}

/// The unwrapped logical detail line under the cursor, so a frame split across
/// two visual rows (narrow side-by-side panel) still parses.
fn source_line_at_cursor(view: &RnView) -> Option<String> {
    let src = view.detail_source();
    let width = view.detail_width.max(1);
    let mut acc = 0;
    for line in &src {
        let n = wrap(line, width).len().max(1);
        if view.detail_cursor < acc + n {
            return Some(line.clone());
        }
        acc += n;
    }
    src.last().cloned()
}

fn open_at_cursor(view: &mut RnView) {
    let line = match source_line_at_cursor(view) {
        Some(l) => l,
        None => return,
    };
    let (file, line_no) = match parse_location(&line) {
        Some(x) => x,
        None => {
            view.flash = Some("no file:line on this line".into());
            return;
        }
    };
    let path = resolve_path(&file, view.root.as_deref());
    match open_in_nvim(&path, line_no) {
        Ok(()) => view.flash = Some(format!("opening {file}:{line_no}")),
        Err(e) => view.flash = Some(e),
    }
}

/// Open `path` at `line` in an already-running nvim: a tmux pane running nvim if
/// we're in tmux, otherwise the running nvim's `--server` socket.
fn open_in_nvim(path: &str, line: usize) -> Result<(), String> {
    if std::env::var("TMUX").is_ok() {
        let pane = tmux_nvim_pane().ok_or("no nvim pane in this tmux session")?;
        let goto = format!(":e +{line} {path}");
        let run = |args: &[&str]| {
            let _ = Command::new("tmux").args(args).status();
        };
        run(&["send-keys", "-t", &pane, "Escape"]);
        run(&["send-keys", "-t", &pane, &goto, "Enter"]);
        run(&["select-window", "-t", &pane]);
        run(&["select-pane", "-t", &pane]);
        return Ok(());
    }
    let sock = nvim_socket().ok_or("no running nvim found (start it with --listen, or set NVIM_LISTEN_ADDRESS)")?;
    let keys = format!("<C-\\><C-n>:e +{line} {path}<CR>");
    let ok = Command::new("nvim")
        .args(["--server", &sock, "--remote-send", &keys])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err("couldn't reach the nvim server".into())
    }
}

/// The pane id of an nvim in the current tmux session, if any.
fn tmux_nvim_pane() -> Option<String> {
    let out = Command::new("tmux").args(["list-panes", "-s", "-F", "#{pane_id} #{pane_current_command}"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for l in text.lines() {
        let mut parts = l.split_whitespace();
        let id = parts.next()?;
        if parts.next() == Some("nvim") {
            return Some(id.to_string());
        }
    }
    None
}

/// Best-effort discovery of a running nvim server socket (off-tmux fallback).
fn nvim_socket() -> Option<String> {
    for var in ["NVIM_LISTEN_ADDRESS", "NVIM"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for var in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Ok(v) = std::env::var(var) {
            roots.push(std::path::PathBuf::from(v));
        }
    }
    roots.push(std::path::PathBuf::from("/tmp"));
    let mut best: Option<(std::time::SystemTime, String)> = None;
    let mut consider = |p: std::path::PathBuf| {
        if let Ok(meta) = std::fs::metadata(&p) {
            let t = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                best = Some((t, p.to_string_lossy().into_owned()));
            }
        }
    };
    for r in roots {
        if let Ok(entries) = std::fs::read_dir(&r) {
            for e in entries.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.starts_with("nvim") {
                    continue;
                }
                if p.is_dir() {
                    if let Ok(inner) = std::fs::read_dir(&p) {
                        for ie in inner.flatten() {
                            consider(ie.path());
                        }
                    }
                } else {
                    consider(p);
                }
            }
        }
    }
    best.map(|(_, s)| s)
}

/// Returns true to quit.
fn handle_key(view: &mut RnView, key: KeyEvent, client: &RnClient) -> bool {
    if view.mode != Mode::Normal {
        match key.code {
            KeyCode::Enter => {
                let was_search = view.mode == Mode::Search;
                view.mode = Mode::Normal;
                if was_search && !view.search.is_empty() && !view.detail {
                    jump(view, 1);
                }
            }
            KeyCode::Esc => {
                view.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                view.input.pop();
                if view.mode == Mode::Filter {
                    view.filter = view.input.clone();
                } else {
                    view.search = view.input.clone();
                    jump_first(view);
                }
            }
            KeyCode::Char(c) => {
                view.input.push(c);
                if view.mode == Mode::Filter {
                    view.filter = view.input.clone();
                } else {
                    view.search = view.input.clone();
                    jump_first(view);
                }
            }
            _ => {}
        }
        return false;
    }

    view.flash = None;
    let n = view.rows().len();
    let ti = view.tab_idx();
    let h = 20usize; // page size approximation for PgUp/PgDn
    let key_active = view.active.clone();

    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        // Debugger stepping (only act while the VM is suspended).
        KeyCode::F(5) => send_if_paused(view, client, &key_active, ConnCmd::Continue),
        KeyCode::F(10) => send_if_paused(view, client, &key_active, ConnCmd::StepOver),
        KeyCode::F(11) if key.modifiers.contains(KeyModifiers::SHIFT) => send_if_paused(view, client, &key_active, ConnCmd::StepOut),
        KeyCode::F(11) => send_if_paused(view, client, &key_active, ConnCmd::StepInto),
        // Interrupt a running VM (works whether or not already paused).
        KeyCode::F(6) => {
            if let Some(k) = &key_active {
                client.send(k, ConnCmd::Pause);
            }
        }
        KeyCode::Char('[') => {
            view.tab = match view.tab { Tab::Logs => Tab::Perf, Tab::Network => Tab::Logs, Tab::Perf => Tab::Network };
            view.detail = false;
            view.maximized = false;
            view.visual = None;
        }
        KeyCode::Char(']') => {
            view.tab = match view.tab { Tab::Logs => Tab::Network, Tab::Network => Tab::Perf, Tab::Perf => Tab::Logs };
            view.detail = false;
            view.maximized = false;
            view.visual = None;
        }
        KeyCode::Char('/') => {
            view.mode = Mode::Search;
            view.input.clear();
            if view.detail {
                view.detail_scroll = 0; // pane search starts at the top
                view.detail_hit = None;
            }
        }
        KeyCode::Char('f') => {
            view.mode = Mode::Filter;
            view.input = view.filter.clone();
        }
        KeyCode::Char('c') => {
            if view.detail && view.tab == Tab::Network {
                copy_curl(view);
            } else if view.detail {
                copy_selection(view);
            } else if let Some(k) = &key_active {
                let d = view.dev(k);
                d.logs.clear();
                d.net.clear();
                client.send(k, ConnCmd::DiscardConsole);
            }
        }
        KeyCode::Char('z') => {
            view.visual = None;
            if !view.detail {
                view.detail = true;
                view.detail_scroll = 0;
                view.detail_cursor = 0;
                view.maximized = true;
                view.opened_by_max = true;
            } else if view.maximized {
                view.maximized = false;
                if view.opened_by_max {
                    view.detail = false;
                    view.opened_by_max = false;
                }
            } else {
                view.maximized = true;
                view.detail_cursor = view.detail_scroll;
            }
        }
        KeyCode::Char('p') => view.clear_on_restart = !view.clear_on_restart,
        // Open the stack frame under the cursor in the already-running nvim.
        KeyCode::Char('o') if view.detail => open_at_cursor(view),
        // Jump between detail sections (── Stack ──, Request/Response headers/body …).
        KeyCode::Char('}') if view.detail => jump_section(view, 1),
        KeyCode::Char('{') if view.detail => jump_section(view, -1),
        KeyCode::Char('F') if view.detail && view.tab == Tab::Logs => {
            view.show_framework = !view.show_framework;
            view.detail_cursor = view.detail_cursor.min(view.detail_wrapped().len().saturating_sub(1));
            keep_cursor_visible(view);
            view.flash = Some(if view.show_framework { "showing framework frames" } else { "hiding framework frames" }.into());
        }
        KeyCode::Char('V') => {
            if view.visual.is_some() {
                view.visual = None;
            } else if view.detail {
                view.visual = Some(view.detail_cursor);
            } else {
                view.follow = false;
                view.visual = Some(view.eff_sel(n));
            }
        }
        KeyCode::Char('y') => {
            if !yank_visual(view) {
                copy_selection(view);
            }
        }
        KeyCode::Char('C') => copy_curl(view),
        KeyCode::Char('e') if view.tab == Tab::Network => view.errors_only = !view.errors_only,
        KeyCode::Char('m') if view.tab == Tab::Network => view.method_idx = (view.method_idx + 1) % METHODS.len(),
        KeyCode::Char('a') | KeyCode::Char(' ') => {
            view.visual = None;
            view.follow = !view.follow;
            if view.follow {
                view.sel[ti] = n.saturating_sub(1);
            }
        }
        KeyCode::Char(ch @ '1'..='9') => {
            let idx = ch as usize - '1' as usize;
            if idx < view.targets.len() {
                view.active = Some(view.targets[idx].key.clone());
                view.detail = false;
                view.maximized = false;
                view.visual = None;
            }
        }
        KeyCode::Char('R') => {
            if let Some(k) = &key_active {
                client.send(k, ConnCmd::Reload);
            }
        }
        KeyCode::Esc if view.visual.is_some() => view.visual = None,
        KeyCode::Enter | KeyCode::Esc => {
            view.visual = None;
            if view.detail {
                view.detail = false;
                view.maximized = false;
                view.opened_by_max = false;
            } else if n > 0 {
                view.detail = true;
                view.detail_scroll = 0;
                view.detail_cursor = 0;
            }
        }
        KeyCode::Char('J') => {
            if view.detail {
                let last = view.detail_wrapped().len().saturating_sub(1);
                view.detail_cursor = (view.detail_cursor + 1).min(last);
                keep_cursor_visible(view);
                view.detail_hit = None;
            }
        }
        KeyCode::Char('K') => {
            if view.detail {
                view.detail_cursor = view.detail_cursor.saturating_sub(1);
                keep_cursor_visible(view);
                view.detail_hit = None;
            }
        }
        KeyCode::Char('n') => {
            if view.detail {
                detail_jump(view, 1);
            } else {
                jump(view, 1);
            }
        }
        KeyCode::Char('N') => {
            if view.detail {
                detail_jump(view, -1);
            } else {
                jump(view, -1);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if view.detail && view.maximized {
                // Move the preview cursor (drives visual selection); view follows.
                view.detail_cursor = view.detail_cursor.saturating_sub(1);
                keep_cursor_visible(view);
                view.detail_hit = None;
            } else {
                // Start from the row actually shown (the bottom while following),
                // not the stale sel=0, so the first `k` steps up by one.
                let cur = if view.follow { n.saturating_sub(1) } else { view.sel[ti] };
                view.follow = false;
                view.sel[ti] = cur.saturating_sub(1);
                reset_detail_cursor(view); // new entry previewed → panel back to top
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if view.detail && view.maximized {
                let last = view.detail_wrapped().len().saturating_sub(1);
                view.detail_cursor = (view.detail_cursor + 1).min(last);
                keep_cursor_visible(view);
                view.detail_hit = None;
            } else {
                let cur = if view.follow { n.saturating_sub(1) } else { view.sel[ti] };
                let next = (cur + 1).min(n.saturating_sub(1));
                view.follow = next >= n.saturating_sub(1);
                view.sel[ti] = next;
                reset_detail_cursor(view);
            }
        }
        KeyCode::PageUp => {
            let cur = if view.follow { n.saturating_sub(1) } else { view.sel[ti] };
            view.follow = false;
            view.sel[ti] = cur.saturating_sub(h);
        }
        KeyCode::PageDown => {
            let cur = if view.follow { n.saturating_sub(1) } else { view.sel[ti] };
            let next = (cur + h).min(n.saturating_sub(1));
            view.follow = next >= n.saturating_sub(1);
            view.sel[ti] = next;
        }
        KeyCode::Char('g') => {
            if view.detail {
                view.detail_cursor = 0;
                keep_cursor_visible(view);
            } else {
                view.visual = None;
                view.follow = false;
                view.sel[ti] = 0;
            }
        }
        KeyCode::Char('G') => {
            if view.detail {
                view.detail_cursor = view.detail_wrapped().len().saturating_sub(1);
                keep_cursor_visible(view);
            } else {
                view.visual = None;
                view.follow = true;
                view.sel[ti] = n.saturating_sub(1);
            }
        }
        _ => {}
    }
    false
}

fn copy_to_clipboard(text: &str) -> bool {
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        return child.wait().map(|s| s.success()).unwrap_or(false);
    }
    false
}
