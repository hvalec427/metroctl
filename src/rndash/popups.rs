//! The `?` key reference and the quit confirmation.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

type KeySection = (&'static str, &'static [(&'static str, &'static str)]);

// Left / right columns of the `?` popup (stacked when the terminal is narrow).
const HELP_LEFT: &[KeySection] = &[
    (
        "Global",
        &[
            ("⇥ / ⇧⇥", "next / previous pane"),
            ("^←→↑↓", "resize panes"),
            ("R", "reload the app"),
            ("D", "open the dev menu"),
            ("?", "this help"),
            ("q / ^C", "quit"),
        ],
    ),
    (
        "Every pane",
        &[
            ("j k  ↑ ↓", "move / scroll"),
            ("g  G", "top / bottom"),
            ("space", "autoscroll on/off"),
            ("[  ]", "previous / next tab"),
        ],
    ),
    (
        "Processes",
        &[
            ("⏎", "type into the process (Esc leaves)"),
            ("x", "stop it"),
            ("d", "delete a stopped tab"),
            ("m", "start / restart Metro (takes over an external one)"),
            ("c", "deep clean (reinstalls)"),
            ("i", "reinstall deps + pods"),
            ("a", "reset: deep clean, then Metro"),
        ],
    ),
    (
        "Devices",
        &[
            ("⏎", "start the simulator / emulator"),
            ("s", "stop it"),
            ("b", "build & run on it"),
            ("o", "open the app"),
            (
                "l",
                "open a deep link (fills ‹placeholders›; / search, : any URL)",
            ),
        ],
    ),
];

const HELP_RIGHT: &[KeySection] = &[
    (
        "Logs · Network · Perf",
        &[
            ("1-9", "switch device"),
            ("/  n N", "search, next / previous match"),
            ("f", "filter"),
            ("⏎", "open the entry"),
            ("z", "open maximized / toggle split"),
            ("V", "visual select, then y"),
            ("y", "copy"),
            ("c", "clear"),
            ("p", "clear on reload on/off"),
        ],
    ),
    (
        "Open entry",
        &[
            ("⏎ / Esc", "close"),
            ("J K", "scroll the entry (split view)"),
            ("{  }", "previous / next section"),
            ("o", "open file:line in nvim"),
            ("F", "show framework stack frames"),
        ],
    ),
    (
        "Network",
        &[
            ("e", "errors only"),
            ("m", "cycle method filter"),
            ("c / C", "copy as curl"),
        ],
    ),
    (
        "Components",
        &[
            ("s", "select by tapping it in the app"),
            ("h l  ← →", "fold / unfold, or out / in"),
            ("u", "parent"),
            ("⏎", "props, state, hooks, source"),
        ],
    ),
    (
        "Debugger (paused)",
        &[
            ("F5", "continue"),
            ("F10", "step over"),
            ("F11 / ⇧F11", "step into / out"),
            ("F6", "pause"),
        ],
    ),
];

fn help_lines(sections: &[KeySection]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, (title, keys)) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(
            format!(" {title}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        for (k, desc) in keys.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("   {k:<12}"), Style::default().fg(Color::Yellow)),
                Span::raw(*desc),
            ]));
        }
    }
    lines
}

/// The `?` key reference, scrolled to `top` (clamped so G lands on the last page).
pub(super) fn render_help(top: &mut u16, frame: &mut Frame, area: Rect) {
    let (left, right) = (help_lines(HELP_LEFT), help_lines(HELP_RIGHT));
    let two_cols = area.width >= 110;
    let content_h = if two_cols {
        left.len().max(right.len())
    } else {
        left.len() + 1 + right.len()
    } as u16;
    let w = if two_cols { 120 } else { 64 };
    let r = centered(area, w, content_h + 2);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Keys ")
        .title_bottom(Line::from(" ? / esc close ").right_aligned());
    let inner = block.inner(r);
    frame.render_widget(block, r);
    *top = (*top).min(content_h.saturating_sub(inner.height));
    let top = *top;
    if two_cols {
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        frame.render_widget(Paragraph::new(Text::from(left)).scroll((top, 0)), cols[0]);
        frame.render_widget(Paragraph::new(Text::from(right)).scroll((top, 0)), cols[1]);
    } else {
        let mut all = left;
        all.push(Line::raw(""));
        all.extend(right);
        frame.render_widget(Paragraph::new(Text::from(all)).scroll((top, 0)), inner);
    }
}

pub(super) fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// `created`: the device this session created and what quitting can do with
/// it ("delete" a simulator, "stop" an emulator), to offer that.
pub(super) fn render_quit_popup(
    frame: &mut Frame,
    area: Rect,
    created: Option<(String, &'static str)>,
) {
    if let Some((sim, verb)) = created {
        let r = centered(area, 64, 6);
        frame.render_widget(Clear, r);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" Quit signalbox? ");
        let inner = block.inner(r);
        frame.render_widget(block, r);
        let lines = vec![
            Line::raw(""),
            Line::from(Span::raw("  This stops Metro and any running builds.")),
            Line::from(Span::raw(format!("  This session created {sim}."))),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("d", Style::default().fg(Color::Red)),
                Span::raw(format!(" quit, {verb} it  ")),
                Span::styled("k/⏎", Style::default().fg(Color::Green)),
                Span::raw(" quit, keep it  "),
                Span::styled("esc", Style::default().fg(Color::Cyan)),
                Span::raw(" cancel"),
            ]),
        ];
        frame.render_widget(Paragraph::new(Text::from(lines)), inner);
        return;
    }
    let r = centered(area, 50, 5);
    frame.render_widget(Clear, r); // wipe whatever's underneath
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Quit signalbox? ");
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let lines = vec![
        Line::raw(""),
        Line::from(Span::raw("  This stops Metro and any running builds.")),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("y/⏎", Style::default().fg(Color::Green)),
            Span::raw(" quit    "),
            Span::styled("n/esc", Style::default().fg(Color::Cyan)),
            Span::raw(" cancel"),
        ]),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}
