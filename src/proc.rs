//! A child process running in a pseudo-terminal, with its output parsed into a
//! live terminal screen (`vt100`). Used for Metro and for `run ios`/`run android`
//! so metroctl can show their colored output and forward raw keys (Metro's `r`/`d`/`j`…)
//! exactly as a normal terminal would. Follows simon's std-thread model — a reader
//! thread pumps PTY bytes into the parser behind a mutex.

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const SCROLLBACK: usize = 5000;

pub struct PtyProcess {
    pub label: String,
    pub cmd: String,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    alive: Arc<AtomicBool>,
    exit: Arc<Mutex<Option<u32>>>,
    rows: u16,
    cols: u16,
    /// Autoscroll: keep the view on the live bottom. Off = stay put while new
    /// output arrives (vt100 shifts a non-zero scrollback offset by itself;
    /// `pin` covers the paused-at-bottom case it doesn't).
    pub follow: bool,
    pin: usize, // scrollback length when last rendered while paused
}

impl PtyProcess {
    /// Spawn `command` (run through `sh -c`) in a PTY sized `rows`×`cols`, with
    /// `cwd` as the working directory and `env` merged over the inherited env.
    pub fn spawn(label: impl Into<String>, command: &str, cwd: &Path, env: &BTreeMap<String, String>, rows: u16, cols: u16) -> Result<PtyProcess> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;

        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(command);
        cmd.cwd(cwd);
        // A sensible TERM so programs emit ANSI the vt100 parser understands.
        cmd.env("TERM", "xterm-256color");
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave); // we only need the master from here on

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK)));
        let alive = Arc::new(AtomicBool::new(true));

        let parser_r = parser.clone();
        let alive_r = alive.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser_r.lock() {
                            p.process(&buf[..n]);
                        }
                    }
                }
            }
            alive_r.store(false, Ordering::SeqCst);
        });

        Ok(PtyProcess {
            label: label.into(),
            cmd: command.to_string(),
            master: pair.master,
            writer,
            child: Arc::new(Mutex::new(child)),
            parser,
            alive,
            exit: Arc::new(Mutex::new(None)),
            rows,
            cols,
            follow: true,
            pin: 0,
        })
    }

    /// Exit code of a finished process (`None` while still running). Reaped
    /// lazily and cached, so callers can poll it each render without flicker.
    pub fn exit_code(&self) -> Option<u32> {
        if let Some(code) = *self.exit.lock().unwrap() {
            return Some(code);
        }
        let mut child = self.child.lock().ok()?;
        if let Ok(Some(status)) = child.try_wait() {
            let code = status.exit_code();
            *self.exit.lock().unwrap() = Some(code);
            return Some(code);
        }
        None
    }

    /// Forward raw input bytes (keystrokes) to the child.
    pub fn write_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Resize the PTY and the parser's screen to match the pane.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
    }

    /// Current scrollback offset (0 = bottom).
    pub fn scroll_offset(&self) -> usize {
        self.parser.lock().map(|p| p.screen().scrollback()).unwrap_or(0)
    }

    /// Scroll into history by `delta` lines (negative = toward the bottom).
    /// Scrolling up pauses autoscroll so the view doesn't jump back.
    pub fn scroll_by(&mut self, delta: isize) {
        if delta > 0 {
            self.pause();
        }
        if let Ok(mut p) = self.parser.lock() {
            let off = p.screen().scrollback().saturating_add_signed(delta);
            p.set_scrollback(off); // clamped to the available history
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_by(isize::MAX);
    }

    /// Jump to the bottom and resume autoscroll.
    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
        if let Ok(mut p) = self.parser.lock() {
            p.set_scrollback(0);
        }
    }

    pub fn toggle_follow(&mut self) {
        if self.follow {
            self.pause();
        } else {
            self.scroll_to_bottom();
        }
    }

    fn pause(&mut self) {
        if self.follow {
            self.follow = false;
            self.pin = self.scrollback_len();
        }
    }

    fn scrollback_len(&self) -> usize {
        let Ok(mut p) = self.parser.lock() else {
            return 0;
        };
        let off = p.screen().scrollback();
        p.set_scrollback(usize::MAX);
        let len = p.screen().scrollback();
        p.set_scrollback(off);
        len
    }

    /// Before rendering: keep a paused view anchored. vt100 only shifts the
    /// offset when it's already > 0, so a view paused at the bottom is moved
    /// up by however many lines arrived since.
    pub fn sync_scroll(&mut self) {
        if self.follow {
            if let Ok(mut p) = self.parser.lock() {
                p.set_scrollback(0);
            }
            return;
        }
        let len = self.scrollback_len();
        if let Ok(mut p) = self.parser.lock() {
            if p.screen().scrollback() == 0 && len > self.pin {
                p.set_scrollback(len - self.pin);
            }
        }
        self.pin = len;
    }

    /// Clone handle to the parser so the renderer can read the screen.
    pub fn parser(&self) -> Arc<Mutex<vt100::Parser>> {
        self.parser.clone()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Terminate the child (best effort).
    pub fn kill(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_child_output_into_the_screen() {
        let env = BTreeMap::new();
        let p = PtyProcess::spawn("test", "printf 'hello pty'", Path::new("/"), &env, 24, 80).expect("spawn");
        // Poll the parsed screen until the output shows up (bounded).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let contents = p.parser().lock().unwrap().screen().contents();
            if contents.contains("hello pty") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "child output never reached the screen");
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    #[test]
    fn paused_view_stays_put_while_output_arrives() {
        let env = BTreeMap::new();
        let mut p = PtyProcess::spawn("test", "seq 1 50; sleep 0.5; seq 51 100", Path::new("/"), &env, 10, 40).expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(250));
        p.toggle_follow(); // pause at the bottom, showing up to line 50
        let bottom_row = |p: &PtyProcess| p.parser().lock().unwrap().screen().rows(0, 40).nth(8).unwrap_or_default();
        p.sync_scroll();
        let before = bottom_row(&p);
        assert_eq!(before.trim(), "50");
        std::thread::sleep(std::time::Duration::from_millis(800));
        p.sync_scroll();
        assert_eq!(bottom_row(&p), before, "paused view moved");
        p.scroll_to_bottom();
        p.sync_scroll();
        assert_eq!(p.scroll_offset(), 0);
        assert!(p.follow);
    }

    #[test]
    fn write_input_reaches_the_child() {
        let env = BTreeMap::new();
        // `cat` echoes back whatever we type into the PTY.
        let mut p = PtyProcess::spawn("test", "cat", Path::new("/"), &env, 24, 80).expect("spawn");
        p.write_input(b"ping\r");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let contents = p.parser().lock().unwrap().screen().contents();
            if contents.contains("ping") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "input never echoed back");
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}
