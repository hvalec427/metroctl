//! React Native over Metro's CDP inspector: target discovery, the `--print-ws`
//! helper, and the plain (non-TTY) log stream. Mirrors `utils/rnlogs.ts` and the
//! inspector bits of `utils/rnclient.ts`. The interactive TUI lives in `rntui`.

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct RawTarget {
    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "deviceName", default)]
    pub device_name: Option<String>,
    /// JS engine reported by the inspector (e.g. "Hermes").
    #[serde(default)]
    pub vm: Option<String>,
}

impl RawTarget {
    pub fn key(&self) -> String {
        self.device_name.clone().or_else(|| self.title.clone()).unwrap_or_else(|| "app".into())
    }
    pub fn label(&self) -> String {
        let title = self.title.clone().or_else(|| self.description.clone()).unwrap_or_else(|| "app".into());
        match &self.device_name {
            Some(d) => format!("{title}  {d}"),
            None => title,
        }
    }
}

pub fn fetch_targets(port: u16) -> Result<Vec<RawTarget>> {
    let url = format!("http://localhost:{port}/json");
    let body = reqwest::blocking::get(&url)?.text()?;
    let targets: Vec<RawTarget> = serde_json::from_str(&body)?;
    Ok(targets)
}

/// URLs on stdout (pipeable), labels on stderr — for pointing an external
/// debugger (nvim-dap) at the right endpoint.
pub fn print_inspector_ws(port: u16, name: Option<&str>) {
    let all = match fetch_targets(port) {
        Ok(t) => t,
        Err(_) => {
            eprintln!("Couldn't reach Metro on :{port}. Is the bundler running?");
            std::process::exit(1);
        }
    };
    let list: Vec<&RawTarget> = all
        .iter()
        .filter(|t| t.web_socket_debugger_url.is_some())
        .filter(|t| name.map(|n| t.key().to_lowercase().contains(&n.to_lowercase())).unwrap_or(true))
        .collect();
    if list.is_empty() {
        eprintln!("No Metro inspector targets on :{port}{}. Is the app running?", name.map(|n| format!(" matching \"{n}\"")).unwrap_or_default());
        std::process::exit(1);
    }
    let many = list.len() > 1;
    for t in list {
        if many {
            eprintln!("# {}", t.label());
        }
        println!("{}", t.web_socket_debugger_url.as_ref().unwrap());
    }
}

fn color(level: &str, s: &str) -> String {
    let code = match level {
        "error" | "assert" => "31",
        "warning" | "warn" => "33",
        "info" => "36",
        "debug" | "verbose" => "90",
        _ => return s.to_string(),
    };
    format!("\x1b[{code}m{s}\x1b[0m")
}

fn render_arg(a: &Value) -> String {
    if let Some(v) = a.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(d) = a.get("description").and_then(|d| d.as_str()) {
        return d.to_string();
    }
    if let Some(u) = a.get("unserializableValue").and_then(|d| d.as_str()) {
        return u.to_string();
    }
    a.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string()
}

fn handle_cdp(msg: &Value) {
    match msg.get("method").and_then(|m| m.as_str()) {
        Some("Runtime.consoleAPICalled") => {
            let p = &msg["params"];
            let level = p.get("type").and_then(|t| t.as_str()).unwrap_or("log");
            let args = p.get("args").and_then(|a| a.as_array()).cloned().unwrap_or_default();
            let text: Vec<String> = args.iter().map(render_arg).collect();
            println!("{}", color(level, &text.join(" ")));
        }
        Some("Log.entryAdded") => {
            let e = &msg["params"]["entry"];
            let level = e.get("level").and_then(|l| l.as_str()).unwrap_or("log");
            println!("{}", color(level, e.get("text").and_then(|t| t.as_str()).unwrap_or("")));
        }
        Some("Runtime.exceptionThrown") => {
            let d = &msg["params"]["exceptionDetails"];
            let text = d["exception"]["description"].as_str().or_else(|| d["text"].as_str()).unwrap_or("Uncaught exception");
            println!("{}", color("error", text));
        }
        Some("Network.requestWillBeSent") => {
            let r = &msg["params"]["request"];
            println!("{}", color("debug", &format!("→ {} {}", r["method"].as_str().unwrap_or("GET"), r["url"].as_str().unwrap_or(""))));
        }
        Some("Network.responseReceived") => {
            let r = &msg["params"]["response"];
            let status = r["status"].as_i64().unwrap_or(0);
            let line = format!("← {} {}", status, r["url"].as_str().unwrap_or(""));
            println!("{}", color(if status >= 400 { "error" } else { "debug" }, &line));
        }
        _ => {}
    }
}

/// Plain (non-TTY) stream: connect to one target and print console + network.
pub fn stream_plain(port: u16, name: Option<&str>) -> Result<()> {
    let all = fetch_targets(port).map_err(|_| {
        anyhow::anyhow!("Could not reach Metro at http://localhost:{port} — is the dev server running?")
    })?;
    let mut debuggable: Vec<RawTarget> = all.into_iter().filter(|t| t.web_socket_debugger_url.is_some()).collect();
    if let Some(n) = name {
        let n = n.to_lowercase();
        let matched: Vec<RawTarget> = debuggable.iter().filter(|t| t.key().to_lowercase().contains(&n)).cloned().collect();
        if !matched.is_empty() {
            debuggable = matched;
        }
    }
    let target = debuggable
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No debuggable React Native target found — is the app running in dev mode?"))?;

    let url = target.web_socket_debugger_url.clone().unwrap();
    let mut socket = connect_cdp(&url, port)?;
    for method in ["Runtime.enable", "Log.enable", "Network.enable"] {
        let _ = socket.send(tungstenite::Message::Text(format!("{{\"id\":1,\"method\":\"{method}\"}}")));
    }
    eprintln!("Connected to Metro ({}) — streaming console + network  (Ctrl+C to stop)\n", target.key());
    loop {
        match socket.read() {
            Ok(tungstenite::Message::Text(t)) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    handle_cdp(&v);
                }
            }
            Ok(tungstenite::Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    Ok(())
}

/// Connect to a Metro CDP target, setting the localhost Origin header the
/// inspector proxy requires.
pub fn connect_cdp(url: &str, port: u16) -> Result<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>> {
    use tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request()?;
    req.headers_mut().insert(
        tungstenite::http::header::ORIGIN,
        format!("http://localhost:{port}").parse().map_err(|_| anyhow::anyhow!("bad origin"))?,
    );
    let (socket, _resp) = tungstenite::connect(req)?;
    Ok(socket)
}
