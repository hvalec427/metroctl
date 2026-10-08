//! A minimal Debug Adapter Protocol server. Any DAP client (nvim-dap, VSCode)
//! connects over TCP and drives the single Hermes debugger this metroctl owns —
//! so the editor shows the paused code + call stack while metroctl stays the
//! sole CDP client. Started from `RnClient::start`, listening on 9223 by default
//! (override with METROCTL_DAP_PORT).
//!
//! Phase 2a: attach, stop/continue/step, stackTrace from the paused frames.
//! Breakpoints set from the editor (source-map mapping) and scopes/variables
//! come next; for now `debugger;` statements drive the stops.

use super::{ConnCmd, DebugEvent};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Senders = Arc<Mutex<HashMap<String, Sender<ConnCmd>>>>;
type Writer = Arc<Mutex<TcpStream>>;
type Seq = Arc<Mutex<i64>>;

pub fn serve(senders: Senders) {
    let port: u16 = std::env::var("METROCTL_DAP_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(9223);
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        // Port busy (e.g. another metroctl already listening) — just skip.
        Err(_) => return,
    };
    for stream in listener.incoming().flatten() {
        let senders = senders.clone();
        std::thread::spawn(move || {
            let _ = session(stream, senders);
        });
    }
}

fn session(stream: TcpStream, senders: Senders) -> std::io::Result<()> {
    let writer: Writer = Arc::new(Mutex::new(stream.try_clone()?));
    let seq: Seq = Arc::new(Mutex::new(1));
    let frames: Arc<Mutex<Vec<super::DapFrame>>> = Arc::new(Mutex::new(Vec::new()));
    let mut reader = BufReader::new(stream);

    let (de_tx, de_rx) = channel::<DebugEvent>();
    let mut de_rx = Some(de_rx);
    let mut target: Option<String> = None;

    while let Some(msg) = read_message(&mut reader)? {
        let command = msg["command"].as_str().unwrap_or("").to_string();
        let req_seq = msg["seq"].as_i64().unwrap_or(0);
        match command.as_str() {
            "initialize" => {
                respond(&writer, &seq, req_seq, &command, json!({
                    "supportsConfigurationDoneRequest": true,
                    "supportsTerminateRequest": true,
                }));
                event(&writer, &seq, "initialized", json!({}));
            }
            "attach" | "launch" => {
                target = wait_for_target(&senders);
                if let Some(k) = &target {
                    send_cmd(&senders, k, ConnCmd::AttachDebugger { events: de_tx.clone() });
                }
                // Pump debugger notifications → DAP events (once).
                if let Some(rx) = de_rx.take() {
                    let (w, sq, fr) = (writer.clone(), seq.clone(), frames.clone());
                    std::thread::spawn(move || {
                        for ev in rx {
                            match ev {
                                DebugEvent::Paused { frames: f, reason } => {
                                    *fr.lock().unwrap() = f;
                                    event(&w, &sq, "stopped", json!({
                                        "reason": map_reason(&reason),
                                        "threadId": 1,
                                        "allThreadsStopped": true,
                                    }));
                                }
                                DebugEvent::Resumed => {
                                    event(&w, &sq, "continued", json!({"threadId": 1, "allThreadsContinued": true}));
                                }
                                DebugEvent::Terminated => {
                                    event(&w, &sq, "terminated", json!({}));
                                }
                            }
                        }
                    });
                }
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "configurationDone" => respond(&writer, &seq, req_seq, &command, json!({})),
            "setBreakpoints" => {
                // Phase 2a: acknowledge but report unverified — editor-set
                // breakpoints need source-map mapping (2b). `debugger;` stops
                // work regardless.
                let lines = msg["arguments"]["breakpoints"].as_array().cloned().unwrap_or_default();
                let bps: Vec<Value> = lines.iter().map(|b| json!({"verified": false, "line": b["line"]})).collect();
                respond(&writer, &seq, req_seq, &command, json!({ "breakpoints": bps }));
            }
            "setExceptionBreakpoints" => respond(&writer, &seq, req_seq, &command, json!({})),
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
            // Scopes/variables arrive in 2b; empty keeps the editor happy.
            "scopes" => respond(&writer, &seq, req_seq, &command, json!({"scopes": []})),
            "variables" => respond(&writer, &seq, req_seq, &command, json!({"variables": []})),
            "continue" => {
                step(&senders, &target, ConnCmd::Continue);
                respond(&writer, &seq, req_seq, &command, json!({"allThreadsContinued": true}));
            }
            "next" => {
                step(&senders, &target, ConnCmd::StepOver);
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "stepIn" => {
                step(&senders, &target, ConnCmd::StepInto);
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "stepOut" => {
                step(&senders, &target, ConnCmd::StepOut);
                respond(&writer, &seq, req_seq, &command, json!({}));
            }
            "pause" => {
                step(&senders, &target, ConnCmd::Pause);
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

fn step(senders: &Senders, target: &Option<String>, cmd: ConnCmd) {
    if let Some(k) = target {
        send_cmd(senders, k, cmd);
    }
}

fn send_cmd(senders: &Senders, key: &str, cmd: ConnCmd) {
    if let Some(s) = senders.lock().unwrap().get(key) {
        let _ = s.send(cmd);
    }
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

fn event(writer: &Writer, seq: &Seq, event: &str, body: Value) {
    write_msg(writer, seq, json!({
        "type": "event",
        "event": event,
        "body": body,
    }));
}
