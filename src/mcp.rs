//! `metroctl mcp`: a stdio MCP server for coding agents. It talks to the
//! dashboard running in the agent's checkout through its control socket (found
//! via `.metroctl/session.json`), and formats replies as compact text so they
//! don't flood the agent's context.
//!
//! Register it in a project's `.mcp.json`:
//! `{"mcpServers": {"metroctl": {"command": "metroctl", "args": ["mcp"]}}}`

use crate::control;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

const PROTOCOL: &str = "2025-06-18";

pub fn run() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // a notification (e.g. notifications/initialized)
        };
        let method = msg["method"].as_str().unwrap_or("");
        let reply = match method {
            "initialize" => json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "metroctl", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Inspect and drive the React Native app running in this checkout's metroctl session: \
                    JS console logs, errors, network requests, reload and rebuild. \
                    To see or use the app's screen (screenshots, taps, typing) use the `touchctl` command line. \
                    After a JS change Fast Refresh applies it; use reload if state looks stale and rebuild after native changes \
                    (Podfile, ios/, native modules). Use `since` with the `next` value from a previous call to see only new entries.",
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
                match call_tool(name, &args) {
                    Ok(content) => json!({ "content": content }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true }),
                }
            }
            _ => {
                let err = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unknown method {method}") } });
                let _ = writeln!(out, "{err}");
                let _ = out.flush();
                continue;
            }
        };
        let _ = writeln!(out, "{}", json!({ "jsonrpc": "2.0", "id": id, "result": reply }));
        let _ = out.flush();
    }
}

fn tools() -> Value {
    let obj = |props: Value| json!({ "type": "object", "properties": props });
    let since = json!({ "type": "integer", "description": "Only entries with seq >= this (pass `next` from a previous call)" });
    let limit = json!({ "type": "integer", "description": "Max entries, newest kept (default 50)" });
    json!([
        { "name": "wait_ready", "description": "Block until the app is built, running and connected to Metro (or the build failed), instead of ending your turn to wait. Use it at the start and after rebuild/restart_metro.",
          "inputSchema": obj(json!({ "timeout_s": { "type": "integer", "description": "Default 900" } })) },
        { "name": "status", "description": "Session overview: Metro port and state, session status (building/running/build_failed…), connected apps, process tabs.", "inputSchema": obj(json!({})) },
        { "name": "logs", "description": "JS console logs from the app (newest last), with stacks for errors.",
          "inputSchema": obj(json!({
              "level": { "type": "string", "enum": ["all", "warn", "error"], "description": "warn = warnings and errors" },
              "filter": { "type": "string", "description": "Case-insensitive text match" },
              "since": since, "limit": limit })) },
        { "name": "errors", "description": "Everything that went wrong: JS errors, failed/4xx/5xx requests, failed processes (build, install) with their last output lines.",
          "inputSchema": obj(json!({ "since": since })) },
        { "name": "network", "description": "The app's network requests (REST, GraphQL operation names, WebSockets).",
          "inputSchema": obj(json!({
              "failed_only": { "type": "boolean" },
              "filter": { "type": "string", "description": "Match on URL or GraphQL operation" },
              "since": since, "limit": limit })) },
        { "name": "request", "description": "One network request in full: headers and bodies.",
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "description": "id from `network`" } }, "required": ["id"] } },
        { "name": "output", "description": "Last lines of a process tab's terminal output (Metro, iOS build, Install…).",
          "inputSchema": obj(json!({
              "process": { "type": "string", "description": "Label match, e.g. iOS or Metro (default: latest process)" },
              "lines": { "type": "integer", "description": "Default 80" } })) },
        { "name": "reload", "description": "Reload the app's JS bundle.", "inputSchema": obj(json!({})) },
        { "name": "rebuild", "description": "Rebuild and reinstall the native app on the session's simulator (needed after native changes). Waits for the result.",
          "inputSchema": obj(json!({ "wait": { "type": "boolean", "description": "Wait for the build to finish (default true)" } })) },
        { "name": "deeplinks", "description": "The app's deep links from the project config. Fill <placeholders> (e.g. a uuid or token from the task) and open one with `touchctl open <url>`.", "inputSchema": obj(json!({})) },
        { "name": "restart_metro", "description": "Restart Metro (e.g. after changing metro.config.js or installing JS deps).", "inputSchema": obj(json!({})) },
    ])
}

fn text(s: impl Into<String>) -> Vec<Value> {
    vec![json!({ "type": "text", "text": s.into() })]
}

fn session() -> Result<(std::path::PathBuf, crate::session::SessionFile)> {
    control::find_session(&std::env::current_dir()?)
}

fn ask(cmd: &str, args: Value) -> Result<Value> {
    let (sock, _) = session()?;
    let v = control::call(&sock, cmd, args)?;
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        bail!("{e}");
    }
    Ok(v)
}

fn call_tool(name: &str, a: &Value) -> Result<Vec<Value>> {
    let mut args = a.clone();
    if args["level"] == "all" {
        args["level"] = Value::Null;
    }
    match name {
        "status" => Ok(text(fmt_status(&ask("status", json!({}))?))),
        "wait_ready" => wait_ready(a["timeout_s"].as_u64().unwrap_or(900)),
        "logs" => Ok(text(fmt_logs(&ask("logs", args)?))),
        "network" => Ok(text(fmt_net(&ask("network", args)?))),
        "request" => Ok(text(ask("request", args)?["text"].as_str().unwrap_or("").to_string())),
        "errors" => Ok(text(fmt_errors(&ask("errors", args)?))),
        "output" => {
            let v = ask("output", args)?;
            let state = match v["exit_code"].as_u64() {
                None => "running".to_string(),
                Some(c) => format!("exited {c}"),
            };
            Ok(text(format!("{} ({state})\n{}", v["label"].as_str().unwrap_or(""), lines(&v["lines"]))))
        }
        "reload" => {
            let v = ask("reload", json!({}))?;
            Ok(text(v["message"].as_str().unwrap_or("reloaded").to_string()))
        }
        "deeplinks" => {
            let v = ask("deeplinks", json!({}))?;
            let links: Vec<String> = v["links"].as_array().into_iter().flatten().map(|l| format!("{}: {}", l["name"].as_str().unwrap_or(""), l["url"].as_str().unwrap_or(""))).collect();
            Ok(text(if links.is_empty() { "(no deep links configured)".into() } else { links.join("\n") }))
        }
        "restart_metro" => {
            ask("restart_metro", json!({}))?;
            Ok(text("Metro restarted"))
        }
        "rebuild" => rebuild(a["wait"].as_bool().unwrap_or(true)),
        _ => bail!("unknown tool {name}"),
    }
}

fn rebuild(wait: bool) -> Result<Vec<Value>> {
    let v = ask("rebuild", json!({}))?;
    let label = v["process"].as_str().unwrap_or("iOS").to_string();
    if !wait {
        return Ok(text(format!("build started ({label} tab); check `status`")));
    }
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let s = ask("status", json!({}))?;
        match s["status"].as_str().unwrap_or("") {
            "building" if start.elapsed() < Duration::from_secs(30 * 60) => continue,
            "running" | "ready" => return Ok(text(format!("build succeeded in {}s; the app was installed and launched", start.elapsed().as_secs()))),
            other => {
                let out = ask("output", json!({ "process": label, "lines": 60 }))?;
                return Ok(text(format!("build {other} after {}s. Last output:\n{}", start.elapsed().as_secs(), lines(&out["lines"]))));
            }
        }
    }
}

fn wait_ready(timeout_s: u64) -> Result<Vec<Value>> {
    let start = Instant::now();
    loop {
        let s = ask("status", json!({}))?;
        let status = s["status"].as_str().unwrap_or("");
        let name = s["device_name"].as_str();
        let connected = s["apps"].as_array().into_iter().flatten().any(|a| a["status"] == "connected" && name.is_none_or(|n| a["device"].as_str().unwrap_or("").contains(n)));
        if status.ends_with("failed") {
            let errors = ask("errors", json!({}))?;
            return Ok(text(format!("not ready: {status} after {}s\n{}", start.elapsed().as_secs(), fmt_errors(&errors))));
        }
        if matches!(status, "running" | "ready") && connected {
            return Ok(text(format!("ready after {}s\n{}", start.elapsed().as_secs(), fmt_status(&s))));
        }
        if start.elapsed() > Duration::from_secs(timeout_s) {
            return Ok(text(format!("still not ready after {timeout_s}s\n{}", fmt_status(&s))));
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn lines(v: &Value) -> String {
    v.as_array().into_iter().flatten().filter_map(|l| l.as_str()).collect::<Vec<_>>().join("\n")
}

fn fmt_status(v: &Value) -> String {
    let mut s = format!(
        "{} — session {} · Metro {} on :{}\n",
        v["project"].as_str().unwrap_or(""),
        v["status"].as_str().unwrap_or("?"),
        v["metro"].as_str().unwrap_or("?"),
        v["port"]
    );
    if let Some(d) = v["device"].as_str() {
        s.push_str(&format!("simulator {d}\n"));
    }
    let apps = v["apps"].as_array().cloned().unwrap_or_default();
    if apps.is_empty() {
        s.push_str("no app connected to Metro\n");
    }
    for a in apps {
        s.push_str(&format!("app on {}: {} · {} logs · {} requests", a["device"].as_str().unwrap_or("?"), a["status"].as_str().unwrap_or("?"), a["logs"], a["requests"]));
        if let Some(p) = a["paused"].as_str() {
            s.push_str(&format!(" · PAUSED at {p}"));
        }
        s.push('\n');
    }
    for p in v["processes"].as_array().into_iter().flatten() {
        let state = match p["exit_code"].as_u64() {
            None => "running".to_string(),
            Some(c) => format!("exited {c}"),
        };
        s.push_str(&format!("process {}: {state}\n", p["label"].as_str().unwrap_or("")));
    }
    s
}

fn fmt_logs(v: &Value) -> String {
    let mut s = String::new();
    for e in v["entries"].as_array().into_iter().flatten() {
        s.push_str(&format!("#{} {} {}\n", e["seq"], e["level"].as_str().unwrap_or(""), e["text"].as_str().unwrap_or("")));
        for d in e["details"].as_array().into_iter().flatten() {
            s.push_str(&format!("  {}\n", d.as_str().unwrap_or("")));
        }
        for f in e["stack"].as_array().into_iter().flatten() {
            s.push_str(&format!("    {}\n", f.as_str().unwrap_or("").trim()));
        }
    }
    if s.is_empty() {
        s.push_str("(no matching logs)\n");
    }
    let shown = v["entries"].as_array().map_or(0, |a| a.len());
    s.push_str(&format!("[{shown} of {} matching · next: {}]", v["matched"], v["next"]));
    s
}

fn fmt_net(v: &Value) -> String {
    let mut s = String::new();
    for r in v["requests"].as_array().into_iter().flatten() {
        let status = if r["failed"].is_string() { format!("FAIL({})", r["failed"].as_str().unwrap_or("")) } else { r["status"].as_i64().map_or("…".into(), |c| c.to_string()) };
        s.push_str(&format!("#{} id={} {status} {} {}", r["seq"], r["id"].as_str().unwrap_or(""), r["method"].as_str().unwrap_or(""), r["url"].as_str().unwrap_or("")));
        if let Some(ms) = r["ms"].as_i64() {
            s.push_str(&format!(" {ms}ms"));
        }
        if let Some(op) = r["graphql"].as_str() {
            s.push_str(&format!(" [{op}]"));
        }
        s.push('\n');
    }
    if s.is_empty() {
        s.push_str("(no matching requests)\n");
    }
    let shown = v["requests"].as_array().map_or(0, |a| a.len());
    s.push_str(&format!("[{shown} of {} matching · next: {}]", v["matched"], v["next"]));
    s
}

fn fmt_errors(v: &Value) -> String {
    let mut s = format!("session status: {}\n", v["status"].as_str().unwrap_or("?"));
    let logs = &v["logs"];
    if logs.get("error").is_none() {
        s.push_str("\nJS errors:\n");
        s.push_str(&fmt_logs(logs));
        s.push_str("\n\nFailed requests:\n");
        s.push_str(&fmt_net(&v["requests"]));
        s.push('\n');
    } else {
        s.push_str("no app connected to Metro\n");
    }
    for p in v["failed_processes"].as_array().into_iter().flatten() {
        s.push_str(&format!("\n{} exited {}:\n{}\n", p["label"].as_str().unwrap_or(""), p["exit_code"], lines(&p["tail"])));
    }
    s
}
