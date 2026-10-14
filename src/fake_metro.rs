//! A stand-in Metro for tests: one local port serving `/json` (the inspector
//! targets), `/symbolicate`, and CDP websockets. Each CDP request the client
//! sends is recorded and answered by a responder, which can slip events in
//! ahead of the reply the way Hermes does; tests can also push events at any
//! time.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

/// Frames to send back for one CDP request (events and/or the reply).
type Responder = dyn Fn(&Value) -> Vec<Value> + Send + Sync;

#[derive(Default)]
struct State {
    requests: Vec<Value>,
    targets: Vec<Value>,
    symbolicate: Option<Value>,
    conns: Vec<Sender<Value>>,
}

pub struct FakeMetro {
    pub port: u16,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

/// The plain reply `{"id", "result": {}}` most requests get.
pub fn reply(req: &Value, result: Value) -> Value {
    json!({"id": req["id"], "result": result})
}

impl FakeMetro {
    /// Answers every request with an empty result.
    pub fn start() -> FakeMetro {
        FakeMetro::with(|req| vec![reply(req, json!({}))])
    }

    pub fn with(responder: impl Fn(&Value) -> Vec<Value> + Send + Sync + 'static) -> FakeMetro {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let responder: Arc<Responder> = Arc::new(responder);
        let (st, stp) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if stp.load(Ordering::Relaxed) {
                    break;
                }
                let (st, stp, r) = (st.clone(), stp.clone(), responder.clone());
                std::thread::spawn(move || serve(stream, &st, &stp, &*r));
            }
        });
        FakeMetro { port, state, stop }
    }

    pub fn ws_url(&self, page: &str) -> String {
        let page = page.replace(' ', "%20");
        format!("ws://127.0.0.1:{}/inspector/debug?page={page}", self.port)
    }

    /// Advertise a target on `/json`, named by its device.
    pub fn add_target(&self, device: &str) {
        let url = self.ws_url(device);
        self.state.lock().unwrap().targets.push(json!({
            "title": "App",
            "deviceName": device,
            "webSocketDebuggerUrl": url,
        }));
    }

    /// The `stack` `/symbolicate` answers with; unset, it fails.
    pub fn set_symbolicated(&self, stack: Value) {
        self.state.lock().unwrap().symbolicate = Some(stack);
    }

    /// Push a frame (usually an event) to every connected client.
    pub fn send(&self, frame: Value) {
        for c in &self.state.lock().unwrap().conns {
            let _ = c.send(frame.clone());
        }
    }

    /// Requests received so far with this CDP method.
    pub fn requests(&self, method: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r["method"] == method)
            .cloned()
            .collect()
    }

    /// Wait until a client has connected over CDP.
    pub fn wait_connected(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.state.lock().unwrap().conns.is_empty() {
            assert!(Instant::now() < deadline, "no CDP connection");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Close every CDP connection, as Metro does when the app goes away.
    pub fn close_all(&self) {
        self.state.lock().unwrap().conns.clear();
    }
}

impl Drop for FakeMetro {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.close_all();
        // Wake the accept loop so it sees `stop`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<State>, stop: &AtomicBool, responder: &Responder) {
    let Some(head) = read_request(&mut stream) else {
        return;
    };
    let lower = head.to_ascii_lowercase();
    if lower.contains("upgrade: websocket") {
        let key = head
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.trim().eq_ignore_ascii_case("sec-websocket-key"))
            })
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default();
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        let _ = write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        state.lock().unwrap().conns.push(tx);
        let _ = stream.set_read_timeout(Some(Duration::from_millis(10)));
        cdp(
            WebSocket::from_raw_socket(stream, Role::Server, None),
            rx,
            state,
            stop,
            responder,
        );
        return;
    }
    let path = head.split_whitespace().nth(1).unwrap_or("");
    let found = {
        let s = state.lock().unwrap();
        match path {
            "/json" | "/json/list" => Some(Value::Array(s.targets.clone())),
            "/symbolicate" => s.symbolicate.clone().map(|stack| json!({ "stack": stack })),
            _ => None,
        }
    };
    let (status, body) = match found {
        Some(v) => ("200 OK", v.to_string()),
        None => ("404 Not Found", String::new()),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// One CDP connection: answer requests, forward pushed frames, until the test
/// closes it (its sender is dropped) or the fake stops.
fn cdp(
    mut ws: WebSocket<TcpStream>,
    pushed: Receiver<Value>,
    state: &Mutex<State>,
    stop: &AtomicBool,
    responder: &Responder,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                let Ok(req) = serde_json::from_str::<Value>(&t) else {
                    continue;
                };
                state.lock().unwrap().requests.push(req.clone());
                for frame in responder(&req) {
                    let _ = ws.send(Message::Text(frame.to_string()));
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        loop {
            match pushed.try_recv() {
                Ok(frame) => {
                    let _ = ws.send(Message::Text(frame.to_string()));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return;
                }
            }
        }
    }
}

/// The request head (through the blank line); its `Content-Length` body is
/// read and dropped.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let len = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        })
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some(head)
}
