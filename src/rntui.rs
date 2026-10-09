//! Standalone `logs --rn` viewer: a full-screen wrapper around `RnView`. All the
//! Logs/Network/Perf state and rendering lives in `rnview`; this just owns the
//! terminal, the `RnClient`, and the event loop.

use crate::rnclient::RnClient;
use crate::rnview::RnView;
use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use std::time::Duration;

pub fn run(port: u16, name: Option<&str>) -> Result<()> {
    let client = RnClient::start(port);
    let mut view = RnView::new(name);
    // Standalone `logs` is run from the project dir — resolve frame paths against it.
    view.set_root(std::env::current_dir().ok().and_then(|p| p.to_str().map(String::from)));
    let mut terminal = ratatui::init();
    let res = main_loop(&mut view, &mut terminal, &client);
    ratatui::restore();
    res
}

fn main_loop(view: &mut RnView, terminal: &mut ratatui::DefaultTerminal, client: &RnClient) -> Result<()> {
    loop {
        while let Ok(ev) = client.rx.try_recv() {
            view.on_event(ev);
        }
        terminal.draw(|f| view.render(f, f.area()))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && view.on_key(k, client) {
                    return Ok(());
                }
            }
        }
    }
}
