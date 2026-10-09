//! Watch a Metro that metroctl didn't start. Its real terminal output lives in
//! someone else's PTY, so instead we subscribe to the dev server's `/events`
//! websocket (from @react-native-community/cli-server-api), which broadcasts
//! every reporter event, and render a rough log of it: bundling progress, build
//! results, bundling errors, server/client logs. The same socket also forwards
//! commands (`reload`, `devMenu`) to connected apps.
//!
//! The watcher runs as the hidden `metroctl metro-events` subcommand inside a
//! dashboard PTY tab, so it gets scrollback/colors like any other process.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::process::Command;
use std::time::{Duration, Instant};

const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";
const CLEAR_LINE: &str = "\r\x1b[2K";

/// Is anything listening on `port` on localhost?
pub fn port_in_use(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200)).is_ok()
}

/// PID and working directory of the process listening on `port` (via lsof).
fn listener_info(port: u16) -> Option<(String, Option<String>)> {
    let out = Command::new("lsof").args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"]).output().ok()?;
    let pid = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    if pid.is_empty() {
        return None;
    }
    let out = Command::new("lsof").args(["-a", "-p", &pid, "-d", "cwd", "-Fn"]).output().ok();
    let cwd = out.and_then(|o| String::from_utf8_lossy(&o.stdout).lines().find_map(|l| l.strip_prefix('n').map(String::from)));
    Some((pid, cwd))
}

/// Short `pid 1234 · ~/dev/app` description of whatever holds the port.
fn describe_listener(port: u16) -> String {
    match listener_info(port) {
        Some((pid, cwd)) => {
            let home = std::env::var("HOME").unwrap_or_default();
            let cwd = cwd.map(|c| if !home.is_empty() && c.starts_with(&home) { c.replacen(&home, "~", 1) } else { c });
            match cwd {
                Some(c) => format!("pid {pid} · {c}"),
                None => format!("pid {pid}"),
            }
        }
        None => "unknown process".into(),
    }
}

fn connect(port: u16) -> anyhow::Result<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>> {
    crate::rn::connect_cdp(&format!("ws://localhost:{port}/events"), port)
}

/// Send a command (`reload`, `devMenu`) to the apps connected to Metro.
pub fn send_command(port: u16, command: &str) -> bool {
    let Ok(mut ws) = connect(port) else {
        return false;
    };
    let msg = json!({ "version": 2, "type": "command", "command": command });
    let ok = ws.send(tungstenite::Message::Text(msg.to_string())).is_ok();
    let _ = ws.close(None);
    let _ = ws.flush();
    ok
}

/// `metroctl metro-events --port N`: print Metro's events until killed,
/// reconnecting if Metro goes away and comes back.
pub fn watch(port: u16) {
    println!("{CYAN}External Metro on :{port}{RESET} {DIM}({}){RESET}", describe_listener(port));
    println!("{DIM}Started outside metroctl, so this is a rebuilt view of its bundler events, not its terminal.{RESET}");
    println!("{DIM}R/D still reload / open the dev menu. Press m to take over (kill it and run Metro here).{RESET}\n");
    let mut r = Renderer::default();
    loop {
        match connect(port) {
            Ok(mut ws) => {
                println!("{GREEN}● connected to Metro events{RESET}");
                loop {
                    match ws.read() {
                        Ok(tungstenite::Message::Text(t)) => {
                            if let Ok(v) = serde_json::from_str::<Value>(&t) {
                                r.event(&v);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                r.end_progress();
                println!("{YELLOW}○ Metro on :{port} went away — waiting for it to come back…{RESET}");
            }
            Err(e) => {
                if port_in_use(port) {
                    // Something holds the port but has no events socket (not a
                    // community-CLI Metro, or a different server entirely).
                    println!("{RED}Couldn't subscribe to Metro events on :{port}: {e}{RESET}");
                    println!("{DIM}Press m to take over and run Metro inside metroctl.{RESET}");
                    while port_in_use(port) {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        }
        while !port_in_use(port) {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[derive(Default)]
struct Renderer {
    builds: HashMap<String, (String, Instant)>, // buildID → (label, start)
    progress: Option<String>,                   // buildID of the in-place progress line
}

impl Renderer {
    fn out(&self, s: &str) {
        let mut o = std::io::stdout();
        let _ = o.write_all(s.as_bytes());
        let _ = o.flush();
    }

    /// Leave the in-place progress line as-is and move below it.
    fn end_progress(&mut self) {
        if self.progress.take().is_some() {
            self.out("\n");
        }
    }

    fn line(&mut self, s: &str) {
        self.end_progress();
        self.out(&format!("{s}\n"));
    }

    fn event(&mut self, v: &Value) {
        let ty = v["type"].as_str().unwrap_or("");
        let id = v["buildID"].as_str().unwrap_or("").to_string();
        match ty {
            "initialize_started" => self.line(&format!("{DIM}Starting Metro…{RESET}")),
            "initialize_done" => self.line(&format!("{GREEN}Metro ready{RESET}")),
            "initialize_failed" => self.line(&format!("{RED}Metro failed to start{RESET}\n{}", text(&v["error"]))),
            "bundle_build_started" => {
                let d = &v["bundleDetails"];
                let platform = d["platform"].as_str().unwrap_or("?");
                let entry = d["entryFile"].as_str().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
                let label = format!("{platform} {entry}");
                self.line(&format!("{CYAN}▸ bundling {label}{RESET}"));
                self.builds.insert(id, (label, Instant::now()));
            }
            "bundle_transform_progressed" => {
                let done = v["transformedFileCount"].as_u64().unwrap_or(0);
                let total = v["totalFileCount"].as_u64().unwrap_or(0).max(1);
                let pct = (done * 100 / total).min(100);
                let filled = (pct / 5) as usize;
                let label = self.builds.get(&id).map(|b| b.0.clone()).unwrap_or_default();
                if self.progress.as_deref() != Some(id.as_str()) {
                    self.end_progress();
                }
                self.out(&format!("{CLEAR_LINE}  {label} {}{} {pct:>3}% ({done}/{total})", "▓".repeat(filled), "░".repeat(20 - filled)));
                self.progress = Some(id);
            }
            "bundle_build_done" | "bundle_build_failed" => {
                let (label, start) = self.builds.remove(&id).unwrap_or_else(|| (String::new(), Instant::now()));
                let secs = start.elapsed().as_secs_f64();
                let msg = if ty == "bundle_build_done" {
                    format!("{GREEN}✓ bundled {label} in {secs:.1}s{RESET}")
                } else {
                    format!("{RED}✗ bundling {label} failed after {secs:.1}s{RESET}")
                };
                if self.progress.as_deref() == Some(id.as_str()) {
                    // Replace the progress bar with the result.
                    self.out(&format!("{CLEAR_LINE}{msg}\n"));
                    self.progress = None;
                } else {
                    self.line(&msg);
                }
            }
            "bundling_error" => self.line(&format!("{RED}error:{RESET} {}", text(&v["error"]))),
            "hmr_client_error" => self.line(&format!("{RED}HMR error:{RESET} {}", text(&v["error"]))),
            "transform_cache_reset" => self.line(&format!("{DIM}transform cache reset{RESET}")),
            "dep_graph_loading" => self.line(&format!("{DIM}loading dependency graph…{RESET}")),
            "dep_graph_loaded" => self.line(&format!("{DIM}dependency graph loaded{RESET}")),
            "global_cache_error" => self.line(&format!("{YELLOW}cache error:{RESET} {}", text(&v["error"]))),
            "worker_stdout_chunk" | "worker_stderr_chunk" => {
                let chunk = v["chunk"].as_str().unwrap_or("").trim_end();
                if !chunk.is_empty() {
                    self.line(chunk);
                }
            }
            "client_log" | "unstable_server_log" => {
                let level = v["level"].as_str().unwrap_or("log");
                let color = match level {
                    "error" => RED,
                    "warn" => YELLOW,
                    _ => DIM,
                };
                let tag = if ty == "client_log" { "app" } else { "metro" };
                self.line(&format!("{color}{tag} {level:>5}{RESET} {}", text(&v["data"])));
            }
            // Noisy/internal events we don't need to show.
            "bundle_save_log" | "watcher_health_check_result" | "watcher_status" | "server_listening" => {}
            "" => {}
            other => self.line(&format!("{DIM}[{other}]{RESET}")),
        }
    }
}

/// Render an event payload field: strings as-is, arrays space-joined, Error-ish
/// objects by their message/stack, anything else as compact JSON.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(text).collect::<Vec<_>>().join(" "),
        Value::Object(o) => {
            if let Some(m) = o.get("message").and_then(|m| m.as_str()) {
                let mut s = m.to_string();
                if let Some(snippet) = o.get("snippet").and_then(|s| s.as_str()) {
                    s.push('\n');
                    s.push_str(snippet);
                }
                s
            } else {
                v.to_string()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
