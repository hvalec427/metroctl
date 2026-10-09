//! The dashboard's control socket: a Unix socket (path in
//! `.metroctl/session.json`) speaking JSON lines, so other tools — `metroctl
//! mcp` for agents, `metroctl ctl` by hand — can read the session's logs,
//! network and process output and drive it (reload, rebuild, restart Metro).
//!
//! Request: `{"cmd": "logs", "args": {"level": "error"}}`, one per line.
//! Reply: one JSON value per line. Requests are answered by the dashboard's
//! main loop, which owns all the state.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

pub struct Request {
    pub cmd: String,
    pub args: Value,
    pub reply: Sender<Value>,
}

/// Per-process socket path (kept short: Unix socket paths max out ~104 bytes).
pub fn socket_path() -> PathBuf {
    std::env::temp_dir().join(format!("metroctl-{}.sock", std::process::id()))
}

/// Listen on `path`, forwarding each request to `tx`.
pub fn serve(path: &Path, tx: Sender<Request>) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || handle(stream, tx));
        }
    });
    Ok(())
}

fn handle(stream: UnixStream, tx: Sender<Request>) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            return;
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => {
                let (rtx, rrx) = channel();
                let cmd = req["cmd"].as_str().unwrap_or("").to_string();
                let args = req.get("args").cloned().unwrap_or(json!({}));
                if tx.send(Request { cmd, args, reply: rtx }).is_err() {
                    return; // dashboard gone
                }
                rrx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| json!({ "error": "dashboard didn't answer" }))
            }
            Err(e) => json!({ "error": format!("bad request: {e}") }),
        };
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            return;
        }
    }
}

/// Send one request to the dashboard at `path` and wait for its reply.
pub fn call(path: &Path, cmd: &str, args: Value) -> Result<Value> {
    let mut s = UnixStream::connect(path).with_context(|| format!("connecting to metroctl at {}", path.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(40)))?;
    writeln!(s, "{}", json!({ "cmd": cmd, "args": args }))?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    if line.is_empty() {
        return Err(anyhow!("metroctl closed the connection"));
    }
    Ok(serde_json::from_str(&line)?)
}

/// The control socket of the session running in `dir` or a parent of it.
pub fn find_session(dir: &Path) -> Result<(PathBuf, crate::session::SessionFile)> {
    for d in dir.ancestors() {
        if let Some(s) = crate::session::read_session(d) {
            if !crate::session::pid_alive(s.pid) {
                return Err(anyhow!("the metroctl session in {} isn't running (stale session.json)", d.display()));
            }
            let sock = s.socket.clone().ok_or_else(|| anyhow!("the metroctl session in {} has no control socket", d.display()))?;
            return Ok((PathBuf::from(sock), s));
        }
    }
    Err(anyhow!("no running metroctl session here — start one with `metroctl` or `metroctl up`"))
}

/// `metroctl ctl <cmd> [json args]`: one request to this checkout's session.
pub fn ctl(cmd: &str, args: Option<&str>) -> Result<()> {
    let args: Value = match args {
        Some(a) => serde_json::from_str(a).context("args must be a JSON object")?,
        None => json!({}),
    };
    let (sock, _) = find_session(&std::env::current_dir()?)?;
    let reply = call(&sock, cmd, args)?;
    println!("{}", serde_json::to_string_pretty(&reply)?);
    Ok(())
}
