//! Drawing: header, item/list panes, search & repo overlays, editor.

use crate::app::{state_style, App, Pane, VIEWS};
use crate::backend::COLUMNS;
use edtui::{EditorTheme, EditorView};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

const ACCENT: Color = Color::Cyan;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .areas(f.area());

    draw_header(f, app, header);

    if let Some(ed) = &mut app.editor {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" edit (vim keys) ");
        let inner = block.inner(body);
        f.render_widget(block, body);
        let view = EditorView::new(&mut ed.state).theme(EditorTheme::default()).wrap(true);
        f.render_widget(view, inner);
        let hint = if ed.saving { " saving… " } else { " ctrl+s save & close · ctrl+q discard " };
        f.render_widget(Paragraph::new(hint).style(Style::default().fg(Color::Yellow)), footer);
        return;
    }

    match app.stack.last_mut() {
        Some(Pane::Item(_)) => draw_item(f, app, body),
        Some(Pane::List(_)) => draw_list(f, app, body),
        None => {
            f.render_widget(Paragraph::new("\n\n  R pick a repo · ctrl+k search").dark_gray(), body);
        }
    }

    draw_footer(f, app, footer);

    if app.meta.is_some() {
        draw_meta(f, app, body);
    }
    if app.search.is_some() {
        draw_search(f, app, body);
    }
    if app.repos.is_some() {
        draw_repos(f, app, body);
    }
}

/// Centered popup of the given size, clamped to `area`.
fn popup(area: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(area.width), h.min(area.height));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn selectable<'a>(items: Vec<Line<'a>>, sel: usize, height: u16) -> List<'a> {
    let items: Vec<ListItem> = items
        .into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            if i == sel {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            ListItem::new(line)
        })
        .collect();
    let offset = sel.saturating_sub(height.saturating_sub(1) as usize);
    List::new(items.into_iter().skip(offset))
}

fn draw_meta(f: &mut Frame, app: &App, area: Rect) {
    use crate::app::MetaMode;
    let Some(m) = &app.meta else { return };
    let widest = m.fields.iter().map(|fl| fl.name.chars().count()).max().unwrap_or(8);
    let rows = match &m.mode {
        MetaMode::List => m.fields.len(),
        MetaMode::Pick { options, .. } => options.len() + 1,
        MetaMode::MultiPick { options, .. } => options.len(),
        MetaMode::Input { .. } => 3,
    };
    let popup = popup(area, 64, ((rows + 2) as u16).clamp(6, area.height.saturating_sub(2)));
    f.render_widget(Clear, popup);
    let hint = match &m.mode {
        MetaMode::List => " enter edit · esc close ",
        MetaMode::Input { .. } => " enter save · esc cancel ",
        MetaMode::Pick { .. } => " enter pick · esc cancel ",
        MetaMode::MultiPick { .. } => " space toggle · enter save · esc cancel ",
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow))
        .title(format!(" #{} metadata ", m.number))
        .title_bottom(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    match &m.mode {
        MetaMode::List => {
            let items = m
                .fields
                .iter()
                .map(|fl| {
                    Line::from(vec![
                        Span::styled(format!("{:widest$}  ", fl.name), Style::default().fg(ACCENT)),
                        Span::raw(fl.value.clone()),
                    ])
                })
                .collect();
            f.render_widget(selectable(items, m.sel, inner.height), inner);
        }
        MetaMode::Input { buf, pos } => {
            let field = &m.fields[m.sel];
            let (before, under, after, cut_l, cut_r) =
                input_window(buf, *pos, inner.width.saturating_sub(4) as usize);
            let dim = Style::default().fg(Color::DarkGray);
            let mut input = vec![Span::styled("❯ ", Style::default().fg(Color::Yellow))];
            if cut_l {
                input.push(Span::styled("…", dim));
            }
            input.push(Span::raw(before));
            input.push(Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)));
            input.push(Span::raw(after));
            if cut_r {
                input.push(Span::styled("…", dim));
            }
            let lines = vec![
                Line::from(Span::styled(field.name, Style::default().fg(ACCENT))),
                Line::default(),
                Line::from(input),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
        MetaMode::Pick { options, sel } => {
            let mut items = vec![Line::from(Span::styled("(clear)", Style::default().fg(Color::DarkGray)))];
            items.extend(options.iter().map(|o| Line::from(o.clone())));
            f.render_widget(selectable(items, *sel, inner.height), inner);
        }
        MetaMode::MultiPick { options, chosen, sel } => {
            let items = options
                .iter()
                .zip(chosen.iter())
                .map(|(o, c)| {
                    let mark = if *c { "☑ " } else { "☐ " };
                    let style = if *c { Style::default().fg(Color::Green) } else { Style::default() };
                    Line::from(vec![Span::styled(mark, style), Span::raw(o.clone())])
                })
                .collect();
            f.render_widget(selectable(items, *sel, inner.height), inner);
        }
    }
}

/// Visible slice of a single-line input, scrolled so the cursor stays in
/// view: (before, under-cursor, after, cut-left, cut-right), within `width`
/// columns including the cursor cell.
fn input_window(buf: &str, pos: usize, width: usize) -> (String, String, String, bool, bool) {
    let width = width.max(2);
    let chars: Vec<char> = buf.chars().collect();
    let pos = pos.min(chars.len());
    let start = (pos + 1).saturating_sub(width);
    let end = (start + width).min(chars.len());
    let before: String = chars[start..pos].iter().collect();
    let under = chars.get(pos).map_or(" ".to_string(), |c| c.to_string());
    let after: String = chars.get(pos + 1..end).unwrap_or(&[]).iter().collect();
    (before, under, after, start > 0, end < chars.len())
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled(" github-tui ", Style::default().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw(" "),
    ];
    let crumbs: Vec<String> = app.stack.iter().map(Pane::title).collect();
    let start = crumbs.len().saturating_sub(3);
    if start > 0 {
        spans.push(Span::styled("… › ", Style::default().fg(Color::DarkGray)));
    }
    for (i, c) in crumbs[start..].iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" › ", Style::default().fg(Color::DarkGray)));
        }
        let last = i == crumbs[start..].len() - 1;
        let style = if last { Style::default().bold() } else { Style::default().fg(Color::Gray) };
        spans.push(Span::styled(c.clone(), style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    if app.backend.pending > 0 {
        let busy = Paragraph::new(Span::styled("⟳ ", Style::default().fg(Color::Yellow))).right_aligned();
        f.render_widget(busy, area);
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if app.find_input {
        let q = match app.stack.last() {
            Some(Pane::Item(p)) => p.find.as_str(),
            Some(Pane::List(t)) => t.find.as_str(),
            None => "",
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" /", Style::default().fg(Color::Yellow).bold()),
                Span::raw(q.to_string()),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
                Span::styled("  enter keep · esc clear", Style::default().fg(Color::DarkGray)),
            ])),
            area,
        );
        return;
    }
    if let Some(Pane::Item(p)) = app.stack.last()
        && !p.find.is_empty()
    {
        let text = if p.matches.is_empty() {
            format!(" /{} · no matches · esc clear", p.find)
        } else {
            format!(" /{} · match {}/{} · n/N next/prev · esc clear", p.find, p.msel + 1, p.matches.len())
        };
        f.render_widget(Paragraph::new(text).style(Style::default().fg(Color::Yellow)), area);
        return;
    }
    let text = match &app.status {
        Some((msg, _)) => msg.clone(),
        None => match app.stack.last() {
            Some(Pane::Item(p)) if p.is_pr => {
                " j/k · / find · tab links · e edit · E vim · m metadata · C comment · c checkout · o browser · r refresh · ctrl+k · R repos · esc back · q quit".into()
            }
            Some(Pane::Item(_)) => {
                " j/k · / find · tab links · e edit · E vim · m metadata · C comment · o browser · r refresh · ctrl+k · R repos · esc back · q quit".into()
            }
            Some(Pane::List(t)) if t.picker.is_some() => " j/k select view · enter apply · esc cancel · q quit".into(),
            Some(Pane::List(_)) => {
                " j/k rows · / filter · h/l cols · x expand · v views · a new issue · enter open · o browser · r refresh · ctrl+k · R repos · q quit".into()
            }
            None => " R repos · ctrl+k search · q quit".into(),
        },
    };
    f.render_widget(Paragraph::new(text).style(Style::default().fg(Color::DarkGray)), area);
}

fn draw_item(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(Pane::Item(p)) = app.stack.last_mut() else { return };
    p.viewport = area.height.saturating_sub(2);
    let mut lines = p.lines.clone();
    if let Some(link) = p.link_sel.and_then(|i| p.links.get(i))
        && let Some(line) = lines.get_mut(link.line)
    {
        for si in link.spans.0..link.spans.1.min(line.spans.len()) {
            line.spans[si].style = line.spans[si].style.add_modifier(Modifier::REVERSED);
        }
    }
    if !p.find.is_empty() {
        let q: Vec<char> = p.find.to_lowercase().chars().collect();
        let cur = p.matches.get(p.msel).copied();
        for (i, line) in lines.iter_mut().enumerate() {
            let hl = if Some(i) == cur {
                Style::default().bg(Color::Yellow).fg(Color::Black)
            } else {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            };
            highlight_find(line, &q, hl);
        }
    }
    let icon = if p.is_pr { "⇄" } else { "●" };
    let mut title = vec![
        Span::styled(format!(" {icon} #{} ", p.number), state_style(&p.state)),
        Span::styled(p.title.clone(), Style::default().fg(ACCENT)),
    ];
    if !p.state.is_empty() {
        title.push(Span::styled(format!(" · {}", p.state), state_style(&p.state)));
    }
    title.push(Span::styled(if p.loaded { " " } else { " (cache) " }, Style::default().fg(Color::DarkGray)));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(title));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((p.scroll as u16, 0));
    f.render_widget(para, area);
}

/// Greedy word-wrap to `width` chars, hard-breaking oversized words; at most
/// `max_lines` lines, with an ellipsis when content is cut off.
fn wrap_cell(s: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur: Vec<char> = Vec::new();
    for word in s.split_whitespace() {
        for chunk in word.chars().collect::<Vec<_>>().chunks(width) {
            let sep = usize::from(!cur.is_empty());
            if cur.len() + sep + chunk.len() > width {
                lines.push(cur.drain(..).collect());
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.extend(chunk);
        }
    }
    if !cur.is_empty() {
        lines.push(cur.into_iter().collect());
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = lines.last_mut().expect("non-empty");
        let mut chars: Vec<char> = last.chars().collect();
        chars.truncate(width.saturating_sub(1).max(1));
        chars.push('…');
        *last = chars.into_iter().collect();
    }
    lines
}

/// Restyle every case-insensitive occurrence of `query` (as lowercase chars) in the line.
fn highlight_find(line: &mut Line<'static>, query: &[char], hl: Style) {
    let m = query.len();
    if m == 0 {
        return;
    }
    let chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
    let lchars: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let n = chars.len();
    if n < m {
        return;
    }
    let mut mark = vec![false; n];
    let mut found = false;
    let mut i = 0;
    while i + m <= n {
        if lchars[i..i + m] == *query {
            mark[i..i + m].iter_mut().for_each(|x| *x = true);
            found = true;
            i += m;
        } else {
            i += 1;
        }
    }
    if !found {
        return;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut ci = 0;
    for span in &line.spans {
        let mut seg = String::new();
        let mut seg_marked = false;
        for ch in span.content.chars() {
            if !seg.is_empty() && mark[ci] != seg_marked {
                out.push(Span::styled(std::mem::take(&mut seg), if seg_marked { hl } else { span.style }));
            }
            seg_marked = mark[ci];
            seg.push(ch);
            ci += 1;
        }
        if !seg.is_empty() {
            out.push(Span::styled(seg, if seg_marked { hl } else { span.style }));
        }
    }
    line.spans = out;
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(Pane::List(t)) = app.stack.last_mut() else { return };
    let doc = &t.doc;
    let col_width = |i: usize| -> u16 {
        let cap = if i == 1 { 80 } else { 30 };
        doc.rows
            .iter()
            .map(|r| r.cells.get(i).map_or(0, |c| c.chars().count()))
            .chain([COLUMNS[i].chars().count()])
            .max()
            .unwrap_or(6)
            .clamp(4, cap) as u16
    };
    // number + title pinned, then as many scrolled columns as actually fit
    let avail = area.width.saturating_sub(2);
    let mut idx: Vec<usize> = vec![0, 1];
    let mut used = col_width(0) + 2 + col_width(1).min(avail.saturating_sub(col_width(0) + 2));
    let mut widths: Vec<Constraint> = vec![Constraint::Length(col_width(0)), Constraint::Length(used - col_width(0) - 2)];
    for i in 2 + t.col_offset..COLUMNS.len() {
        let w = col_width(i);
        if used + 2 + w > avail {
            break;
        }
        used += 2 + w;
        idx.push(i);
        widths.push(Constraint::Length(w));
    }
    let header = Row::new(
        idx.iter().map(|&i| Span::styled(COLUMNS[i], Style::default().fg(ACCENT).bold())),
    );
    let col_widths: Vec<usize> = widths
        .iter()
        .map(|c| match c {
            Constraint::Length(w) => *w as usize,
            _ => 10,
        })
        .collect();
    let vis = t.visible();
    let expanded = t.expanded;
    let rows = vis.iter().map(|&ri| {
        let r = &doc.rows[ri];
        let cell_style = |i: usize, text: &str| match i {
            0 => Style::default().fg(Color::DarkGray),
            2 => state_style(text),
            _ => Style::default(),
        };
        if expanded {
            let cells: Vec<Vec<String>> = idx
                .iter()
                .zip(&col_widths)
                .map(|(&i, &w)| wrap_cell(r.cells.get(i).map_or("", String::as_str), w, 8))
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
            Row::new(cells.into_iter().zip(&idx).map(|(lines, &i)| {
                let style = cell_style(i, lines.first().map_or("", String::as_str));
                Text::from(lines.into_iter().map(|l| Line::from(Span::styled(l, style))).collect::<Vec<_>>())
            }))
            .height(height as u16)
            .bottom_margin(1)
        } else {
            Row::new(idx.iter().map(|&i| {
                let cell = r.cells.get(i).cloned().unwrap_or_default();
                let style = cell_style(i, &cell);
                Text::from(Span::styled(cell.chars().take(80).collect::<String>(), style))
            }))
        }
    });
    let plus = if doc.has_more { "+" } else { "" };
    let more = if t.find.is_empty() {
        format!(" {}{plus} rows ", doc.rows.len())
    } else {
        format!(" {}/{}{plus} rows · /{} ", vis.len(), doc.rows.len(), t.find)
    };
    let title = format!(" ▦ {} · {}{} ", t.repo, t.label(), if t.loaded { "" } else { " (cache)" });
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(title, Style::default().fg(ACCENT)))
        .title_bottom(Span::styled(more, Style::default().fg(Color::DarkGray)));
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(2)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(table, area, &mut t.state);

    if let Some(sel) = t.picker {
        let popup = popup(area, 34, (VIEWS.len() + 2) as u16);
        f.render_widget(Clear, popup);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(" views ");
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let items = VIEWS
            .iter()
            .map(|(label, kind, _)| {
                let (icon, color) = if *kind == "pulls" { ("⇄ ", Color::Magenta) } else { ("● ", Color::Green) };
                Line::from(vec![Span::styled(icon, Style::default().fg(color)), Span::raw(*label)])
            })
            .collect();
        f.render_widget(selectable(items, sel, inner.height), inner);
    }
}

/// Bordered popup with a prompt line on top; returns the list area below it.
fn prompt_popup(f: &mut Frame, area: Rect, title: &str, hint: &str, input: &str) -> Rect {
    let w = (area.width * 7 / 10).clamp(30, 90);
    let h = (area.height * 7 / 10).clamp(10, 30);
    let popup = popup(area, w, h);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(title.to_string())
        .title_bottom(Span::styled(hint.to_string(), Style::default().fg(Color::DarkGray)));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let [input_area, list_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯ ", Style::default().fg(ACCENT)),
            Span::raw(input.to_string()),
            Span::styled("▏", Style::default().fg(ACCENT)),
        ])),
        input_area,
    );
    list_area
}

fn draw_search(f: &mut Frame, app: &App, area: Rect) {
    let Some(s) = &app.search else { return };
    let list_area = prompt_popup(f, area, &format!(" search {} ", s.repo), " enter open · esc close ", &s.input);
    let items = s
        .hits
        .iter()
        .map(|hit| {
            let icon = if hit.is_pr { "⇄ " } else { "● " };
            Line::from(vec![
                Span::styled(icon, state_style(&hit.state)),
                Span::styled(format!("#{} ", hit.number), Style::default().fg(Color::DarkGray)),
                Span::raw(hit.title.clone()),
            ])
        })
        .collect();
    f.render_widget(selectable(items, s.sel, list_area.height), list_area);
}

fn draw_repos(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = &app.repos else { return };
    let list_area = prompt_popup(f, area, " repositories ", " type owner/name · enter open · esc close ", &p.input);
    let cwd = app.cwd_repo.as_deref();
    let items = p
        .hits
        .iter()
        .map(|r| {
            let mut spans = vec![Span::styled("▦ ", Style::default().fg(Color::Magenta)), Span::raw(r.clone())];
            if Some(r.as_str()) == cwd {
                spans.push(Span::styled("  (current directory)", Style::default().fg(Color::DarkGray)));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(selectable(items, p.sel, list_area.height), list_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_window_keeps_cursor_visible() {
        assert_eq!(input_window("hello", 2, 20), ("he".into(), "l".into(), "lo".into(), false, false));
        let (b, u, a, l, r) = input_window("0123456789", 10, 5);
        assert_eq!((b.as_str(), u.as_str(), a.as_str(), l, r), ("6789", " ", "", true, false));
        let (b, u, a, l, r) = input_window("0123456789", 0, 5);
        assert_eq!((b.as_str(), u.as_str(), a.as_str(), l, r), ("", "0", "1234", false, true));
        let (b, u, _, l, r) = input_window("0123456789", 7, 5);
        assert_eq!((b.as_str(), u.as_str(), l, r), ("3456", "7", true, true));
        assert_eq!(input_window("", 0, 5), ("".into(), " ".into(), "".into(), false, false));
        let (b, u, _, _, _) = input_window("héllö wörld", 6, 4);
        assert_eq!((b.as_str(), u.as_str()), ("lö ", "w"));
    }

    #[test]
    fn highlight_splits_spans() {
        let mut line = Line::from(vec![
            Span::styled("Hello Qd".to_string(), Style::default().fg(Color::Blue)),
            Span::raw("rant world qdrant".to_string()),
        ]);
        let q: Vec<char> = "qdrant".chars().collect();
        let hl = Style::default().bg(Color::Yellow);
        highlight_find(&mut line, &q, hl);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Hello Qdrant world qdrant");
        let marked: String = line
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Yellow))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, "Qdrantqdrant");
        assert!(line.spans.iter().any(|s| s.content == "Hello " && s.style.fg == Some(Color::Blue)));
        let mut line2 = Line::from("nothing here".to_string());
        highlight_find(&mut line2, &"zzz".chars().collect::<Vec<_>>(), hl);
        assert_eq!(line2.spans.len(), 1);
    }

    #[test]
    fn wrap_cell_basics() {
        assert_eq!(wrap_cell("hello world", 20, 8), vec!["hello world"]);
        assert_eq!(wrap_cell("hello world", 6, 8), vec!["hello", "world"]);
        assert_eq!(wrap_cell("", 10, 8), vec![""]);
        assert_eq!(wrap_cell("abcdefghij", 4, 8), vec!["abcd", "efgh", "ij"]);
        let out = wrap_cell("one two three four five six", 4, 2);
        assert_eq!(out.len(), 2);
        assert!(out[1].ends_with('…'));
        for l in wrap_cell("some quite long sentence with several words in it", 7, 8) {
            assert!(l.chars().count() <= 7, "{l:?}");
        }
    }
}
