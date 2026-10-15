//! `signalbox up --detach`: the dashboard needs a terminal, which coding agents
//! don't have. Run `up` in a window of a tmux server of signalbox's own
//! (`tmux -L signalbox`, apart from yours) instead, wait until its session is
//! up and print what it got (port, device, window).

use crate::session::{self, SessionFile};
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

/// The tmux server (`-L`) and its one session.
const TMUX: &str = "signalbox";
/// Long enough for a first `--new-sim`, which builds the simulator template.
const TIMEOUT: Duration = Duration::from_secs(20 * 60);
const FAILED: &str = "[signalbox up failed";

/// `args` are `up`'s own (without `--detach`); `root` is the checkout.
pub fn run(args: &[String], root: &Path) -> Result<()> {
    if let Some(s) = session::read_session(root).filter(|s| session::pid_alive(s.pid)) {
        println!("already running: {}", describe(&s));
        return Ok(());
    }
    let ok = tmux().arg("-V").output().is_ok_and(|o| o.status.success());
    if !ok {
        bail!("--detach runs the dashboard in tmux, which isn't installed (brew install tmux)");
    }
    let exe = std::env::current_exe()?.display().to_string();
    let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("app").to_string();
    // The window closes when signalbox stops (or is stopped); on an error it
    // stays, so the output can be read.
    let cmd = format!(
        "{} up {}; s=$?; [ $s -eq 0 ] || [ $s -ge 128 ] || {{ echo; echo '{FAILED} ('$s')]'; read _; }}",
        quote(&exe),
        args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    );
    let started = SystemTime::now();
    // A window in the session, or the session itself if there's none yet (two
    // `up`s may race to create it: the loser adds a window).
    let mut out = open(&name, root, &cmd, false)?;
    if !out.status.success() {
        out = open(&name, root, &cmd, true)?;
    }
    if !out.status.success() {
        bail!("tmux: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
    eprintln!("starting in tmux window {name} (tmux -L {TMUX} attach to watch)…");
    let t = Instant::now();
    loop {
        if let Some(s) = fresh_session(root, started) {
            println!("{} · tmux -L {TMUX} window {name}", describe(&s));
            return Ok(());
        }
        let screen = tmux().args(["capture-pane", "-p", "-t", &pane, "-S", "-60"]).output();
        match screen {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                if text.contains(FAILED) {
                    let _ = tmux().args(["kill-pane", "-t", &pane]).output();
                    bail!("signalbox up failed:\n{}", text.trim_end());
                }
            }
            // The pane closed without a session: signalbox stopped right away.
            _ => return Err(anyhow!("signalbox up stopped before its session started")),
        }
        if t.elapsed() > TIMEOUT {
            bail!("no session after {} minutes; see its window: tmux -L {TMUX} attach", TIMEOUT.as_secs() / 60);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn tmux() -> Command {
    let mut c = Command::new("tmux");
    c.args(["-L", TMUX]);
    c
}

/// Run `cmd` in `root` in a new window named `name` (`window`), or in a new
/// session, printing the pane id. A tmux server gives windows its own
/// environment, not ours: pass what builds and Metro need (node from nvm…).
fn open(name: &str, root: &Path, cmd: &str, window: bool) -> Result<std::process::Output> {
    let mut t = tmux();
    if window {
        t.args(["new-window", "-d", "-t", &format!("{TMUX}:")]);
    } else {
        t.args(["new-session", "-d", "-s", TMUX]);
    }
    t.args(["-P", "-F", "#{pane_id}", "-n", name]);
    for var in ["PATH", "SIGNALBOX_HOME"] {
        if let Ok(v) = std::env::var(var) {
            t.arg("-e").arg(format!("{var}={v}"));
        }
    }
    t.arg("-c").arg(root).arg(cmd).output().context("running tmux")
}

/// The checkout's session, if it's live and was written after `since`.
fn fresh_session(root: &Path, since: SystemTime) -> Option<SessionFile> {
    let modified = std::fs::metadata(session::session_path(root)).and_then(|m| m.modified()).ok()?;
    let s = session::read_session(root)?;
    (modified >= since && session::pid_alive(s.pid)).then_some(s)
}

fn describe(s: &SessionFile) -> String {
    let device = match (&s.udid, s.platform.as_deref()) {
        (Some(u), Some("android")) => format!("device {u}"),
        (Some(u), _) => format!("simulator {u}"),
        (None, _) => "no device".into(),
    };
    format!("port {} · {device} · {}", s.port, s.status)
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `up`'s arguments for the detached run: these, minus `--detach`, and
/// throwaway simulators deleted when it stops (nobody is there to be asked).
pub fn up_args(all: &[String]) -> Vec<String> {
    let mut args: Vec<String> = all.iter().filter(|a| *a != "--detach").cloned().collect();
    if !args.iter().any(|a| a == "--sim-cleanup" || a.starts_with("--sim-cleanup=")) {
        args.extend(["--sim-cleanup".into(), "delete".into()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn detached_args() {
        assert_eq!(up_args(&v(&["--detach", "--port", "auto", "--new-sim"])), v(&["--port", "auto", "--new-sim", "--sim-cleanup", "delete"]));
        assert_eq!(up_args(&v(&["--new-sim", "--sim-cleanup", "keep", "--detach"])), v(&["--new-sim", "--sim-cleanup", "keep"]));
        assert_eq!(up_args(&v(&["--sim-cleanup=keep"])), v(&["--sim-cleanup=keep"]));
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
