//! The deep-link picker (`l` in the Devices pane): quick-pick keys, `/`
//! search, `:` for any URL, and the form that asks for a link's
//! placeholders. It only decides which URL to open; the dashboard opens it on
//! the selected device.

use super::popups::centered;
use crate::rnconfig::{fill_link, link_placeholders, DeepLink};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::HashMap;

/// What the dashboard should do after a key or paste the picker handled.
pub(super) enum LinkAction {
    Open(String),
    Flash(&'static str),
}

#[derive(Default)]
pub(super) struct Links {
    picker: bool,                    // the quick-picker overlay is open
    search: Option<(String, usize)>, // `/` in the picker: filter query and selected match
    form: Option<LinkForm>,          // values for a picked link's placeholders, or a pasted URL
    values: HashMap<String, String>, // last value per placeholder name, prefilled next time
}

/// Values for a link's placeholders (`template` set), or one pasted URL.
struct LinkForm {
    title: String,
    template: String,
    values: Vec<(String, String)>,
    field: usize,
}

impl Links {
    pub(super) fn open_picker(&mut self) {
        self.picker = true;
    }

    /// The picker or the form is up and takes the keys.
    pub(super) fn is_open(&self) -> bool {
        self.picker || self.form.is_some()
    }

    /// A key while `is_open`.
    pub(super) fn on_key(&mut self, key: KeyEvent, links: &[DeepLink]) -> Option<LinkAction> {
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(form) = self.form.as_mut() {
            match key.code {
                KeyCode::Esc => self.form = None,
                _ if ctrl_c => self.form = None,
                KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % form.values.len(),
                KeyCode::BackTab | KeyCode::Up => {
                    form.field = (form.field + form.values.len() - 1) % form.values.len()
                }
                KeyCode::Backspace => {
                    form.values[form.field].1.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.values[form.field].1.clear()
                }
                KeyCode::Enter if form.field + 1 < form.values.len() => form.field += 1,
                KeyCode::Enter => {
                    if form.values.iter().any(|(_, v)| v.trim().is_empty()) {
                        return Some(LinkAction::Flash("fill in every value"));
                    }
                    let form = self.form.take().unwrap();
                    let url = if form.template.is_empty() {
                        form.values[0].1.trim().to_string() // a pasted URL, as is
                    } else {
                        for (k, v) in &form.values {
                            self.values.insert(k.clone(), v.clone());
                        }
                        fill_link(&form.template, &form.values)
                    };
                    return Some(LinkAction::Open(url));
                }
                KeyCode::Char(c) => form.values[form.field].1.push(c),
                _ => {}
            }
            return None;
        }

        if let Some((query, sel)) = self.search.as_mut() {
            let matches = link_matches(links, query);
            match key.code {
                KeyCode::Esc => self.search = None,
                _ if ctrl_c => self.search = None,
                KeyCode::Down | KeyCode::Tab => {
                    *sel = (*sel + 1).min(matches.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::BackTab => *sel = sel.saturating_sub(1),
                KeyCode::Backspace if query.is_empty() => self.search = None,
                KeyCode::Backspace => {
                    query.pop();
                    *sel = 0;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.clear();
                    *sel = 0;
                }
                KeyCode::Enter => {
                    if let Some(&i) = matches.get(*sel) {
                        self.search = None;
                        self.picker = false;
                        return self.pick(links, i);
                    }
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    *sel = 0;
                }
                _ => {}
            }
            return None;
        }

        match key.code {
            KeyCode::Esc => self.picker = false,
            KeyCode::Char('/') => self.search = Some((String::new(), 0)),
            KeyCode::Char(':') => {
                // Any URL, e.g. a magic login link pasted from an email.
                self.picker = false;
                self.form = Some(LinkForm {
                    title: "Open a URL".into(),
                    template: String::new(),
                    values: vec![("url".into(), String::new())],
                    field: 0,
                });
            }
            KeyCode::Char(c) => {
                if let Some(i) = picker_index(c).filter(|&i| i < links.len()) {
                    self.picker = false;
                    return self.pick(links, i);
                }
            }
            _ => {}
        }
        None
    }

    /// Text pasted into the terminal, when the form is up: into its current
    /// field. Returns whether it was taken.
    pub(super) fn on_paste(&mut self, text: &str) -> bool {
        match self.form.as_mut() {
            Some(form) => {
                form.values[form.field].1.push_str(text.trim());
                true
            }
            None => false,
        }
    }

    /// Open link `idx`, asking for its placeholders' values first.
    fn pick(&mut self, links: &[DeepLink], idx: usize) -> Option<LinkAction> {
        let link = links.get(idx)?;
        let names = link_placeholders(link.url());
        if names.is_empty() {
            return Some(LinkAction::Open(link.url().to_string()));
        }
        let values = names
            .into_iter()
            .map(|n| {
                let v = self.values.get(&n).cloned().unwrap_or_default();
                (n, v)
            })
            .collect();
        self.form = Some(LinkForm {
            title: link.label().to_string(),
            template: link.url().to_string(),
            values,
            field: 0,
        });
        None
    }

    pub(super) fn render(&self, links: &[DeepLink], frame: &mut Frame, area: Rect) {
        if self.picker {
            render_picker(links, self.search.as_ref(), frame, area);
        }
        if let Some(form) = &self.form {
            render_form(form, frame, area);
        }
    }
}

/// Quick-pick key for item `i`: 1-9, then a-z, then A-Z once those run out.
pub(super) fn picker_char(i: usize) -> Option<char> {
    if i < 9 {
        Some((b'1' + i as u8) as char)
    } else if i < 9 + 26 {
        Some((b'a' + (i - 9) as u8) as char)
    } else if i < 9 + 26 + 26 {
        Some((b'A' + (i - 9 - 26) as u8) as char)
    } else {
        None
    }
}

/// Inverse of `picker_char`.
pub(super) fn picker_index(c: char) -> Option<usize> {
    match c {
        '1'..='9' => Some(c as usize - '1' as usize),
        'a'..='z' => Some(9 + (c as usize - 'a' as usize)),
        'A'..='Z' => Some(9 + 26 + (c as usize - 'A' as usize)),
        _ => None,
    }
}

/// Indexes of the deep links whose name or URL contains `query` (any case).
fn link_matches(links: &[DeepLink], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    links
        .iter()
        .enumerate()
        .filter(|(_, d)| {
            d.label().to_lowercase().contains(&q) || d.url().to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

fn render_form(form: &LinkForm, frame: &mut Frame, area: Rect) {
    let w = 76.min(area.width);
    let h = (form.values.len() as u16 * 2 + 5).min(area.height);
    let r = centered(area, w, h);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" {} ", form.title));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let mut lines = vec![Line::raw("")];
    // Names left-aligned in a column as wide as the longest, values lined up after it.
    let name_w = form
        .values
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    let room = inner.width.saturating_sub(name_w as u16 + 4) as usize;
    for (i, (name, value)) in form.values.iter().enumerate() {
        let active = i == form.field;
        // Long values (tokens, magic links): show the end, where typing happens.
        let chars: Vec<char> = value.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {name:<name_w$}  "),
                if active {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw(if active { format!("{shown}█") } else { shown }),
        ]));
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(
        " type or paste · ⇥ next · ⏎ open · esc cancel",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_picker(
    links: &[DeepLink],
    search: Option<&(String, usize)>,
    frame: &mut Frame,
    area: Rect,
) {
    let shown: Vec<usize> = match search {
        Some((q, _)) => link_matches(links, q),
        None => (0..links.len())
            .filter(|&i| picker_char(i).is_some())
            .collect(),
    };
    let key_style = Style::default().fg(Color::Black).bg(Color::Cyan);

    // The list itself, with a heading above each run of links in one group.
    let mut body: Vec<Line> = Vec::new();
    let mut heading_rows: Vec<usize> = Vec::new();
    let mut group: Option<&str> = None;
    for (row, &i) in shown.iter().enumerate() {
        let d = &links[i];
        if d.group().is_some() && d.group() != group {
            if !body.is_empty() {
                body.push(Line::raw(""));
            }
            heading_rows.push(body.len());
            body.push(Line::styled(
                format!(" {}", d.group().unwrap()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        group = d.group();
        // While searching, keys are typed into the query: ⏎ opens the highlighted match.
        let (key, label) = match search {
            Some((_, sel)) if *sel == row => (
                "  ▸".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Some(_) => ("   ".to_string(), Style::default()),
            None => (
                format!(" {} ", picker_char(i).unwrap_or(' ')),
                Style::default(),
            ),
        };
        body.push(Line::from(vec![
            Span::styled(
                key,
                if search.is_some() {
                    Style::default().fg(Color::Cyan)
                } else {
                    key_style
                },
            ),
            Span::styled(format!("  {}", d.label()), label),
            Span::styled(
                link_placeholders(d.url())
                    .iter()
                    .map(|p| format!(" ‹{p}›"))
                    .collect::<String>(),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    let mut head: Vec<Line> = Vec::new();
    if let Some((q, _)) = search {
        head.push(Line::from(vec![
            Span::styled(" / ", key_style),
            Span::raw(format!("  {q}█")),
        ]));
        head.push(Line::raw(""));
    }
    let foot: Vec<Line> = vec![
        Line::raw(""),
        if search.is_some() {
            Line::styled(
                " type to filter · ↑↓ select · ⏎ open · esc back",
                Style::default().fg(Color::DarkGray),
            )
        } else {
            Line::from(vec![
                Span::styled(" / ", key_style),
                Span::raw("  search    "),
                Span::styled(" : ", key_style),
                Span::raw("  paste any URL (e.g. a magic login link)"),
            ])
        },
    ];

    // Too tall for the terminal: split into two columns, at a group heading near the middle.
    let chrome = (head.len() + foot.len() + 2) as u16;
    let split = if body.len() as u16 + chrome > area.height && area.width >= 128 {
        let mid = body.len().div_ceil(2);
        let at = heading_rows
            .iter()
            .copied()
            .filter(|&r| r > 0)
            .min_by_key(|&r| r.abs_diff(mid))
            .filter(|&r| r.abs_diff(mid) <= mid / 3)
            .unwrap_or(mid);
        Some(at)
    } else {
        None
    };
    let (rows, w) = match split {
        Some(at) => (at.max(body.len() - at), 128),
        None => (body.len(), 64),
    };
    let r = centered(area, w, (rows as u16 + chrome).clamp(6, area.height));
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Deep links · esc ");
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let [head_area, body_area, foot_area] = Layout::vertical([
        Constraint::Length(head.len() as u16),
        Constraint::Min(0),
        Constraint::Length(foot.len() as u16),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(Text::from(head)), head_area);
    frame.render_widget(Paragraph::new(Text::from(foot)), foot_area);
    match split {
        Some(at) => {
            let right: Vec<Line> = body
                .split_off(at)
                .into_iter()
                .skip_while(|l| l.width() == 0)
                .collect();
            let [left_area, right_area] =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(body_area);
            frame.render_widget(Paragraph::new(Text::from(body)), left_area);
            frame.render_widget(Paragraph::new(Text::from(right)), right_area);
        }
        None => frame.render_widget(Paragraph::new(Text::from(body)), body_area),
    }
}
