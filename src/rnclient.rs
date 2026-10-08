//! Multi-connection Metro CDP client. One background thread per RN target reads
//! the inspector socket (with a short read timeout so it can also service
//! commands), eagerly expands console object args into real JS, proactively
//! fetches response bodies, and streams everything to the UI over a channel.
//! Mirrors the TypeScript `utils/rnclient.ts`.

use crate::rn::{connect_cdp, fetch_targets};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Optional CDP wire trace. Set METROCTL_CDP_DEBUG=/path/to/log to append every
/// frame sent/received (and anything `await_result` drops) for debugging the
/// Hermes/fusebox handshake. Resolved once per process.
static CDP_TRACE: OnceLock<Option<String>> = OnceLock::new();

fn cdp_trace(tag: &str, msg: &str) {
    let path = CDP_TRACE.get_or_init(|| std::env::var("METROCTL_CDP_DEBUG").ok());
    if let Some(p) = path {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let trimmed: String = msg.chars().take(600).collect();
            let _ = writeln!(f, "{tag} {trimmed}");
        }
    }
}

// The DAP server lives in its own file but as a submodule here, so it can reach
// ConnCmd/DebugEvent without touching main.rs (which carries unrelated WIP).
#[path = "dap.rs"]
mod dap;

/// A stack frame, already symbolicated to an original source location, handed to
/// an attached DAP client for its stackTrace.
#[derive(Debug, Clone)]
pub struct DapFrame {
    pub name: String,
    pub file: String,
    pub line: i64,
    pub column: i64,
}

/// Async debugger notifications pushed to an attached DAP session (one per
/// connected editor). Separate from `RnEvent`, which drives the TUI.
pub enum DebugEvent {
    Paused { frames: Vec<DapFrame>, reason: String, raw_frames: Value },
    Resumed,
}

/// Shared slot holding the attached DAP session's event stream. It lives above
/// the per-target connection, so a Metro reload — which tears down and respawns
/// that connection — doesn't detach the editor. Each (re)connected `conn_loop`
/// reads this to know where to fan Debugger events.
pub type DebugSink = Arc<Mutex<Option<Sender<DebugEvent>>>>;

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub kind: String,  // console | network | system
    pub level: String, // log | warn | error | info | ...
    pub text: String,
    pub expanded: Option<Vec<String>>, // deep object tree (already fetched), if any
    pub stack: Option<Vec<String>>,    // call frames ("fn (file:line)"), for exceptions
}

#[derive(Debug, Clone, Default)]
pub struct NetRecord {
    pub id: String,
    pub method: String,
    pub url: String,
    pub status: Option<i64>,
    pub mime_type: Option<String>,
    pub req_headers: Vec<(String, String)>,
    pub req_body: Option<String>,
    pub res_headers: Vec<(String, String)>,
    pub res_body: Option<String>,
    pub start_ts: Option<f64>,
    pub duration_ms: Option<f64>,
    pub size: Option<i64>,       // bytes over the wire (encodedDataLength)
    pub failed: Option<String>,  // error text when the request failed
    pub is_ws: bool,             // a WebSocket connection
    pub ws_frames: Vec<(bool, String)>, // (sent?, payload) for WS frames
}

#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub key: String,
    pub label: String,
    pub meta: Option<String>, // app/device/engine metadata for the header
}

#[derive(Debug, Clone, Default)]
pub struct PerfSample {
    pub fps: Option<i64>,
    pub heap_used: Option<i64>,
    pub heap_total: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum RnEvent {
    Targets(Vec<TargetInfo>),
    Status(String, Status),
    Log(String, LogEntry),
    Net(String, NetRecord),
    Network(String, bool),
    ContextCleared(String),
    Perf(String, PerfSample),
    /// The JS VM hit a breakpoint / `debugger;` / step and is now suspended.
    Paused(String, PausedInfo),
    /// Execution resumed (continue or step completed).
    Resumed(String),
}

/// Where the JS VM stopped, already mapped back to an original source location.
#[derive(Debug, Clone)]
pub struct PausedInfo {
    pub file: String,
    pub line: i64,
    pub reason: String,
}

/// Fire-and-forget commands the UI sends to a connection.
pub enum ConnCmd {
    Reload,
    DiscardConsole,
    Continue,
    StepOver,
    StepInto,
    StepOut,
    Pause,
    /// Synchronous CDP call on behalf of a DAP session (scopes/variables/eval):
    /// send `method`+`params`, reply with the CDP `result`.
    DebugRequest { method: String, params: Value, reply: Sender<Value> },
}

pub struct RnClient {
    pub rx: Receiver<RnEvent>,
    cmd_senders: Arc<Mutex<HashMap<String, Sender<ConnCmd>>>>,
    port: u16,
}

impl RnClient {
    pub fn start(port: u16) -> RnClient {
        let (tx, rx) = std::sync::mpsc::channel();
        let cmd_senders: Arc<Mutex<HashMap<String, Sender<ConnCmd>>>> = Arc::new(Mutex::new(HashMap::new()));
        let debug_sink: DebugSink = Arc::new(Mutex::new(None));
        let senders = cmd_senders.clone();
        let evt = tx.clone();
        let sink = debug_sink.clone();
        std::thread::spawn(move || poll_loop(port, evt, senders, sink));
        // Expose this metroctl's single Hermes debugger to editors over DAP.
        let dap_senders = cmd_senders.clone();
        std::thread::spawn(move || dap::serve(dap_senders, debug_sink));
        RnClient { rx, cmd_senders, port }
    }

    pub fn send(&self, key: &str, cmd: ConnCmd) {
        if let Some(s) = self.cmd_senders.lock().unwrap().get(key) {
            let _ = s.send(cmd);
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

fn poll_loop(port: u16, tx: Sender<RnEvent>, senders: Arc<Mutex<HashMap<String, Sender<ConnCmd>>>>, debug_sink: DebugSink) {
    loop {
        if let Ok(targets) = fetch_targets(port) {
            let live: Vec<_> = targets.into_iter().filter(|t| t.web_socket_debugger_url.is_some()).collect();
            let infos: Vec<TargetInfo> = live.iter().map(|t| TargetInfo { key: t.key(), label: t.label(), meta: t.vm.clone() }).collect();
            let _ = tx.send(RnEvent::Targets(infos));
            for t in live {
                let key = t.key();
                let already = senders.lock().unwrap().contains_key(&key);
                if !already {
                    let (ctx, crx) = std::sync::mpsc::channel();
                    senders.lock().unwrap().insert(key.clone(), ctx);
                    let url = t.web_socket_debugger_url.clone().unwrap();
                    let tx2 = tx.clone();
                    let senders2 = senders.clone();
                    let sink = debug_sink.clone();
                    std::thread::spawn(move || {
                        conn_loop(&key, &url, port, &tx2, crx, &sink);
                        senders2.lock().unwrap().remove(&key); // allow reconnection on next poll
                    });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(3000));
    }
}

fn conn_loop(key: &str, url: &str, port: u16, tx: &Sender<RnEvent>, crx: Receiver<ConnCmd>, debug_sink: &DebugSink) {
    let _ = tx.send(RnEvent::Status(key.into(), Status::Connecting));
    let mut socket = match connect_cdp(url, port) {
        Ok(s) => s,
        Err(_) => {
            let _ = tx.send(RnEvent::Status(key.into(), Status::Disconnected));
            return;
        }
    };
    if let tungstenite::stream::MaybeTlsStream::Plain(ref s) = socket.get_ref() {
        let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
    }
    let mut seq = 100i64;
    let mut send = |socket: &mut tungstenite::WebSocket<_>, method: &str| {
        seq += 1;
        let _ = socket.send(tungstenite::Message::Text(json!({"id": seq, "method": method}).to_string()));
    };
    send(&mut socket, "Runtime.enable");
    send(&mut socket, "Log.enable");
    send(&mut socket, "Network.enable");
    send(&mut socket, "Debugger.enable");
    // If Hermes started paused waiting for a debugger, let it run; harmless on an
    // already-running VM.
    send(&mut socket, "Runtime.runIfWaitingForDebugger");
    cdp_trace("--", &format!("connected {key}: sent Runtime/Log/Network/Debugger.enable + runIfWaitingForDebugger"));
    let _ = tx.send(RnEvent::Status(key.into(), Status::Connected));
    let _ = tx.send(RnEvent::Network(key.into(), true));

    let mut records: HashMap<String, NetRecord> = HashMap::new();
    // scriptId → bundle URL. Debugger.paused callFrames carry only a scriptId,
    // so we learn URLs from console/exception stacks and scriptParsed to map a
    // pause back to a source location.
    let mut scripts: HashMap<String, String> = HashMap::new();
    let mut last_perf = std::time::Instant::now() - Duration::from_secs(2);
    // While suspended at a breakpoint we must not poke the VM with the perf
    // sampler's `Runtime.evaluate` — it either queues behind the pause or
    // disturbs it. Set on `Debugger.paused`, cleared on `Debugger.resumed`.
    let mut paused = false;
    const FPS_EXPR: &str = "(function(){var s=globalThis.__simonFps;if(!s){s=globalThis.__simonFps={v:0,c:0,t:Date.now()};var loop=function(){s.c++;var n=Date.now();if(n-s.t>=1000){s.v=Math.round(s.c*1000/(n-s.t));s.c=0;s.t=n;}(globalThis.requestAnimationFrame||function(f){return setTimeout(f,16);})(loop);};loop();}return s.v;})()";

    loop {
        if !paused && last_perf.elapsed() >= Duration::from_secs(1) {
            last_perf = std::time::Instant::now();
            let mut sample = PerfSample::default();
            seq += 1;
            let rid = seq;
            let _ = socket.send(tungstenite::Message::Text(json!({"id": rid, "method": "Runtime.evaluate", "params": {"expression": FPS_EXPR, "returnByValue": true}}).to_string()));
            if let Some(r) = request_response(&mut socket, rid, key, tx, &mut records, &mut seq, port, &mut paused, &mut scripts, debug_sink) {
                sample.fps = r["result"]["value"].as_i64();
            }
            seq += 1;
            let rid = seq;
            let _ = socket.send(tungstenite::Message::Text(json!({"id": rid, "method": "Runtime.getHeapUsage"}).to_string()));
            if let Some(r) = request_response(&mut socket, rid, key, tx, &mut records, &mut seq, port, &mut paused, &mut scripts, debug_sink) {
                sample.heap_used = r["usedSize"].as_i64();
                sample.heap_total = r["totalSize"].as_i64();
            }
            let _ = tx.send(RnEvent::Perf(key.into(), sample));
        }
        // Service UI commands.
        while let Ok(cmd) = crx.try_recv() {
            match cmd {
                ConnCmd::Reload => {
                    let _ = socket.send(tungstenite::Message::Text(json!({"id": 1, "method": "Page.reload"}).to_string()));
                    let _ = socket.send(tungstenite::Message::Text(json!({"id": 2, "method": "ReactNativeApplication.reload"}).to_string()));
                }
                ConnCmd::DiscardConsole => {
                    records.clear();
                    let _ = socket.send(tungstenite::Message::Text(json!({"id": 3, "method": "Runtime.discardConsoleEntries"}).to_string()));
                    let _ = socket.send(tungstenite::Message::Text(json!({"id": 4, "method": "Log.clear"}).to_string()));
                }
                ConnCmd::Continue => debug_cmd(&mut socket, &mut seq, "Debugger.resume"),
                ConnCmd::StepOver => debug_cmd(&mut socket, &mut seq, "Debugger.stepOver"),
                ConnCmd::StepInto => debug_cmd(&mut socket, &mut seq, "Debugger.stepInto"),
                ConnCmd::StepOut => debug_cmd(&mut socket, &mut seq, "Debugger.stepOut"),
                ConnCmd::Pause => debug_cmd(&mut socket, &mut seq, "Debugger.pause"),
                ConnCmd::DebugRequest { method, params, reply } => {
                    seq += 1;
                    let rid = seq;
                    cdp_trace(">>", &method);
                    let _ = socket.send(tungstenite::Message::Text(
                        json!({"id": rid, "method": method, "params": params}).to_string(),
                    ));
                    let result = request_response(&mut socket, rid, key, tx, &mut records, &mut seq, port, &mut paused, &mut scripts, debug_sink)
                        .unwrap_or(Value::Null);
                    let _ = reply.send(result);
                }
            }
        }
        match socket.read() {
            Ok(tungstenite::Message::Text(t)) => {
                cdp_trace("<<", &t);
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    handle_message(key, &v, tx, &mut records, &mut socket, &mut seq, port, &mut paused, &mut scripts, debug_sink);
                }
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    let _ = tx.send(RnEvent::Status(key.into(), Status::Disconnected));
}

/// Send a parameterless Debugger command, allocating a fresh request id.
fn debug_cmd(socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, seq: &mut i64, method: &str) {
    *seq += 1;
    cdp_trace(">>", method);
    let _ = socket.send(tungstenite::Message::Text(json!({"id": *seq, "method": method}).to_string()));
}

/// Learn scriptId → URL from a CDP stackTrace (console/exception frames carry
/// both, unlike Debugger.paused frames which only have a scriptId).
fn learn_scripts(scripts: &mut HashMap<String, String>, st: &Value) {
    if let Some(frames) = st.get("callFrames").and_then(|f| f.as_array()) {
        for f in frames {
            if let (Some(sid), Some(url)) = (f["scriptId"].as_str(), f["url"].as_str()) {
                if !url.is_empty() {
                    scripts.entry(sid.to_string()).or_insert_with(|| url.to_string());
                }
            }
        }
    }
}

/// Like `await_result`, but routes every non-matching frame through
/// `handle_message` instead of dropping it — so async notifications
/// (Debugger.paused/resumed, console, network) that arrive while we're waiting
/// for a request's reply are still processed. Used for the perf sampler, which
/// otherwise silently ate the `Debugger.paused` that follows a step.
#[allow(clippy::too_many_arguments)]
fn request_response(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    id: i64,
    key: &str,
    tx: &Sender<RnEvent>,
    records: &mut HashMap<String, NetRecord>,
    seq: &mut i64,
    port: u16,
    paused: &mut bool,
    scripts: &mut HashMap<String, String>,
    debug_sink: &DebugSink,
) -> Option<Value> {
    for _ in 0..50 {
        match socket.read() {
            Ok(tungstenite::Message::Text(t)) => {
                cdp_trace("<<", &t);
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
                        return v.get("result").cloned();
                    }
                    handle_message(key, &v, tx, records, socket, seq, port, paused, scripts, debug_sink);
                }
            }
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            _ => return None,
        }
    }
    None
}

fn handle_message(
    key: &str,
    msg: &Value,
    tx: &Sender<RnEvent>,
    records: &mut HashMap<String, NetRecord>,
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    seq: &mut i64,
    port: u16,
    paused: &mut bool,
    scripts: &mut HashMap<String, String>,
    debug_sink: &DebugSink,
) {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "Debugger.paused" => {
            *paused = true;
            let frames = msg["params"]["callFrames"].as_array().cloned().unwrap_or_default();
            let reason = msg["params"]["reason"].as_str().unwrap_or("breakpoint").to_string();
            let dap_frames = symbolicate_frames(port, &frames, scripts);
            let (file, line) = dap_frames.first().map(|f| (f.file.clone(), f.line)).unwrap_or_default();
            let _ = tx.send(RnEvent::Paused(key.into(), PausedInfo { file, line, reason: reason.clone() }));
            if let Some(de) = debug_sink.lock().unwrap().as_ref() {
                let _ = de.send(DebugEvent::Paused {
                    frames: dap_frames,
                    reason,
                    raw_frames: msg["params"]["callFrames"].clone(),
                });
            }
        }
        "Debugger.resumed" => {
            *paused = false;
            let _ = tx.send(RnEvent::Resumed(key.into()));
            if let Some(de) = debug_sink.lock().unwrap().as_ref() {
                let _ = de.send(DebugEvent::Resumed);
            }
        }
        "Debugger.scriptParsed" => {
            let p = &msg["params"];
            if let (Some(sid), Some(url)) = (p["scriptId"].as_str(), p["url"].as_str()) {
                if !url.is_empty() {
                    scripts.entry(sid.to_string()).or_insert_with(|| url.to_string());
                }
            }
        }
        "Runtime.consoleAPICalled" => {
            let p = &msg["params"];
            learn_scripts(scripts, &p["stackTrace"]);
            let level = p["type"].as_str().unwrap_or("log").to_string();
            let args = p["args"].as_array().cloned().unwrap_or_default();
            let text = args.iter().map(render_arg).collect::<Vec<_>>().join(" ");
            // Eagerly expand the first object arg into a JS tree via getProperties.
            let mut expanded = None;
            for a in &args {
                if a.get("type").and_then(|t| t.as_str()) == Some("object") {
                    if let Some(oid) = a.get("objectId").and_then(|o| o.as_str()) {
                        if let Some(tree) = get_object_tree(socket, seq, oid, 4) {
                            expanded = Some(tree);
                        }
                        break;
                    }
                }
            }
            let stack = stack_frames(&p["stackTrace"], port);
            let _ = tx.send(RnEvent::Log(key.into(), LogEntry { kind: "console".into(), level, text, expanded, stack }));
        }
        "Log.entryAdded" => {
            let e = &msg["params"]["entry"];
            let _ = tx.send(RnEvent::Log(
                key.into(),
                LogEntry { kind: "console".into(), level: e["level"].as_str().unwrap_or("log").into(), text: e["text"].as_str().unwrap_or("").into(), expanded: None, stack: stack_frames(&e["stackTrace"], port) },
            ));
        }
        "Runtime.exceptionThrown" => {
            let d = &msg["params"]["exceptionDetails"];
            learn_scripts(scripts, &d["stackTrace"]);
            let text = d["exception"]["description"].as_str().or_else(|| d["text"].as_str()).unwrap_or("Uncaught exception").to_string();
            let stack = stack_frames(&d["stackTrace"], port);
            let _ = tx.send(RnEvent::Log(key.into(), LogEntry { kind: "console".into(), level: "error".into(), text, expanded: None, stack }));
        }
        "Runtime.executionContextsCleared" => {
            records.clear();
            let _ = tx.send(RnEvent::ContextCleared(key.into()));
        }
        m if m.starts_with("Network.") => handle_network(key, m, &msg["params"], tx, records, socket, seq),
        _ => {}
    }
}

fn handle_network(
    key: &str,
    method: &str,
    p: &Value,
    tx: &Sender<RnEvent>,
    records: &mut HashMap<String, NetRecord>,
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    seq: &mut i64,
) {
    let id = p["requestId"].as_str().unwrap_or("").to_string();
    match method {
        "Network.requestWillBeSent" => {
            let r = &p["request"];
            let rec = NetRecord {
                id: id.clone(),
                method: r["method"].as_str().unwrap_or("GET").into(),
                url: r["url"].as_str().unwrap_or("").into(),
                req_headers: headers(&r["headers"]),
                req_body: r["postData"].as_str().map(String::from),
                start_ts: p["timestamp"].as_f64(),
                ..Default::default()
            };
            records.insert(id.clone(), rec.clone());
            let _ = tx.send(RnEvent::Net(key.into(), rec));
        }
        "Network.responseReceived" => {
            if let Some(rec) = records.get_mut(&id) {
                let res = &p["response"];
                rec.status = res["status"].as_i64();
                rec.res_headers = headers(&res["headers"]);
                rec.mime_type = res["mimeType"].as_str().map(String::from);
                let _ = tx.send(RnEvent::Net(key.into(), rec.clone()));
            }
        }
        "Network.loadingFinished" => {
            if let Some(rec) = records.get_mut(&id) {
                if let (Some(start), Some(end)) = (rec.start_ts, p["timestamp"].as_f64()) {
                    rec.duration_ms = Some(((end - start) * 1000.0).max(0.0));
                }
                rec.size = p["encodedDataLength"].as_f64().map(|b| b as i64);
                // Proactively fetch the response body (request/response correlated inline).
                *seq += 1;
                let req_id = *seq;
                let _ = socket.send(tungstenite::Message::Text(json!({"id": req_id, "method": "Network.getResponseBody", "params": {"requestId": id}}).to_string()));
                if let Some(result) = await_result(socket, req_id) {
                    let body = result["body"].as_str().unwrap_or("");
                    let decoded = if result["base64Encoded"].as_bool().unwrap_or(false) {
                        use base64_lite::decode;
                        decode(body)
                    } else {
                        body.to_string()
                    };
                    rec.res_body = Some(decoded);
                }
                let _ = tx.send(RnEvent::Net(key.into(), rec.clone()));
            }
        }
        "Network.loadingFailed" => {
            if let Some(rec) = records.get_mut(&id) {
                if let (Some(start), Some(end)) = (rec.start_ts, p["timestamp"].as_f64()) {
                    rec.duration_ms = Some(((end - start) * 1000.0).max(0.0));
                }
                rec.failed = Some(p["errorText"].as_str().unwrap_or("request failed").to_string());
                let _ = tx.send(RnEvent::Net(key.into(), rec.clone()));
            }
        }
        // WebSocket traffic (GraphQL subscriptions, live sockets).
        "Network.webSocketCreated" => {
            let rec = NetRecord { id: id.clone(), method: "WS".into(), url: p["url"].as_str().unwrap_or("").into(), is_ws: true, start_ts: p["timestamp"].as_f64(), ..Default::default() };
            records.insert(id.clone(), rec.clone());
            let _ = tx.send(RnEvent::Net(key.into(), rec));
        }
        "Network.webSocketFrameSent" | "Network.webSocketFrameReceived" => {
            if let Some(rec) = records.get_mut(&id) {
                let sent = method == "Network.webSocketFrameSent";
                let fr = &p["response"];
                let payload = match fr["opcode"].as_i64() {
                    Some(1) | None => fr["payloadData"].as_str().unwrap_or("").to_string(),
                    Some(2) => format!("[binary {} bytes]", fr["payloadData"].as_str().map(|s| s.len()).unwrap_or(0)),
                    Some(op) => format!("[opcode {op}]"),
                };
                rec.ws_frames.push((sent, payload));
                let _ = tx.send(RnEvent::Net(key.into(), rec.clone()));
            }
        }
        _ => {}
    }
}

/// Flatten a CDP stackTrace into `function (url:line)` strings.
/// Keep only the last few path segments so frames read like `src/utils/log.ts`.
fn shorten_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() > 3 {
        parts[parts.len() - 3..].join("/")
    } else {
        p.to_string()
    }
}

fn frame_line(name: &str, file: &str, line: i64) -> String {
    let name = if name.is_empty() { "<anonymous>" } else { name };
    format!("  at {name} ({}:{line})", shorten_path(file))
}

/// Turn a CDP stackTrace into readable lines. Tries Metro's `/symbolicate` to map
/// bundle positions back to source files (dropping collapsed framework frames);
/// falls back to the raw bundle positions if symbolication isn't available.
fn stack_frames(st: &Value, port: u16) -> Option<Vec<String>> {
    let frames = st.get("callFrames")?.as_array()?;
    if frames.is_empty() {
        return None;
    }
    if let Some(sym) = symbolicate(port, frames) {
        if !sym.is_empty() {
            return Some(sym);
        }
    }
    // Fallback: raw bundle positions.
    Some(
        frames
            .iter()
            .map(|f| {
                let name = f["functionName"].as_str().unwrap_or("");
                let url = f["url"].as_str().unwrap_or("");
                let line = f["lineNumber"].as_i64().map(|l| l + 1).unwrap_or(0);
                frame_line(name, url.rsplit('/').next().unwrap_or(url), line)
            })
            .collect(),
    )
}

/// POST a prebuilt symbolicate `stack` to Metro's `/symbolicate` and return the
/// mapped frames (each with `methodName`/`file`/`lineNumber`/`collapse`).
fn post_symbolicate(port: u16, stack: Vec<Value>) -> Option<Vec<Value>> {
    let url = format!("http://localhost:{port}/symbolicate");
    let resp = reqwest::blocking::Client::new().post(&url).json(&json!({ "stack": stack })).send().ok()?;
    let body: Value = resp.json().ok()?;
    body.get("stack")?.as_array().cloned()
}

/// Symbolicate a Runtime stackTrace (line/col top-level) into readable frame
/// lines, dropping collapsed framework frames. CDP positions are 0-based; Metro
/// wants a 1-based line.
fn symbolicate(port: u16, frames: &[Value]) -> Option<Vec<String>> {
    let req: Vec<Value> = frames
        .iter()
        .map(|f| {
            json!({
                "file": f["url"].as_str().unwrap_or(""),
                "lineNumber": f["lineNumber"].as_i64().unwrap_or(0) + 1,
                "column": f["columnNumber"].as_i64().unwrap_or(0),
                "methodName": f["functionName"].as_str().unwrap_or(""),
            })
        })
        .collect();
    let mapped = post_symbolicate(port, req)?;
    let out: Vec<String> = mapped
        .iter()
        .filter(|f| f.get("collapse").and_then(|c| c.as_bool()) != Some(true)) // drop framework frames
        .map(|f| {
            let name = f["methodName"].as_str().unwrap_or("");
            let file = f["file"].as_str().unwrap_or("");
            let line = f["lineNumber"].as_i64().unwrap_or(0);
            frame_line(name, file, line)
        })
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Map a `Debugger.paused` callFrame list to the top original `(file, line)`.
/// CDP Debugger frames carry line/col under `location`; the script URL is
/// top-level. Falls back to the raw bundle basename + line when symbolication
/// isn't available.
/// Symbolicate `Debugger.paused` callFrames into DAP stack frames (original
/// source locations). Debugger frames carry only a scriptId, so the URL is
/// resolved from the learned `scripts` map. Falls back to raw bundle positions
/// per frame when the symbolicator can't map them.
fn symbolicate_frames(port: u16, frames: &[Value], scripts: &HashMap<String, String>) -> Vec<DapFrame> {
    let frame_url = |f: &Value| -> String {
        let sid = f["location"]["scriptId"].as_str().unwrap_or("");
        scripts
            .get(sid)
            .map(|s| s.as_str())
            .or_else(|| f["url"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let name_of = |n: &str| if n.is_empty() { "<anonymous>".to_string() } else { n.to_string() };

    // Per-frame raw fallback (bundle basename + 1-based line/col).
    let raw: Vec<DapFrame> = frames
        .iter()
        .map(|f| DapFrame {
            name: name_of(f["functionName"].as_str().unwrap_or("")),
            file: frame_url(f),
            line: f["location"]["lineNumber"].as_i64().unwrap_or(0) + 1,
            column: f["location"]["columnNumber"].as_i64().unwrap_or(0) + 1,
        })
        .collect();

    let req: Vec<Value> = frames
        .iter()
        .map(|f| {
            json!({
                "file": frame_url(f),
                "lineNumber": f["location"]["lineNumber"].as_i64().unwrap_or(0) + 1,
                "column": f["location"]["columnNumber"].as_i64().unwrap_or(0),
                "methodName": f["functionName"].as_str().unwrap_or(""),
            })
        })
        .collect();

    if let Some(mapped) = post_symbolicate(port, req) {
        let out: Vec<DapFrame> = mapped
            .iter()
            .filter(|f| f.get("collapse").and_then(|c| c.as_bool()) != Some(true))
            .filter_map(|f| {
                let file = f["file"].as_str().unwrap_or("");
                if file.is_empty() {
                    return None;
                }
                Some(DapFrame {
                    name: name_of(f["methodName"].as_str().unwrap_or("")),
                    file: file.to_string(),
                    line: f["lineNumber"].as_i64().unwrap_or(0),
                    column: f["column"].as_i64().unwrap_or(0) + 1,
                })
            })
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    raw
}

/// Read frames until the response with `id` arrives (bounded), returning its `result`.
fn await_result(socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, id: i64) -> Option<Value> {
    for _ in 0..50 {
        match socket.read() {
            Ok(tungstenite::Message::Text(t)) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
                        return v.get("result").cloned();
                    }
                }
                // Non-matching frame read mid-request is silently discarded here —
                // trace it so we can see if a Debugger.paused gets eaten.
                cdp_trace("DROP", &t);
            }
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            _ => return None,
        }
    }
    None
}

/// Deep-expand an object via Runtime.getProperties into pretty JS lines.
fn get_object_tree(socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, seq: &mut i64, object_id: &str, depth: i32) -> Option<Vec<String>> {
    let val = build_value(socket, seq, object_id, depth)?;
    Some(format_js(&val, "").lines().map(String::from).collect())
}

fn build_value(socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, seq: &mut i64, object_id: &str, depth: i32) -> Option<Value> {
    *seq += 1;
    let req_id = *seq;
    let _ = socket.send(tungstenite::Message::Text(json!({"id": req_id, "method": "Runtime.getProperties", "params": {"objectId": object_id, "ownProperties": true}}).to_string()));
    let result = await_result(socket, req_id)?;
    let props = result["result"].as_array().cloned().unwrap_or_default();
    let mut map = serde_json::Map::new();
    let mut is_array = true;
    for p in props.iter() {
        if p["enumerable"].as_bool() == Some(false) {
            continue;
        }
        let name = p["name"].as_str().unwrap_or("").to_string();
        if name.parse::<usize>().is_err() {
            is_array = false;
        }
    }
    for p in props {
        if p["enumerable"].as_bool() == Some(false) || p.get("value").is_none() {
            continue;
        }
        let name = p["name"].as_str().unwrap_or("").to_string();
        if name == "length" && is_array {
            continue;
        }
        let v = &p["value"];
        let child = if v["type"].as_str() == Some("object") && v.get("objectId").is_some() && depth > 0 {
            build_value(socket, seq, v["objectId"].as_str().unwrap(), depth - 1).unwrap_or(Value::String("{…}".into()))
        } else {
            primitive(v)
        };
        map.insert(name, child);
    }
    if is_array && !map.is_empty() {
        let mut items: Vec<(usize, Value)> = map.into_iter().filter_map(|(k, v)| k.parse::<usize>().ok().map(|i| (i, v))).collect();
        items.sort_by_key(|(i, _)| *i);
        Some(Value::Array(items.into_iter().map(|(_, v)| v).collect()))
    } else {
        Some(Value::Object(map))
    }
}

fn primitive(v: &Value) -> Value {
    if let Some(val) = v.get("value") {
        return val.clone();
    }
    if v["type"].as_str() == Some("undefined") {
        return Value::String("undefined".into());
    }
    if let Some(d) = v["description"].as_str() {
        return Value::String(d.into());
    }
    Value::Null
}

/// Pretty-print a reconstructed value as a JS literal.
pub fn format_js(value: &Value, indent: &str) -> String {
    let next = format!("{indent}  ");
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("{s:?}"),
        Value::Array(a) if a.is_empty() => "[]".into(),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(|v| format!("{next}{}", format_js(v, &next))).collect();
            format!("[\n{}\n{indent}]", items.join(",\n"))
        }
        Value::Object(o) if o.is_empty() => "{}".into(),
        Value::Object(o) => {
            let items: Vec<String> = o.iter().map(|(k, v)| format!("{next}{}: {}", k, format_js(v, &next))).collect();
            format!("{{\n{}\n{indent}}}", items.join(",\n"))
        }
    }
}

fn render_arg(a: &Value) -> String {
    if let Some(v) = a.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(preview) = a.get("preview") {
        return preview_to_string(preview);
    }
    if let Some(d) = a["description"].as_str() {
        return d.to_string();
    }
    a["type"].as_str().unwrap_or("").to_string()
}

fn preview_to_string(p: &Value) -> String {
    let subtype_array = p["subtype"].as_str() == Some("array");
    let props = p["properties"].as_array().cloned().unwrap_or_default();
    let parts: Vec<String> = props
        .iter()
        .map(|pr| {
            let val = if pr.get("valuePreview").is_some() {
                preview_to_string(&pr["valuePreview"])
            } else {
                pr["value"].as_str().map(String::from).unwrap_or_else(|| pr["type"].as_str().unwrap_or("").to_string())
            };
            if subtype_array {
                val
            } else {
                format!("{}: {}", pr["name"].as_str().unwrap_or(""), val)
            }
        })
        .collect();
    if subtype_array {
        format!("[{}]", parts.join(", "))
    } else {
        format!("{{ {} }}", parts.join(", "))
    }
}

fn headers(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|o| o.iter().map(|(k, val)| (k.clone(), val.as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default()
}

// Minimal base64 decode for response bodies (avoids another dependency).
mod base64_lite {
    pub fn decode(s: &str) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut lut = [255u8; 256];
        for (i, &c) in T.iter().enumerate() {
            lut[c as usize] = i as u8;
        }
        let mut out = Vec::new();
        let mut buf = 0u32;
        let mut bits = 0;
        for &c in s.as_bytes() {
            if c == b'=' || lut[c as usize] == 255 {
                continue;
            }
            buf = (buf << 6) | lut[c as usize] as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}
