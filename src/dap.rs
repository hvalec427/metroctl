//! A minimal Debug Adapter Protocol server. Any DAP client (nvim-dap, VSCode)
//! connects over TCP and drives the single Hermes debugger this metroctl owns —
//! so the editor shows the paused code + call stack while metroctl stays the
//! sole CDP client. Started from `RnClient::start`, listening on 9223 by default
//! (override with METROCTL_DAP_PORT).
//!
//! Phase 2a: attach, stop/continue/step, stackTrace from the paused frames.
//! Breakpoints set from the editor (source-map mapping) and scopes/variables
//! come next; for now `debugger;` statements drive the stops.

use super::{ConnCmd, DebugEvent, DebugSink, UrlSlot};
use serde_json::{json, Value};
use sourcemap::SourceMap;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Senders = Arc<Mutex<HashMap<String, Sender<ConnCmd>>>>;
type Writer = Arc<Mutex<TcpStream>>;
type Seq = Arc<Mutex<i64>>;

pub fn serve(senders: Senders, debug_sink: DebugSink, bundle: UrlSlot) {
    let port: u16 = std::env::var("METROCTL_DAP_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(9223);
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        // Port busy (e.g. another metroctl already listening) — just skip.
        Err(_) => return,
    };
    for stream in listener.incoming().flatten() {
        let senders = senders.clone();
        let sink = debug_sink.clone();
        let bundle = bundle.clone();
        std::thread::spawn(move || {
            let _ = session(stream, senders, sink, bundle);
        });
    }
}

fn session(stream: TcpStream, senders: Senders, debug_sink: DebugSink, bundle: UrlSlot) -> std::io::Result<()> {
    let writer: Writer = Arc::new(Mutex::new(stream.try_clone()?));
    let seq: Seq = Arc::new(Mutex::new(1));
    let frames: Arc<Mutex<Vec<super::DapFrame>>> = Arc::new(Mutex::new(Vec::new()));
    // Raw CDP callFrames of the current pause (scopeChain + objectIds for
    // scopes/variables). variablesReference → CDP objectId, rebuilt per pause
    // since objectIds die on resume.
    let raw: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let var_refs: Arc<Mutex<HashMap<i64, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let next_ref: Arc<Mutex<i64>> = Arc::new(Mutex::new(1000));
    // Parsed source map for the current bundle (url, map), and the CDP
    // breakpointIds we've set per source file (so we can replace them).
    let mut smap: Option<(String, SourceMap)> = None;
    let mut bps_by_src: HashMap<String, Vec<String>> = HashMap::new();
    let mut reader = BufReader::new(stream);

    let (de_tx, de_rx) = channel::<DebugEvent>();
    let mut de_rx = Some(de_rx);

    while let Some(msg) = read_message(&mut reader)? {
        let command = msg["command"].as_str().unwrap_or("").to_string();
        let req_seq = msg["seq"].as_i64().unwrap_or(0);
        match command.as_str() {
            "initialize" => {
                respond(&writer, &seq, req_seq, &command, json!({
                    "supportsConfigurationDoneRequest": true,
                    "supportsTerminateRequest": true,
                    "supportsConditionalBreakpoints": true,
                    "supportsEvaluateForHovers": true,
                    "exceptionBreakpointFilters": [
                        { "filter": "all", "label": "All Exceptions" },
                        { "filter": "uncaught", "label": "Uncaught Exceptions", "default": true },
                    ],
                }));
                event(&writer, &seq, "initialized", json!({}));
            }
            "attach" | "launch" => {
                let _ = wait_for_target(&senders);
                // Register our event stream in the shared slot; every current
                // and future Hermes connection fans Debugger events here, so a
                // Metro reload doesn't detach us.
                *debug_sink.lock().unwrap() = Some(de_tx.clone());
                // Pump debugger notifications → DAP events (once).
                if let Some(rx) = de_rx.take() {
                    let (w, sq, fr) = (writer.clone(), seq.clone(), frames.clone());
                    let (rawc, refs) = (raw.clone(), var_refs.clone());
                    std::thread::spawn(move || {
                        for ev in rx {
                            match ev {
                                DebugEvent::Paused { frames: f, reason, raw_frames } => {
                                    *fr.lock().unwrap() = f;
                                    *rawc.lock().unwrap() = raw_frames;
                                    refs.lock().unwrap().clear(); // objectIds are per-pause
                                    event(&w, &sq, "stopped", json!({
                                        "reason": map_reason(&reason),
                                        "threadId": 1,
                                        "allThreadsStopped": true,
                                    }));
                                }
                                DebugEvent::Resumed => {
                                    refs.lock().unwrap().clear();
                                    event(&w, &sq, "continued", json!({"threadId": 1, "allThreadsContinued": true}));
                                }
                                DebugEvent::Output { category, output } => {
                                    event(&w, &sq, "output", json!({ "category": category, "output": output }));
                                }
                            }
                        }
                    });
                }
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "configurationDone" => respond(&writer, &seq, req_seq, &command, json!({})),
            "setBreakpoints" => {
                let src_path = msg["arguments"]["source"]["path"].as_str().unwrap_or("").to_string();
                let want = msg["arguments"]["breakpoints"].as_array().cloned().unwrap_or_default();

                // Clear this file's previous breakpoints before re-setting.
                if let Some(old) = bps_by_src.remove(&src_path) {
                    for id in old {
                        let _ = debug_request(&senders, "Debugger.removeBreakpoint", json!({ "breakpointId": id }));
                    }
                }

                ensure_sourcemap(&bundle, &mut smap);

                let mut out = Vec::new();
                let mut ids = Vec::new();
                for b in &want {
                    let line = b["line"].as_i64().unwrap_or(0);
                    let condition = b["condition"].as_str().filter(|c| !c.is_empty());
                    // Map original .tsx line → generated bundle position.
                    let mapped = smap.as_ref().and_then(|(_, sm)| map_breakpoint(sm, &src_path, line));
                    match mapped {
                        Some((gline, gcol)) => {
                            let mut params = json!({
                                "urlRegex": "index\\.bundle",
                                "lineNumber": gline,
                                "columnNumber": gcol,
                            });
                            if let Some(c) = condition {
                                params["condition"] = json!(c);
                            }
                            let res = debug_request(&senders, "Debugger.setBreakpointByUrl", params);
                            let bound = res["locations"].as_array().map(|a| !a.is_empty()).unwrap_or(false);
                            if let Some(id) = res["breakpointId"].as_str() {
                                ids.push(id.to_string());
                            }
                            out.push(json!({ "verified": bound, "line": line }));
                        }
                        None => out.push(json!({ "verified": false, "line": line })),
                    }
                }
                if !ids.is_empty() {
                    bps_by_src.insert(src_path, ids);
                }
                respond(&writer, &seq, req_seq, &command, json!({ "breakpoints": out }));
            }
            "setExceptionBreakpoints" => {
                let filters = msg["arguments"]["filters"].as_array().cloned().unwrap_or_default();
                let has = |f: &str| filters.iter().any(|x| x.as_str() == Some(f));
                let state = if has("all") {
                    "all"
                } else if has("uncaught") {
                    "uncaught"
                } else {
                    "none"
                };
                debug_request(&senders, "Debugger.setPauseOnExceptions", json!({ "state": state }));
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "evaluate" => {
                let expr = msg["arguments"]["expression"].as_str().unwrap_or("").to_string();
                // Evaluate in the selected frame when paused, else globally.
                let call_frame_id = msg["arguments"]["frameId"]
                    .as_i64()
                    .and_then(|i| raw.lock().unwrap().get(i as usize).and_then(|f| f["callFrameId"].as_str().map(String::from)));
                let res = if let Some(cfid) = call_frame_id {
                    debug_request(&senders, "Debugger.evaluateOnCallFrame", json!({
                        "callFrameId": cfid, "expression": expr,
                        "includeCommandLineAPI": true, "generatePreview": true, "silent": true,
                    }))
                } else {
                    debug_request(&senders, "Runtime.evaluate", json!({
                        "expression": expr,
                        "includeCommandLineAPI": true, "generatePreview": true, "silent": true,
                    }))
                };
                if res["exceptionDetails"].is_object() {
                    let m = res["exceptionDetails"]["exception"]["description"]
                        .as_str()
                        .or_else(|| res["exceptionDetails"]["text"].as_str())
                        .unwrap_or("evaluation error");
                    respond_fail(&writer, &seq, req_seq, &command, m);
                } else {
                    let (display, child) = describe_value(&res["result"], &var_refs, &next_ref);
                    respond(&writer, &seq, req_seq, &command, json!({ "result": display, "variablesReference": child }));
                }
            }
            "threads" => {
                respond(&writer, &seq, req_seq, &command, json!({"threads": [{"id": 1, "name": "Hermes"}]}));
            }
            "stackTrace" => {
                let fr = frames.lock().unwrap();
                let sframes: Vec<Value> = fr.iter().enumerate().map(|(i, f)| {
                    // Only frames that map to a real local source file are
                    // navigable. Internal/bundle frames come back as http(s)
                    // URLs — the editor must NOT try to open those (it fetches
                    // the whole bundle and the generated line is out of range).
                    let navigable = !f.file.is_empty() && !f.file.contains("://");
                    if navigable {
                        json!({
                            "id": i,
                            "name": f.name,
                            "line": f.line,
                            "column": f.column.max(1),
                            "source": { "path": f.file, "name": basename(&f.file) },
                        })
                    } else {
                        json!({
                            "id": i,
                            "name": f.name,
                            "line": 0,
                            "column": 0,
                            "presentationHint": "subtle",
                        })
                    }
                }).collect();
                let total = sframes.len();
                respond(&writer, &seq, req_seq, &command, json!({"stackFrames": sframes, "totalFrames": total}));
            }
            "scopes" => {
                let frame_id = msg["arguments"]["frameId"].as_i64().unwrap_or(0);
                let chain = raw
                    .lock()
                    .unwrap()
                    .get(frame_id as usize)
                    .and_then(|f| f["scopeChain"].as_array().cloned())
                    .unwrap_or_default();
                let scopes: Vec<Value> = chain
                    .iter()
                    .filter_map(|sc| {
                        let oid = sc["object"]["objectId"].as_str().filter(|s| !s.is_empty())?;
                        let kind = sc["type"].as_str().unwrap_or("scope");
                        let name = sc["name"].as_str().map(|s| s.to_string()).unwrap_or_else(|| cap(kind));
                        Some(json!({
                            "name": name,
                            "variablesReference": alloc_ref(&var_refs, &next_ref, oid),
                            "expensive": kind == "global",
                            "presentationHint": kind,
                        }))
                    })
                    .collect();
                respond(&writer, &seq, req_seq, &command, json!({"scopes": scopes}));
            }
            "variables" => {
                let vref = msg["arguments"]["variablesReference"].as_i64().unwrap_or(0);
                let object_id = var_refs.lock().unwrap().get(&vref).cloned();
                let mut vars = Vec::new();
                if let Some(oid) = object_id {
                    let result = debug_request(&senders, "Runtime.getProperties", json!({"objectId": oid, "ownProperties": true, "generatePreview": true}));
                    if let Some(props) = result["result"].as_array() {
                        for p in props {
                            if p["enumerable"].as_bool() == Some(false) {
                                continue;
                            }
                            let name = p["name"].as_str().unwrap_or("").to_string();
                            let (display, child) = describe_value(&p["value"], &var_refs, &next_ref);
                            vars.push(json!({ "name": name, "value": display, "variablesReference": child }));
                        }
                    }
                }
                respond(&writer, &seq, req_seq, &command, json!({"variables": vars}));
            }
            "continue" => {
                step(&senders, ConnCmd::Continue);
                respond(&writer, &seq, req_seq, &command, json!({"allThreadsContinued": true}));
            }
            "next" => {
                step(&senders, ConnCmd::StepOver);
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "stepIn" => {
                step(&senders, ConnCmd::StepInto);
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "stepOut" => {
                step(&senders, ConnCmd::StepOut);
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "pause" => {
                step(&senders, ConnCmd::Pause);
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "disconnect" | "terminate" => {
                respond(&writer, &seq, req_seq, &command, json!({}));
                break;
            }
            // Unknown request: ack so the client doesn't stall.
            other => respond(&writer, &seq, req_seq, other, json!({})),
        }
    }
    // Editor disconnected — stop fanning events to a dead stream.
    *debug_sink.lock().unwrap() = None;
    Ok(())
}

fn map_reason(cdp: &str) -> &'static str {
    match cdp {
        "exception" | "promiseRejection" => "exception",
        "step" => "step",
        "debugCommand" => "pause",
        _ => "breakpoint",
    }
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

fn wait_for_target(senders: &Senders) -> Option<String> {
    for _ in 0..100 {
        if let Some(k) = senders.lock().unwrap().keys().next().cloned() {
            return Some(k);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Send a command to the current (single) Hermes target. Resolving the sender
/// fresh each call means step/continue survive a Metro reload that swaps the
/// underlying connection.
fn step(senders: &Senders, cmd: ConnCmd) {
    if let Some(s) = senders.lock().unwrap().values().next() {
        let _ = s.send(cmd);
    }
}

/// Synchronous CDP call via the current target; returns the CDP `result` (or Null).
fn debug_request(senders: &Senders, method: &str, params: Value) -> Value {
    let (tx, rx) = channel::<Value>();
    {
        let map = senders.lock().unwrap();
        match map.values().next() {
            Some(s) => {
                let _ = s.send(ConnCmd::DebugRequest { method: method.to_string(), params, reply: tx });
            }
            None => return Value::Null,
        }
    }
    rx.recv_timeout(Duration::from_secs(3)).unwrap_or(Value::Null)
}

/// Allocate a fresh variablesReference pointing at a CDP objectId.
fn alloc_ref(refs: &Arc<Mutex<HashMap<i64, String>>>, next: &Arc<Mutex<i64>>, object_id: &str) -> i64 {
    let id = {
        let mut n = next.lock().unwrap();
        let v = *n;
        *n += 1;
        v
    };
    refs.lock().unwrap().insert(id, object_id.to_string());
    id
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A CDP RemoteObject → (display string, child variablesReference). Objects and
/// functions with an objectId are expandable; everything else is a leaf (ref 0).
fn describe_value(v: &Value, refs: &Arc<Mutex<HashMap<i64, String>>>, next: &Arc<Mutex<i64>>) -> (String, i64) {
    let ty = v["type"].as_str().unwrap_or("");
    let display = if let Some(s) = v["value"].as_str() {
        format!("\"{s}\"")
    } else if !v["value"].is_null() {
        v["value"].to_string()
    } else if let Some(d) = v["description"].as_str() {
        d.to_string()
    } else if ty == "undefined" {
        "undefined".to_string()
    } else {
        ty.to_string()
    };
    let child = if (ty == "object" || ty == "function") && v["objectId"].is_string() {
        alloc_ref(refs, next, v["objectId"].as_str().unwrap())
    } else {
        0
    };
    (display, child)
}

/// Fetch + parse the bundle's source map (cached by bundle URL). The map lives
/// next to the bundle at the same path with `.bundle` → `.map`.
fn ensure_sourcemap(bundle: &UrlSlot, cache: &mut Option<(String, SourceMap)>) {
    let url = match bundle.lock().unwrap().clone() {
        Some(u) => u,
        None => return,
    };
    if cache.as_ref().map(|(u, _)| u == &url).unwrap_or(false) {
        return; // already have this bundle's map
    }
    let map_url = url.replacen(".bundle", ".map", 1);
    if let Ok(resp) = reqwest::blocking::get(&map_url) {
        if let Ok(bytes) = resp.bytes() {
            if let Ok(sm) = SourceMap::from_slice(&bytes) {
                *cache = Some((url, sm));
            }
        }
    }
}

/// The source-map `sources` entry sharing the longest trailing path with `path`
/// (requires at least the basename to match), so an editor's absolute path finds
/// the right source regardless of how Metro spells it.
fn best_source(sm: &SourceMap, path: &str) -> Option<String> {
    let p: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let mut best: Option<(usize, String)> = None;
    for s in sm.sources() {
        let sc: Vec<&str> = s.split('/').filter(|c| !c.is_empty()).collect();
        let mut n = 0;
        while n < p.len() && n < sc.len() && p[p.len() - 1 - n] == sc[sc.len() - 1 - n] {
            n += 1;
        }
        if n == 0 {
            continue; // not even the filename matches
        }
        if best.as_ref().map(|(bn, _)| n > *bn).unwrap_or(true) {
            best = Some((n, s.to_string()));
        }
    }
    best.map(|(_, s)| s)
}

/// Map an original (file, 1-based line) to the generated (line, col) in the
/// bundle: the first mapping token on or after that line for the matched source.
fn map_breakpoint(sm: &SourceMap, path: &str, dap_line: i64) -> Option<(u32, u32)> {
    if dap_line <= 0 {
        return None;
    }
    let source = best_source(sm, path)?;
    let want = (dap_line - 1) as u32; // source maps are 0-based
    let mut best: Option<(u32, u32, u32)> = None; // (src_line, dst_line, dst_col)
    for tok in sm.tokens() {
        if tok.get_source() != Some(source.as_str()) {
            continue;
        }
        let sl = tok.get_src_line();
        if sl < want {
            continue;
        }
        let cand = (sl, tok.get_dst_line(), tok.get_dst_col());
        if best.map(|b| cand < b).unwrap_or(true) {
            best = Some(cand);
        }
    }
    best.map(|(_, dl, dc)| (dl, dc))
}

// ── DAP wire protocol (Content-Length framed JSON, like LSP) ──────────────────

fn read_message(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Value>> {
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF — client disconnected
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // blank line terminates headers
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf).ok())
}

fn write_msg(writer: &Writer, seq: &Seq, mut msg: Value) {
    let s = {
        let mut g = seq.lock().unwrap();
        let v = *g;
        *g += 1;
        v
    };
    msg["seq"] = json!(s);
    let body = msg.to_string();
    let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    if let Ok(mut w) = writer.lock() {
        let _ = w.write_all(framed.as_bytes());
        let _ = w.flush();
    }
}

fn respond(writer: &Writer, seq: &Seq, req_seq: i64, command: &str, body: Value) {
    write_msg(writer, seq, json!({
        "type": "response",
        "request_seq": req_seq,
        "success": true,
        "command": command,
        "body": body,
    }));
}

fn respond_fail(writer: &Writer, seq: &Seq, req_seq: i64, command: &str, message: &str) {
    write_msg(writer, seq, json!({
        "type": "response",
        "request_seq": req_seq,
        "success": false,
        "command": command,
        "message": message,
    }));
}

fn event(writer: &Writer, seq: &Seq, event: &str, body: Value) {
    write_msg(writer, seq, json!({
        "type": "event",
        "event": event,
        "body": body,
    }));
}
