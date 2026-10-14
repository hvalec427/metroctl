//! The Processes pane's terminal plumbing: keys in as the bytes a terminal
//! sends, and a child's vt100 screen out as styled lines.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;

/// Translate a key event into the bytes a terminal would send to the child.
pub(super) fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii_alphabetic() {
                vec![(c.to_ascii_lowercase() as u8) & 0x1f]
            } else {
                c.to_string().into_bytes()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

pub(super) fn pty_lines(
    parser: &std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let guard = match parser.lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let screen = guard.screen();
    let (srows, scols) = screen.size();
    let h = height.min(srows);
    let w = width.min(scols);
    let mut lines: Vec<Line> = Vec::with_capacity(height as usize);
    for row in 0..h {
        let mut spans: Vec<Span> = Vec::new();
        let mut run = String::new();
        let mut run_style = Style::default();
        for col in 0..w {
            let (glyph, style) = match screen.cell(row, col) {
                Some(cell) => {
                    let g = cell.contents();
                    (
                        if g.is_empty() { " ".to_string() } else { g },
                        cell_style(cell),
                    )
                }
                None => (" ".to_string(), Style::default()),
            };
            if style != run_style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
            run.push_str(&glyph);
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, run_style));
        }
        lines.push(Line::from(spans));
    }
    while lines.len() < height as usize {
        lines.push(Line::raw(""));
    }
    lines
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut s = Style::default();
    if let Some(fg) = conv_color(cell.fgcolor()) {
        s = s.fg(fg);
    }
    if let Some(bg) = conv_color(cell.bgcolor()) {
        s = s.bg(bg);
    }
    if cell.bold() {
        s = s.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

fn conv_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}
