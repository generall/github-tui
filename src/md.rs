//! Markdown → styled ratatui lines, with link extraction for navigation.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// issue or PR; `repo` None = the repo of the page it appears on
    Item { repo: Option<String>, number: u64 },
    External(String),
}

#[derive(Clone)]
pub struct Link {
    pub line: usize,
    pub spans: (usize, usize), // [lo, hi) span indices within the line
    pub target: Target,
}

pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<Link>,
}

/// github.com/{owner}/{repo}/(issues|pull)/{n} opens in-app; anything else externally.
pub fn target_from_url(url: &str) -> Target {
    if let Some(rest) = url.split("github.com/").nth(1) {
        let path = rest.split(['?', '#']).next().unwrap_or("");
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 4
            && matches!(parts[2], "issues" | "pull")
            && let Ok(number) = parts[3].parse::<u64>()
        {
            return Target::Item { repo: Some(format!("{}/{}", parts[0], parts[1])), number };
        }
    }
    Target::External(url.into())
}

/// Split plain text into runs, auto-linking bare `#123` references and
/// http(s) URLs the way GitHub renders them.
pub fn autolink(text: &str) -> Vec<(String, Option<Target>)> {
    let mut out: Vec<(String, Option<Target>)> = Vec::new();
    let mut plain = String::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let at_boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if at_boundary && b[i] == b'#' {
            let j = i + 1 + b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if j > i + 1
                && (j == b.len() || !b[j].is_ascii_alphanumeric())
                && let Ok(number) = text[i + 1..j].parse::<u64>()
            {
                if !plain.is_empty() {
                    out.push((std::mem::take(&mut plain), None));
                }
                out.push((text[i..j].into(), Some(Target::Item { repo: None, number })));
                i = j;
                continue;
            }
        }
        if at_boundary && (text[i..].starts_with("http://") || text[i..].starts_with("https://")) {
            let end = text[i..]
                .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | ')' | '"' | '\'' | '`'))
                .map_or(text.len(), |e| i + e);
            let mut j = end;
            while j > i && matches!(b[j - 1], b'.' | b',' | b';' | b':' | b'!' | b'?') {
                j -= 1;
            }
            if !plain.is_empty() {
                out.push((std::mem::take(&mut plain), None));
            }
            out.push((text[i..j].into(), Some(target_from_url(&text[i..j]))));
            i = j;
            continue;
        }
        let ch = text[i..].chars().next().expect("in bounds");
        plain.push(ch);
        i += ch.len_utf8();
    }
    if !plain.is_empty() {
        out.push((plain, None));
    }
    out
}

fn syntect_assets() -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let ss = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_defaults().themes["base16-ocean.dark"].clone();
        (ss, theme)
    })
}

#[derive(Default)]
struct Ctx {
    bold: bool,
    italic: bool,
    strike: bool,
    heading: u8,
    quote: usize,
    link: bool,
}

impl Ctx {
    fn style(&self) -> Style {
        let mut s = Style::default();
        match self.heading {
            1 => s = s.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            2 => s = s.fg(Color::LightBlue).add_modifier(Modifier::BOLD),
            n if n >= 3 => s = s.add_modifier(Modifier::BOLD),
            _ => {}
        }
        if self.bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.quote > 0 {
            s = s.fg(Color::Gray).add_modifier(Modifier::ITALIC);
        }
        if self.link {
            s = s.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
        }
        s
    }
}

pub fn render(md: &str) -> Rendered {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(md, opts);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut ctx = Ctx::default();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut pending_prefix: Option<String> = None;
    // links open/closed within the current line
    let mut open_link: Option<(usize, Target)> = None;
    let mut line_links: Vec<(usize, usize, Target)> = Vec::new();
    // code block buffering
    let mut code: Option<(String, String)> = None; // (lang, text)
    // table buffering
    let mut table: Option<Vec<Vec<String>>> = None;

    macro_rules! flush {
        () => {{
            if let Some(p) = pending_prefix.take() {
                spans.insert(0, Span::styled(p, Style::default().fg(Color::DarkGray)));
                for l in &mut line_links {
                    l.0 += 1;
                    l.1 += 1;
                }
            }
            if ctx.quote > 0 && !spans.is_empty() {
                spans.insert(0, Span::styled("▌ ".repeat(ctx.quote), Style::default().fg(Color::DarkGray)));
                for l in &mut line_links {
                    l.0 += 1;
                    l.1 += 1;
                }
            }
            if !spans.is_empty() {
                for (lo, hi, target) in line_links.drain(..) {
                    links.push(Link { line: lines.len(), spans: (lo, hi), target });
                }
                lines.push(Line::from(std::mem::take(&mut spans)));
            } else {
                line_links.clear();
            }
        }};
    }
    macro_rules! blank {
        () => {
            if !lines.is_empty() && !lines.last().map_or(true, |l| l.spans.is_empty()) {
                lines.push(Line::default());
            }
        };
    }

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush!();
                    blank!();
                    ctx.heading = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        _ => 3,
                    };
                }
                Tag::Paragraph => {
                    flush!();
                }
                Tag::BlockQuote(_) => {
                    flush!();
                    ctx.quote += 1;
                }
                Tag::List(start) => {
                    flush!();
                    list_stack.push(start);
                }
                Tag::Item => {
                    flush!();
                    let depth = list_stack.len().saturating_sub(1);
                    let marker = match list_stack.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => "• ".into(),
                    };
                    pending_prefix = Some(format!("{}{marker}", "  ".repeat(depth)));
                }
                Tag::CodeBlock(kind) => {
                    flush!();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code = Some((lang, String::new()));
                }
                Tag::Emphasis => ctx.italic = true,
                Tag::Strong => ctx.bold = true,
                Tag::Strikethrough => ctx.strike = true,
                Tag::Link { dest_url, .. } => {
                    ctx.link = true;
                    if table.is_none() {
                        open_link = Some((spans.len(), target_from_url(&dest_url)));
                    }
                }
                Tag::Image { dest_url, .. } => {
                    ctx.link = true;
                    if table.is_none() {
                        open_link = Some((spans.len(), Target::External(dest_url.to_string())));
                        spans.push(Span::styled("🖼 ", Style::default().fg(Color::DarkGray)));
                    }
                }
                Tag::Table(_) => {
                    flush!();
                    blank!();
                    table = Some(vec![Vec::new()]);
                }
                Tag::TableRow | Tag::TableHead => {
                    if let Some(t) = &mut table
                        && !t.last().is_none_or(|r| r.is_empty())
                    {
                        t.push(Vec::new());
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = &mut table
                        && let Some(r) = t.last_mut()
                    {
                        r.push(String::new());
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    flush!();
                    ctx.heading = 0;
                }
                TagEnd::Paragraph => {
                    flush!();
                    if list_stack.is_empty() {
                        blank!();
                    }
                }
                TagEnd::BlockQuote(_) => {
                    flush!();
                    ctx.quote = ctx.quote.saturating_sub(1);
                    if ctx.quote == 0 {
                        blank!();
                    }
                }
                TagEnd::List(_) => {
                    flush!();
                    list_stack.pop();
                    if list_stack.is_empty() {
                        blank!();
                    }
                }
                TagEnd::Item => flush!(),
                TagEnd::CodeBlock => {
                    if let Some((lang, text)) = code.take() {
                        lines.extend(highlight_code(&lang, &text));
                        blank!();
                    }
                }
                TagEnd::Emphasis => ctx.italic = false,
                TagEnd::Strong => ctx.bold = false,
                TagEnd::Strikethrough => ctx.strike = false,
                TagEnd::Link | TagEnd::Image => {
                    ctx.link = false;
                    if let Some((lo, target)) = open_link.take() {
                        line_links.push((lo, spans.len(), target));
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = table.take() {
                        lines.extend(render_table(t));
                        blank!();
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some((_, buf)) = &mut code {
                    buf.push_str(&t);
                } else if let Some(tb) = &mut table {
                    if let Some(cell) = tb.last_mut().and_then(|r| r.last_mut()) {
                        cell.push_str(&t);
                    }
                } else if open_link.is_some() {
                    spans.push(Span::styled(t.into_string(), ctx.style()));
                } else {
                    for (txt, target) in autolink(&t) {
                        match target {
                            Some(target) => {
                                let lo = spans.len();
                                spans.push(Span::styled(
                                    txt,
                                    ctx.style().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
                                ));
                                line_links.push((lo, lo + 1, target));
                            }
                            None => spans.push(Span::styled(txt, ctx.style())),
                        }
                    }
                }
            }
            Event::Code(t) => {
                if let Some(tb) = &mut table {
                    if let Some(cell) = tb.last_mut().and_then(|r| r.last_mut()) {
                        cell.push_str(&t);
                    }
                } else {
                    spans.push(Span::styled(t.into_string(), Style::default().fg(Color::Yellow)));
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                let txt = t.trim().to_string();
                if !txt.is_empty() && !txt.starts_with("<!--") && table.is_none() && code.is_none() {
                    spans.push(Span::styled(txt, Style::default().fg(Color::DarkGray)));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if open_link.is_none() {
                    flush!();
                } else {
                    spans.push(Span::raw(" "));
                }
            }
            Event::Rule => {
                flush!();
                lines.push(Line::from("─".repeat(50)).dark_gray());
                blank!();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "☑ " } else { "☐ " };
                let style = if done { Style::default().fg(Color::Green) } else { Style::default() };
                spans.push(Span::styled(mark.to_string(), style));
            }
            Event::FootnoteReference(t) => {
                spans.push(Span::styled(format!("[{t}]"), Style::default().fg(Color::DarkGray)));
            }
            _ => {}
        }
    }
    flush!();
    Rendered { lines, links }
}

fn highlight_code(lang: &str, text: &str) -> Vec<Line<'static>> {
    let (ss, theme) = syntect_assets();
    let syntax = ss
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut spans = vec![Span::styled("  ", Style::default())];
        match hl.highlight_line(&format!("{line}\n"), ss) {
            Ok(ranges) => {
                for (style, chunk) in ranges {
                    let fg = style.foreground;
                    spans.push(Span::styled(
                        chunk.trim_end_matches('\n').to_string(),
                        Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b)),
                    ));
                }
            }
            Err(_) => spans.push(Span::raw(line.to_string())),
        }
        out.push(Line::from(spans));
    }
    out
}

fn render_table(rows: Vec<Vec<String>>) -> Vec<Line<'static>> {
    let rows: Vec<Vec<String>> = rows.into_iter().filter(|r| !r.is_empty()).collect();
    if rows.is_empty() {
        return Vec::new();
    }
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; ncols];
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count()).min(40);
        }
    }
    let mut out = Vec::new();
    for (ri, r) in rows.iter().enumerate() {
        let mut spans = Vec::new();
        for (i, w) in widths.iter().enumerate() {
            let cell = r.get(i).map(String::as_str).unwrap_or("");
            let cell: String = cell.chars().take(40).collect();
            let style = if ri == 0 {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(format!("{cell:<w$}"), style));
            if i + 1 < ncols {
                spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            }
        }
        out.push(Line::from(spans));
        if ri == 0 {
            let total: usize = widths.iter().sum::<usize>() + 3 * (ncols.saturating_sub(1));
            out.push(Line::from("─".repeat(total)).dark_gray());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(repo: Option<&str>, number: u64) -> Target {
        Target::Item { repo: repo.map(String::from), number }
    }

    #[test]
    fn url_targets() {
        assert_eq!(target_from_url("https://github.com/qdrant/qdrant/pull/123/files"), item(Some("qdrant/qdrant"), 123));
        assert_eq!(target_from_url("https://github.com/qdrant/qdrant/issues/5#issuecomment-1"), item(Some("qdrant/qdrant"), 5));
        assert_eq!(target_from_url("https://github.com/qdrant/qdrant"), Target::External("https://github.com/qdrant/qdrant".into()));
        assert_eq!(target_from_url("https://example.com/x"), Target::External("https://example.com/x".into()));
    }

    #[test]
    fn autolinks() {
        let runs = autolink("fixes #12, see https://example.com/a. and #x #3a color#4");
        let links: Vec<(&str, &Target)> = runs.iter().filter_map(|(t, l)| l.as_ref().map(|l| (t.as_str(), l))).collect();
        assert_eq!(links.len(), 2, "{runs:?}");
        assert_eq!(links[0], ("#12", &item(None, 12)));
        assert_eq!(links[1], ("https://example.com/a", &Target::External("https://example.com/a".into())));
        let text: String = runs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(text, "fixes #12, see https://example.com/a. and #x #3a color#4");
        assert_eq!(autolink("héllo wörld").len(), 1);
    }

    #[test]
    fn render_links_and_code() {
        let md = "# T\n\nSee [ext](https://example.com) and #42 and https://github.com/o/r/pull/7.\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n<!-- hidden -->\n- [x] done\n";
        let r = render(md);
        let targets: Vec<&Target> = r.links.iter().map(|l| &l.target).collect();
        assert!(targets.contains(&&Target::External("https://example.com".into())));
        assert!(targets.contains(&&item(None, 42)));
        assert!(targets.contains(&&item(Some("o/r"), 7)));
        for l in &r.links {
            let line = &r.lines[l.line];
            assert!(l.spans.1 <= line.spans.len() && l.spans.0 < l.spans.1);
        }
        let text: String = r.lines.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.as_ref()).collect();
        assert!(text.contains("fn main"));
        assert!(text.contains("1"));
        assert!(!text.contains("hidden"));
        assert!(text.contains("☑"));
    }
}
