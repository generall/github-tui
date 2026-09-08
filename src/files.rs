//! Compact, directory-grouped PR file statistics.

use crate::backend::{ChangedFile, FileChanges};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::collections::BTreeMap;

#[derive(Default)]
struct Directory {
    dirs: BTreeMap<String, Directory>,
    files: BTreeMap<String, ChangedFile>,
}

struct TreeRow {
    path: String,
    file: Option<ChangedFile>,
}

pub struct FileTree {
    rows: Vec<TreeRow>,
    total: u64,
    additions: u64,
    deletions: u64,
    more: u64,
}

impl FileTree {
    pub fn new(changes: &FileChanges) -> Self {
        let mut root = Directory::default();
        for file in &changes.files {
            let mut dir = &mut root;
            let mut parts = file.path.split('/').peekable();
            while let Some(part) = parts.next() {
                if parts.peek().is_none() {
                    dir.files.insert(part.into(), file.clone());
                } else {
                    dir = dir.dirs.entry(part.into()).or_default();
                }
            }
        }
        let mut rows = Vec::new();
        collect_rows(root, 0, &mut rows);
        Self {
            rows,
            total: changes.total,
            additions: changes.additions,
            deletions: changes.deletions,
            more: changes.total.saturating_sub(changes.files.len() as u64),
        }
    }

    /// Row count stays constant across widths, preserving search and link offsets.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let green = Style::default().fg(Color::Green);
        let red = Style::default().fg(Color::Red);
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = vec![
            fit_line(
                Line::from(vec![
                    Span::styled(
                        format!("Files changed: {}   ", self.total),
                        Style::default().bold(),
                    ),
                    Span::styled(format!("+{} ", self.additions), green),
                    Span::styled(format!("−{}", self.deletions), red),
                ]),
                width,
            ),
            Line::default(),
        ];
        let digits = |added: bool| {
            self.rows
                .iter()
                .filter_map(|row| row.file.as_ref())
                .map(|file| {
                    if added {
                        file.additions
                    } else {
                        file.deletions
                    }
                    .to_string()
                    .len()
                })
                .max()
                .unwrap_or(1)
        };
        let add_width = digits(true) + 1;
        let del_width = digits(false) + 1;
        // Two spaces before type, and two before each signed number.
        let stats_width = 7 + add_width + del_width;
        let path_width = self
            .rows
            .iter()
            .filter(|row| row.file.is_some())
            .map(|row| Span::raw(&row.path).width())
            .max()
            .unwrap_or(0)
            .min(width.saturating_sub(stats_width));

        for row in &self.rows {
            let Some(file) = &row.file else {
                lines.push(Line::from(Span::styled(shorten(&row.path, width), dim)));
                continue;
            };
            let (mark, color) = match file.change_type.as_str() {
                "ADDED" => ("A", Color::Green),
                "DELETED" => ("D", Color::Red),
                "MODIFIED" => ("M", Color::Yellow),
                "RENAMED" => ("R", Color::Cyan),
                "COPIED" => ("C", Color::Cyan),
                "CHANGED" => ("T", Color::Yellow),
                _ => ("?", Color::DarkGray),
            };
            let path = shorten(&row.path, path_width);
            let padding = path_width.saturating_sub(Span::raw(&path).width());
            lines.push(fit_line(
                Line::from(vec![
                    Span::raw(path),
                    Span::raw(" ".repeat(padding + 2)),
                    Span::styled(mark, Style::default().fg(color)),
                    Span::styled(
                        format!("  {:>add_width$}", format!("+{}", file.additions)),
                        green,
                    ),
                    Span::styled(
                        format!("  {:>del_width$}", format!("−{}", file.deletions)),
                        red,
                    ),
                ]),
                width,
            ));
        }
        if self.more > 0 {
            lines.push(Line::from(Span::styled(
                shorten(&format!("… {} more files", self.more), width),
                dim,
            )));
        } else if self.total == 0 {
            lines.push(Line::from(Span::styled(
                shorten("(no changed files)", width),
                dim,
            )));
        }
        lines
    }
}

fn collect_rows(dir: Directory, depth: usize, rows: &mut Vec<TreeRow>) {
    let indent = "  ".repeat(depth);
    for (mut path, mut child) in dir.dirs {
        while child.files.is_empty() && child.dirs.len() == 1 {
            let (name, next) = child.dirs.pop_first().unwrap();
            path.push('/');
            path.push_str(&name);
            child = next;
        }
        if child.dirs.is_empty() && child.files.len() == 1 {
            let (name, file) = child.files.pop_first().unwrap();
            rows.push(TreeRow {
                path: clean_path(format!("{indent}{path}/{name}")),
                file: Some(file),
            });
        } else {
            rows.push(TreeRow {
                path: clean_path(format!("{indent}{path}/")),
                file: None,
            });
            collect_rows(child, depth + 1, rows);
        }
    }
    for (name, file) in dir.files {
        rows.push(TreeRow {
            path: clean_path(format!("{indent}{name}")),
            file: Some(file),
        });
    }
}

fn clean_path(path: String) -> String {
    // Filenames can contain newlines and tabs; every file must stay on one row.
    path.chars()
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}

/// Clip by terminal cells, keeping combining marks and wide Unicode intact.
fn shorten(text: &str, width: usize) -> String {
    let span = Span::raw(text);
    if span.width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in span.styled_graphemes(Style::default()) {
        let cells = Span::raw(grapheme.symbol).width();
        if used + cells > width - 1 {
            break;
        }
        out.push_str(grapheme.symbol);
        used += cells;
    }
    out.push('…');
    out
}

fn fit_line(mut line: Line<'static>, width: usize) -> Line<'static> {
    let mut left = width;
    for span in &mut line.spans {
        let content = shorten(&span.content, left);
        left = left.saturating_sub(Span::raw(&content).width());
        span.content = content.into();
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        widgets::{Paragraph, Wrap},
    };

    fn file(path: &str, change_type: &str, additions: u64, deletions: u64) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            change_type: change_type.into(),
            additions,
            deletions,
        }
    }

    #[test]
    fn directory_tree_compacts_single_children_and_sorts_paths() {
        let changes = FileChanges {
            total: 5,
            additions: 80,
            deletions: 143,
            files: vec![
                file("Cargo.lock", "MODIFIED", 9, 44),
                file(
                    "lib/common/common/src/universal_io/io_uring/mod.rs",
                    "MODIFIED",
                    5,
                    30,
                ),
                file(
                    "lib/common/common/src/universal_io/cached_fs/mod.rs",
                    "MODIFIED",
                    8,
                    48,
                ),
                file(
                    "lib/common/common/src/universal_io/disk_cache/mod.rs",
                    "MODIFIED",
                    1,
                    21,
                ),
                file(
                    "lib/common/common/src/universal_io/cached_fs/async_io.rs",
                    "ADDED",
                    57,
                    0,
                ),
            ],
        };
        let tree = FileTree::new(&changes);
        let paths: Vec<_> = tree.rows.iter().map(|row| row.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "lib/common/common/src/universal_io/",
                "  cached_fs/",
                "    async_io.rs",
                "    mod.rs",
                "  disk_cache/mod.rs",
                "  io_uring/mod.rs",
                "Cargo.lock",
            ]
        );
        let lines = tree.lines(80);
        assert_eq!(lines[0].to_string(), "Files changed: 5   +80 −143");
        assert_eq!(lines[4].to_string(), "    async_io.rs      A  +57   −0");
        assert_eq!(lines[5].to_string(), "    mod.rs           M   +8  −48");
        assert_eq!(
            tree.rows.iter().filter(|row| row.file.is_some()).count(),
            changes.files.len()
        );
    }

    #[test]
    fn file_counts_align_and_colors_survive_narrow_unicode_rendering() {
        let files = vec![
            file("新しいファイル.rs", "ADDED", 1000, 0),
            file("e\u{301}.rs", "MODIFIED", 8, 12),
            file("gone.rs", "DELETED", 0, 120),
            file("renamed.rs", "RENAMED", 0, 0),
            file("copy.rs", "COPIED", 0, 0),
            file("kind.rs", "CHANGED", 0, 0),
            file("unknown.rs", "FUTURE_VALUE", 0, 0),
            file("newline\nand\ttab.rs", "MODIFIED", 1, 1),
        ];
        let tree = FileTree::new(&FileChanges {
            total: 8,
            additions: 1009,
            deletions: 133,
            files,
        });
        for width in [80, 40, 24, 10, 1, 0] {
            let lines = tree.lines(width);
            assert_eq!(lines.len(), 10);
            assert!(lines.iter().all(|line| line.width() <= width));
            assert!(
                lines
                    .iter()
                    .all(|line| !line.to_string().contains(['\n', '\t']))
            );
            if width < 24 {
                continue;
            }
            let mut terminal = Terminal::new(TestBackend::new(width as u16, 12)).unwrap();
            terminal
                .draw(|f| {
                    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), f.area())
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut ends = Vec::new();
            for y in 2..10 {
                let mut added_end = None;
                let mut deleted_end = None;
                for x in 0..width as u16 {
                    let cell = &buffer[(x, y)];
                    if cell.symbol().chars().all(|c| c.is_ascii_digit()) {
                        match cell.fg {
                            Color::Green => added_end = Some(x),
                            Color::Red => deleted_end = Some(x),
                            _ => {}
                        }
                    }
                }
                assert!(added_end.is_some() && deleted_end.is_some());
                ends.push((added_end, deleted_end));
            }
            assert!(ends.windows(2).all(|pair| pair[0] == pair[1]));
        }
        let lines = tree.lines(80);
        for (path, mark, color) in [
            ("新しい", "A", Color::Green),
            ("e\u{301}.rs", "M", Color::Yellow),
            ("gone.rs", "D", Color::Red),
            ("renamed.rs", "R", Color::Cyan),
            ("copy.rs", "C", Color::Cyan),
            ("kind.rs", "T", Color::Yellow),
            ("unknown.rs", "?", Color::DarkGray),
        ] {
            let line = lines
                .iter()
                .find(|line| line.to_string().starts_with(path))
                .unwrap();
            assert!(
                line.spans
                    .iter()
                    .any(|span| span.content == mark && span.style.fg == Some(color))
            );
        }
    }

    #[test]
    fn partial_and_empty_trees_keep_whole_pr_totals() {
        let files: Vec<_> = (0..100)
            .map(|i| file(&format!("file{i:03}.rs"), "MODIFIED", 1, 0))
            .collect();
        let tree = FileTree::new(&FileChanges {
            total: 142,
            additions: 547,
            deletions: 937,
            files,
        });
        let lines = tree.lines(80);
        assert_eq!(lines[0].to_string(), "Files changed: 142   +547 −937");
        assert_eq!(lines.last().unwrap().to_string(), "… 42 more files");
        assert_eq!(lines.len(), 103);
        let empty = FileTree::new(&FileChanges {
            total: 0,
            additions: 0,
            deletions: 0,
            files: vec![],
        });
        assert_eq!(
            empty.lines(80).last().unwrap().to_string(),
            "(no changed files)"
        );
    }
}
