//! App state: pane stack, search/repo overlays, editor session, key/msg handling.

use crate::backend::{
    apply_field, edit_cmds, editable_fields, epoch, frecency, list_key, Backend, Cache, ItemDoc,
    ListDoc, Msg, ReviewKind, SearchHit, COLUMNS, PR_COLUMNS,
};
use crate::md::{self, Target};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use edtui::{EditorEventHandler, EditorState, Lines};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::TableState;
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::Instant;

/// (label, REST collection, state filter) — the `v` picker entries.
pub const VIEWS: &[(&str, &str, &str)] = &[
    ("Open issues", "issues", "open"),
    ("Open PRs", "pulls", "open"),
    ("Closed issues", "issues", "closed"),
    ("Closed & merged PRs", "pulls", "closed"),
    ("All issues", "issues", "all"),
    ("All PRs", "pulls", "all"),
];

pub struct ItemView {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub state: String,
    pub is_pr: bool,
    pub lines: Vec<Line<'static>>,
    pub links: Vec<md::Link>,
    pub scroll: usize,
    pub link_sel: Option<usize>,
    pub loaded: bool,
    pub viewport: u16, // set during draw, used for paging keys
    pub find: String,
    pub matches: Vec<usize>, // line indices containing the find query
    pub msel: usize,
    pub show_long_bot_comments: bool,
    comment_lines: Vec<usize>, // comment header positions, used to preserve scroll when toggling
}

impl ItemView {
    fn new(repo: String, number: u64, title: String) -> Self {
        Self {
            repo,
            number,
            title,
            state: String::new(),
            is_pr: false,
            lines: vec![Line::from("  loading…")],
            links: Vec::new(),
            scroll: 0,
            link_sel: None,
            loaded: false,
            viewport: 20,
            find: String::new(),
            matches: Vec::new(),
            msel: 0,
            show_long_bot_comments: false,
            comment_lines: Vec::new(),
        }
    }

    pub fn key(&self) -> String {
        ItemDoc::key(&self.repo, self.number)
    }

    fn recompute_matches(&mut self) {
        self.matches = if self.find.is_empty() {
            Vec::new()
        } else {
            let q = self.find.to_lowercase();
            self.lines
                .iter()
                .enumerate()
                .filter(|(_, l)| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .to_lowercase()
                        .contains(&q)
                })
                .map(|(i, _)| i)
                .collect()
        };
        self.msel = self.msel.min(self.matches.len().saturating_sub(1));
    }

    fn goto_match(&mut self) {
        if let Some(&line) = self.matches.get(self.msel) {
            let vp = self.viewport as usize;
            if line < self.scroll || line >= self.scroll + vp {
                self.scroll = line.saturating_sub(vp / 3);
            }
        }
    }

    fn set_doc(&mut self, doc: &ItemDoc) {
        self.title = doc.title.clone();
        self.state = doc.state.clone();
        self.is_pr = doc.is_pr;
        let mut lines: Vec<Line<'static>> = doc
            .props()
            .iter()
            .map(|(k, v)| {
                let vs = match k.as_str() {
                    "state" => state_style(v),
                    "review" => review_style(v),
                    _ => Style::default().fg(Color::Gray),
                };
                Line::from(vec![
                    Span::styled(format!("{k}: "), Style::default().fg(Color::DarkGray)),
                    Span::styled(v.clone(), vs),
                ])
            })
            .collect();
        lines.push(Line::default());
        let mut links = Vec::new();
        let mut append = |md_text: &str, lines: &mut Vec<Line<'static>>| {
            let rendered = md::render(md_text);
            let offset = lines.len();
            lines.extend(rendered.lines);
            links.extend(rendered.links.into_iter().map(|mut l| {
                l.line += offset;
                l
            }));
        };
        append(&doc.body, &mut lines);
        self.comment_lines.clear();
        for c in &doc.comments {
            lines.push(Line::default());
            self.comment_lines.push(lines.len());
            let kind = if c.kind.is_empty() { String::new() } else { format!(" · {}", c.kind) };
            lines.push(Line::from(vec![
                Span::styled("── ", Style::default().fg(Color::DarkGray)),
                Span::styled(c.author.clone(), Style::default().fg(Color::Magenta)),
                Span::styled(format!(" · {}{kind} ", c.date), Style::default().fg(Color::DarkGray)),
                Span::styled("─".repeat(30), Style::default().fg(Color::DarkGray)),
            ]));
            // GraphQL identifies bots even when their login lacks the [bot] suffix.
            // The suffix also supports comments loaded from older disk caches.
            let long_bot = (c.is_bot || c.author.ends_with("[bot]"))
                && (c.body.trim().lines().count() > 10 || c.body.chars().count() > 1000);
            if long_bot && !self.show_long_bot_comments {
                lines.push(Line::from(Span::styled(
                    "  [long bot comment hidden · x to expand]",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                append(&c.body, &mut lines);
            }
        }
        self.lines = lines;
        self.links = links;
        self.loaded = true;
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(1));
        if self.link_sel.is_some_and(|i| i >= self.links.len()) {
            self.link_sel = None;
        }
        self.recompute_matches();
    }

    fn toggle_bot_comments(&mut self, doc: &ItemDoc) {
        let anchor = self.comment_lines.iter().enumerate().rev()
            .find(|&(_, &line)| line <= self.scroll)
            .map(|(i, &line)| (i, self.scroll - line));
        self.show_long_bot_comments = !self.show_long_bot_comments;
        self.link_sel = None; // hidden links must not leave a different link selected
        self.set_doc(doc);
        if let Some((i, offset)) = anchor {
            let end = self.comment_lines.get(i + 1).copied().unwrap_or(self.lines.len());
            self.scroll = (self.comment_lines[i] + offset).min(end.saturating_sub(1));
        }
    }
}

pub fn state_style(state: &str) -> Style {
    let s = Style::default();
    match state.split_whitespace().next().unwrap_or("") {
        "open" => s.fg(Color::Green),
        "closed" => s.fg(Color::Red),
        "merged" => s.fg(Color::Magenta),
        "draft" => s.fg(Color::DarkGray),
        _ => s.fg(Color::Gray),
    }
}

pub fn review_style(status: &str) -> Style {
    Style::default().fg(match status {
        "approved" => Color::Green,
        "changes requested" => Color::Red,
        "needs review" => Color::Yellow,
        _ => Color::DarkGray,
    })
}

pub struct ListView {
    pub repo: String,
    pub view: usize, // index into VIEWS
    pub doc: ListDoc,
    pub state: TableState,
    pub col_offset: usize,
    pub loaded: bool,
    pub picker: Option<usize>, // Some(sel) = view picker open
    pub find: String,
    pub expanded: bool, // wrap cell text over multiple lines instead of truncating
}

impl ListView {
    fn new(repo: String) -> Self {
        Self {
            repo,
            view: 0,
            doc: ListDoc::default(),
            state: TableState::default(),
            col_offset: 0,
            loaded: false,
            picker: None,
            find: String::new(),
            expanded: false,
        }
    }

    pub fn label(&self) -> &'static str {
        VIEWS[self.view].0
    }

    pub fn columns(&self) -> &'static [&'static str] {
        if VIEWS[self.view].1 == "pulls" { PR_COLUMNS } else { COLUMNS }
    }

    pub fn key(&self) -> String {
        let (_, kind, state) = VIEWS[self.view];
        list_key(&self.repo, kind, state)
    }

    /// Indices of rows matching the row filter (all rows when no filter).
    /// ponytail: recomputed per keypress/draw over loaded rows; index it if lists get huge.
    pub fn visible(&self) -> Vec<usize> {
        if self.find.is_empty() {
            return (0..self.doc.rows.len()).collect();
        }
        let q = self.find.to_lowercase();
        self.doc
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.cells.iter().any(|c| c.to_lowercase().contains(&q))
                || r.review_status.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn set_doc(&mut self, doc: ListDoc, append: bool) {
        if append {
            self.doc.rows.extend(doc.rows);
            self.doc.has_more = doc.has_more;
            self.doc.page = doc.page;
        } else {
            self.doc = doc;
            if self.state.selected().is_none() && !self.doc.rows.is_empty() {
                self.state.select(Some(0));
            }
        }
        self.loaded = true;
    }
}

#[allow(clippy::large_enum_variant)] // a handful of panes live on the stack
pub enum Pane {
    Item(ItemView),
    List(ListView),
}

impl Pane {
    pub fn title(&self) -> String {
        match self {
            Pane::Item(p) => format!("#{} {}", p.number, p.title),
            Pane::List(t) => format!("{} · {}", t.repo, t.label()),
        }
    }

    pub fn repo(&self) -> &str {
        match self {
            Pane::Item(p) => &p.repo,
            Pane::List(t) => &t.repo,
        }
    }
}

pub struct SearchState {
    pub repo: String,
    pub input: String,
    /// every hit we know about for this repo, newest first
    pub pool: Vec<SearchHit>,
    /// what the list shows: instant local ranking of the pool; server hits for
    /// the current input that match only by body are appended after
    pub hits: Vec<SearchHit>,
    pub server_hits: Vec<SearchHit>,
    /// precomputed frecency score per id (scaled to integer for sorting)
    pub frec: HashMap<String, i64>,
    pub sel: usize,
    pub dirty_at: Option<Instant>,
    pub fired: String,
}

impl SearchState {
    fn refilter(&mut self) {
        let mut hits: Vec<SearchHit> =
            rank(&self.pool, &self.input, &self.frec, |h| (h.id(), h.title.clone())).into_iter().cloned().collect();
        // GitHub matches bodies too; keep those below the title matches
        for h in &self.server_hits {
            if !hits.iter().any(|x| x.id() == h.id()) {
                hits.push(h.clone());
            }
        }
        self.hits = hits;
        self.sel = 0;
    }

    fn merge_pool(&mut self, fresh: &[SearchHit]) {
        self.pool.retain(|h| !fresh.iter().any(|n| n.id() == h.id()));
        let mut pool = fresh.to_vec();
        pool.append(&mut self.pool);
        self.pool = pool;
    }
}

/// `R` overlay: pick a known repo, or type any `owner/name`.
pub struct RepoPicker {
    pub input: String,
    pub hits: Vec<String>,
    pub sel: usize,
}

/// Match tier of one query word against a title: starts-a-word (0) beats
/// mid-word substring (1) beats fuzzy subsequence (2). Frecency decides within a tier.
fn word_tier(title: &str, word: &str) -> Option<u8> {
    match title.find(word) {
        Some(0) => Some(0),
        Some(pos) => {
            let before = title[..pos].chars().next_back().unwrap_or(' ');
            Some(if before.is_alphanumeric() { 1 } else { 0 })
        }
        None => {
            let mut it = title.chars();
            if word.chars().all(|qc| it.any(|tc| tc == qc)) { Some(2) } else { None }
        }
    }
}

/// Instant client-side ranking. Every query word must match (its own tier);
/// items order by worst word tier, then frecency (how often/recently you open
/// the item), then title length (denser match), then pool order.
fn rank<'a, T>(
    pool: &'a [T],
    query: &str,
    frec: &HashMap<String, i64>,
    key: impl Fn(&T) -> (String, String),
) -> Vec<&'a T> {
    let q = query.to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();
    if words.is_empty() {
        return pool.iter().collect();
    }
    let mut scored: Vec<(u8, i64, usize, usize, &T)> = pool
        .iter()
        .enumerate()
        .filter_map(|(i, h)| {
            let (id, title) = key(h);
            let t = title.to_lowercase();
            let tier = words
                .iter()
                .map(|w| word_tier(&t, w))
                .collect::<Option<Vec<u8>>>()?
                .into_iter()
                .max()
                .unwrap_or(2);
            let f = frec.get(&id).copied().unwrap_or(0);
            Some((tier, -f, t.chars().count(), i, h))
        })
        .collect();
    scored.sort_by_key(|(tier, negf, len, i, _)| (*tier, *negf, *len, *i));
    scored.into_iter().map(|(_, _, _, _, h)| h).collect()
}

pub struct EditorSession {
    pub repo: String,
    pub number: u64,
    pub is_pr: bool,
    pub state: EditorState,
    pub handler: EditorEventHandler,
    pub saving: bool,
}

/// A pending request to edit text in the user's real $EDITOR (git-style).
/// The main loop suspends the TUI, runs the editor, and hands the result back.
pub struct ExternalEdit {
    pub template: String,
    pub req: ExternalReq,
}

pub enum ExternalReq {
    Body { repo: String, number: u64, is_pr: bool },
    Comment { repo: String, number: u64, is_pr: bool },
    Review { repo: String, number: u64, kind: ReviewKind },
    NewIssue { repo: String },
}

pub struct ReviewDialog {
    pub repo: String,
    pub number: u64,
    pub sel: usize,
    pub body: String,
    pub mode: ReviewMode,
}

#[derive(PartialEq, Eq)]
pub enum ReviewMode {
    Pick,
    Confirm,
    Submitting,
}

impl ReviewDialog {
    pub fn kind(&self) -> ReviewKind {
        ReviewKind::ALL[self.sel]
    }
}

/// Field-at-a-time metadata editor overlay (`m` on an item).
pub struct MetaEditor {
    pub repo: String,
    pub number: u64,
    pub fields: Vec<MetaField>,
    pub sel: usize,
    pub mode: MetaMode,
}

pub struct MetaField {
    pub name: &'static str,
    pub value: String,
}

pub enum MetaMode {
    List,
    Input { buf: String, pos: usize }, // pos = cursor, in chars
    Pick { options: Vec<String>, sel: usize },
    MultiPick { options: Vec<String>, chosen: Vec<bool>, sel: usize },
}

/// Byte index of char position `pos` in `s` (len when past the end).
pub fn char_byte(s: &str, pos: usize) -> usize {
    s.char_indices().nth(pos).map_or(s.len(), |(i, _)| i)
}

pub struct App {
    pub stack: Vec<Pane>,
    pub search: Option<SearchState>,
    pub repos: Option<RepoPicker>,
    pub editor: Option<EditorSession>,
    pub backend: Backend,
    pub cache: Cache,
    pub status: Option<(String, Instant)>,
    pub quit: bool,
    pub find_input: bool, // `/` prompt active in the footer
    pub external: Option<ExternalEdit>, // picked up by the main loop
    pub meta: Option<MetaEditor>,
    pub review: Option<ReviewDialog>,
    pub cwd_repo: Option<String>,
    search_seq: u64,
}

impl App {
    pub fn new(tx: Sender<Msg>, cwd_repo: Option<String>, arg: Option<String>) -> Self {
        let mut app = Self {
            stack: Vec::new(),
            search: None,
            repos: None,
            editor: None,
            backend: Backend::new(tx),
            cache: Cache::load(),
            status: None,
            quit: false,
            find_input: false,
            external: None,
            meta: None,
            review: None,
            cwd_repo,
            search_seq: 0,
        };
        app.backend.fetch_repos();
        // `owner/repo`, `owner/repo#N`, `N` / `#N` (in the cwd repo)
        let (repo, number) = match arg.as_deref() {
            Some(a) if a.contains('/') => {
                let (r, n) = a.split_once('#').unwrap_or((a, ""));
                (Some(r.to_string()), n.parse::<u64>().ok())
            }
            Some(a) => (app.cwd_repo.clone(), a.trim_start_matches('#').parse::<u64>().ok()),
            None => (None, None),
        };
        match (repo, number) {
            (Some(r), n) => {
                app.open_list(r.clone());
                match n {
                    Some(n) => app.open_item(r, n, format!("#{n}")),
                    None if arg.is_none() => app.backend.fetch_cwd_pr(r),
                    None => {}
                }
            }
            (None, Some(_)) => app.set_status("not in a GitHub checkout: pass owner/repo#N"),
            (None, None) => match app.cache.repos.first().cloned() {
                Some(r) => app.open_list(r),
                None => app.open_repos(),
            },
        }
        app
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// Repo of the top pane, else the cwd's, else the most recently used.
    pub fn cur_repo(&self) -> Option<String> {
        self.stack
            .last()
            .map(|p| p.repo().to_string())
            .or_else(|| self.cwd_repo.clone())
            .or_else(|| self.cache.repos.first().cloned())
    }

    // ---- navigation ----

    fn open_search(&mut self) {
        let Some(repo) = self.cur_repo() else {
            self.open_repos();
            return;
        };
        let pool: Vec<SearchHit> = self.cache.pool.iter().filter(|h| h.repo == repo).cloned().collect();
        let now = epoch();
        let frec: HashMap<String, i64> = self
            .cache
            .visits
            .iter()
            .map(|(id, v)| (id.clone(), (frecency(*v, now) * 1000.0) as i64))
            .collect();
        self.search = Some(SearchState {
            repo: repo.clone(),
            input: String::new(),
            hits: pool.clone(),
            pool,
            server_hits: Vec::new(),
            frec,
            sel: 0,
            dirty_at: None,
            fired: String::new(),
        });
        self.search_seq += 1;
        self.backend.search(repo, String::new(), self.search_seq);
    }

    fn open_repos(&mut self) {
        let mut p = RepoPicker { input: String::new(), hits: Vec::new(), sel: 0 };
        p.hits = self.cache.repos.clone();
        self.repos = Some(p);
    }

    fn repos_refilter(&mut self) {
        let Some(p) = &mut self.repos else { return };
        let none = HashMap::new();
        p.hits = rank(&self.cache.repos, &p.input, &none, |r| (r.clone(), r.clone()))
            .into_iter()
            .cloned()
            .collect();
        p.sel = 0;
    }

    pub fn open_list(&mut self, repo: String) {
        self.cache.use_repo(&repo);
        let mut t = ListView::new(repo);
        if let Some(doc) = self.cache.lists.get(&t.key()) {
            t.set_doc(doc.clone(), false);
        }
        self.stack.push(Pane::List(t));
        self.list_fetch(1);
    }

    /// Fetch `page` of the top list pane's current view.
    fn list_fetch(&mut self, page: u32) {
        let Some(Pane::List(t)) = self.stack.last() else { return };
        let (_, kind, state) = VIEWS[t.view];
        self.backend.fetch_list(t.repo.clone(), kind, state, page);
    }

    fn list_select_view(&mut self, choice: usize) {
        let Some(Pane::List(t)) = self.stack.last_mut() else { return };
        t.picker = None;
        t.view = choice.min(VIEWS.len() - 1);
        t.doc = ListDoc::default();
        t.loaded = false;
        t.state = TableState::default();
        t.col_offset = 0;
        if let Some(doc) = self.cache.lists.get(&t.key()) {
            t.set_doc(doc.clone(), false);
        }
        self.list_fetch(1);
    }

    pub fn open_item(&mut self, repo: String, number: u64, title_hint: String) {
        let key = ItemDoc::key(&repo, number);
        self.cache.visit(&key);
        let mut view = ItemView::new(repo.clone(), number, title_hint);
        if let Some(doc) = self.cache.items.get(&key) {
            view.set_doc(&doc.clone());
        }
        self.backend.fetch_item(repo, number);
        self.stack.push(Pane::Item(view));
    }

    fn open_target(&mut self, target: Target, title_hint: String) {
        match target {
            Target::Item { repo, number } => {
                let repo = repo.or_else(|| self.cur_repo());
                if let Some(repo) = repo {
                    self.open_item(repo, number, title_hint);
                }
            }
            Target::External(url) => self.open_external(&url),
        }
    }

    fn open_external(&mut self, url: &str) {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.set_status(format!("opened {url}"));
    }

    fn back(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        } else if self.stack.len() == 1 && self.search.is_none() {
            self.open_search();
        }
    }

    /// Refetch every list pane of `repo` (row cells may have changed).
    fn refresh_lists(&mut self, repo: &str) {
        let targets: Vec<(String, usize)> = self
            .stack
            .iter()
            .filter_map(|p| match p {
                Pane::List(t) if t.repo == repo => Some((t.repo.clone(), t.view)),
                _ => None,
            })
            .collect();
        for (repo, view) in targets {
            let (_, kind, state) = VIEWS[view];
            self.backend.fetch_list(repo, kind, state, 1);
        }
    }

    fn refresh_current(&mut self) {
        match self.stack.last() {
            Some(Pane::Item(p)) => {
                let (repo, n) = (p.repo.clone(), p.number);
                self.backend.fetch_item(repo, n);
                self.set_status("refreshing…");
            }
            Some(Pane::List(_)) => {
                self.list_fetch(1);
                self.set_status("refreshing…");
            }
            None => {}
        }
    }

    /// The cached doc of the top item pane, or a status message why not.
    fn top_doc(&mut self) -> Option<ItemDoc> {
        let Some(Pane::Item(p)) = self.stack.last() else { return None };
        let doc = self.cache.items.get(&p.key()).cloned();
        if doc.is_none() {
            self.set_status("item not loaded yet");
        }
        doc
    }

    fn start_edit(&mut self) {
        let Some(doc) = self.top_doc() else { return };
        let Some(Pane::Item(p)) = self.stack.last() else { return };
        self.editor = Some(EditorSession {
            repo: p.repo.clone(),
            number: p.number,
            is_pr: doc.is_pr,
            state: EditorState::new(Lines::from(doc.body.as_str())),
            handler: EditorEventHandler::default(),
            saving: false,
        });
    }

    fn start_external_body(&mut self) {
        let Some(doc) = self.top_doc() else { return };
        let Some(Pane::Item(p)) = self.stack.last() else { return };
        self.external = Some(ExternalEdit {
            template: doc.body.clone(),
            req: ExternalReq::Body { repo: p.repo.clone(), number: p.number, is_pr: doc.is_pr },
        });
    }

    fn start_comment(&mut self) {
        let Some(doc) = self.top_doc() else { return };
        let Some(Pane::Item(p)) = self.stack.last() else { return };
        self.external = Some(ExternalEdit {
            template: COMMENT_TEMPLATE.into(),
            req: ExternalReq::Comment { repo: p.repo.clone(), number: p.number, is_pr: doc.is_pr },
        });
    }

    fn start_review(&mut self) {
        let Some(doc) = self.top_doc() else { return };
        if !doc.is_pr {
            self.set_status("not a pull request");
            return;
        }
        let Some(Pane::Item(p)) = self.stack.last() else { return };
        self.review = Some(ReviewDialog {
            repo: p.repo.clone(), number: p.number, sel: 0,
            body: String::new(), mode: ReviewMode::Pick,
        });
    }

    fn on_review_key(&mut self, key: KeyEvent) {
        let Some(review) = &mut self.review else { return };
        if review.mode == ReviewMode::Submitting {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.review = None;
                self.set_status("review cancelled");
            }
            KeyCode::Char('j') | KeyCode::Down if review.mode == ReviewMode::Pick => {
                review.sel = (review.sel + 1).min(ReviewKind::ALL.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up if review.mode == ReviewMode::Pick => {
                review.sel = review.sel.saturating_sub(1);
            }
            KeyCode::Enter if review.mode == ReviewMode::Confirm => {
                review.mode = ReviewMode::Submitting;
                self.backend.review(review.repo.clone(), review.number, review.kind(), review.body.clone());
                self.set_status("submitting review…");
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let hint = if review.kind() == ReviewKind::Approve {
                    "A message is optional. Save and close to preview your approval."
                } else {
                    "Write your review above. An empty message cancels the review."
                };
                self.external = Some(ExternalEdit {
                    template: format!("{}\n\n<!-- {} {}#{}: {hint} -->\n",
                        review.body, review.kind().label(), review.repo, review.number),
                    req: ExternalReq::Review {
                        repo: review.repo.clone(), number: review.number, kind: review.kind(),
                    },
                });
            }
            _ => {}
        }
    }

    fn start_meta_edit(&mut self) {
        let Some(doc) = self.top_doc() else { return };
        let Some(Pane::Item(p)) = self.stack.last() else { return };
        let repo = p.repo.clone();
        if !self.cache.meta.contains_key(&repo) {
            self.backend.fetch_meta(repo.clone());
        }
        self.meta = Some(MetaEditor {
            number: p.number,
            repo,
            fields: editable_fields(&doc).into_iter().map(|(name, value)| MetaField { name, value }).collect(),
            sel: 0,
            mode: MetaMode::List,
        });
    }

    fn start_new_issue(&mut self) {
        let Some(repo) = self.cur_repo() else { return };
        self.external = Some(ExternalEdit {
            template: NEW_ISSUE_TEMPLATE.into(),
            req: ExternalReq::NewIssue { repo },
        });
    }

    fn checkout_pr(&mut self) {
        let Some(Pane::Item(p)) = self.stack.last() else { return };
        let (repo, n) = (p.repo.clone(), p.number);
        if !self.cache.items.get(&p.key()).is_some_and(|d| d.is_pr) {
            self.set_status("not a pull request");
            return;
        }
        if self.cwd_repo.as_deref() != Some(repo.as_str()) {
            self.set_status(format!("current directory is not a checkout of {repo}"));
            return;
        }
        self.backend.checkout(repo, n);
        self.set_status(format!("checking out #{n}…"));
    }

    /// Save one field of the item under metadata edit and reflect it locally.
    fn meta_save(&mut self, value_text: &str) {
        let Some(m) = &mut self.meta else { return };
        let (name, repo, number) = (m.fields[m.sel].name, m.repo.clone(), m.number);
        m.fields[m.sel].value = match name {
            "draft" => if value_text == "true" { "☑" } else { "☐" }.to_string(),
            _ => value_text.trim().to_string(),
        };
        m.mode = MetaMode::List;
        let key = ItemDoc::key(&repo, number);
        let Some(doc) = self.cache.items.get_mut(&key) else { return };
        let cmds = edit_cmds(&repo, doc, name, value_text);
        if cmds.is_empty() {
            self.set_status("unchanged");
            return;
        }
        apply_field(doc, name, value_text);
        self.backend.edit(key, cmds);
        self.set_status(format!("saving {name}…"));
    }

    fn on_meta_key(&mut self, key: KeyEvent) {
        enum Act {
            None,
            Close,
            Save(String),
        }
        let mut act = Act::None;
        let Some(m) = &mut self.meta else { return };
        match &mut m.mode {
            MetaMode::List => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('m') => act = Act::Close,
                KeyCode::Char('j') | KeyCode::Down => {
                    m.sel = (m.sel + 1).min(m.fields.len().saturating_sub(1))
                }
                KeyCode::Char('k') | KeyCode::Up => m.sel = m.sel.saturating_sub(1),
                KeyCode::Enter => {
                    let field = &m.fields[m.sel];
                    let meta = self.cache.meta.get(&m.repo).cloned().unwrap_or_default();
                    let multi = |opts: Vec<String>| {
                        let current: Vec<&str> = field.value.split(',').map(str::trim).collect();
                        let chosen = opts.iter().map(|o| current.contains(&o.as_str())).collect();
                        MetaMode::MultiPick { options: opts, chosen, sel: 0 }
                    };
                    let input = || {
                        let buf = field.value.clone();
                        let pos = buf.chars().count();
                        MetaMode::Input { buf, pos }
                    };
                    match field.name {
                        "draft" => act = Act::Save(if field.value == "☑" { "false" } else { "true" }.into()),
                        "state" => act = Act::Save(if field.value == "open" { "closed" } else { "open" }.into()),
                        "labels" if !meta.labels.is_empty() => m.mode = multi(meta.labels),
                        "assignees" | "reviewers" if !meta.assignees.is_empty() => m.mode = multi(meta.assignees),
                        "milestone" if !meta.milestones.is_empty() => {
                            let cur = meta.milestones.iter().position(|o| *o == field.value).map_or(0, |i| i + 1);
                            m.mode = MetaMode::Pick { options: meta.milestones, sel: cur };
                        }
                        _ => m.mode = input(),
                    }
                }
                _ => {}
            },
            MetaMode::Input { buf, pos } => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => m.mode = MetaMode::List,
                    KeyCode::Enter => act = Act::Save(buf.clone()),
                    KeyCode::Left => *pos = pos.saturating_sub(1),
                    KeyCode::Right => *pos = (*pos + 1).min(buf.chars().count()),
                    KeyCode::Home => *pos = 0,
                    KeyCode::End => *pos = buf.chars().count(),
                    KeyCode::Char('a') if ctrl => *pos = 0,
                    KeyCode::Char('e') if ctrl => *pos = buf.chars().count(),
                    KeyCode::Backspace => {
                        if *pos > 0 {
                            *pos -= 1;
                            let i = char_byte(buf, *pos);
                            buf.remove(i);
                        }
                    }
                    KeyCode::Delete => {
                        let i = char_byte(buf, *pos);
                        if i < buf.len() {
                            buf.remove(i);
                        }
                    }
                    KeyCode::Char('u') if ctrl => {
                        let i = char_byte(buf, *pos);
                        buf.drain(..i);
                        *pos = 0;
                    }
                    KeyCode::Char('w') if ctrl => {
                        let chars: Vec<char> = buf.chars().collect();
                        let mut p = *pos;
                        while p > 0 && chars[p - 1] == ' ' {
                            p -= 1;
                        }
                        while p > 0 && chars[p - 1] != ' ' {
                            p -= 1;
                        }
                        let (lo, hi) = (char_byte(buf, p), char_byte(buf, *pos));
                        buf.drain(lo..hi);
                        *pos = p;
                    }
                    KeyCode::Char(c) if !ctrl => {
                        let i = char_byte(buf, *pos);
                        buf.insert(i, c);
                        *pos += 1;
                    }
                    _ => {}
                }
            }
            MetaMode::Pick { options, sel } => match key.code {
                KeyCode::Esc => m.mode = MetaMode::List,
                KeyCode::Char('j') | KeyCode::Down => *sel = (*sel + 1).min(options.len()), // 0 = (clear)
                KeyCode::Char('k') | KeyCode::Up => *sel = sel.saturating_sub(1),
                KeyCode::Enter => {
                    act = Act::Save(if *sel == 0 { String::new() } else { options[*sel - 1].clone() });
                }
                _ => {}
            },
            MetaMode::MultiPick { options, chosen, sel } => match key.code {
                KeyCode::Esc => m.mode = MetaMode::List,
                KeyCode::Char('j') | KeyCode::Down => {
                    *sel = (*sel + 1).min(options.len().saturating_sub(1))
                }
                KeyCode::Char('k') | KeyCode::Up => *sel = sel.saturating_sub(1),
                KeyCode::Char(' ') => chosen[*sel] = !chosen[*sel],
                KeyCode::Enter => {
                    act = Act::Save(
                        options
                            .iter()
                            .zip(chosen.iter())
                            .filter(|(_, c)| **c)
                            .map(|(o, _)| o.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                }
                _ => {}
            },
        }
        match act {
            Act::None => {}
            Act::Close => self.meta = None,
            Act::Save(text) => self.meta_save(&text),
        }
    }

    /// Result of the external $EDITOR session; `None` = editor failed/aborted.
    pub fn on_external(&mut self, req: ExternalReq, result: Option<String>) {
        let Some(content) = result else {
            self.set_status("editor aborted, nothing saved");
            return;
        };
        let body = |s: &str| -> String {
            s.lines()
                .filter(|l| !l.trim_start().starts_with("<!--"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        };
        match req {
            ExternalReq::Body { repo, number, is_pr } => {
                let key = ItemDoc::key(&repo, number);
                if Some(content.as_str()) == self.cache.items.get(&key).map(|d| d.body.as_str()) {
                    self.set_status("unchanged");
                } else {
                    self.backend.save_body(repo, number, is_pr, content);
                    self.set_status("saving…");
                }
            }
            ExternalReq::Comment { repo, number, is_pr } => {
                let text = body(&content);
                if text.is_empty() {
                    self.set_status("empty, no comment posted");
                } else {
                    self.backend.comment(repo, number, is_pr, text);
                    self.set_status("posting comment…");
                }
            }
            ExternalReq::Review { repo, number, kind } => {
                let text = body(&content);
                if text.is_empty() && kind != ReviewKind::Approve {
                    self.review = None;
                    self.set_status("empty, no review submitted");
                } else {
                    self.review = Some(ReviewDialog {
                        repo, number, sel: ReviewKind::ALL.iter().position(|&k| k == kind).unwrap(),
                        body: text, mode: ReviewMode::Confirm,
                    });
                }
            }
            ExternalReq::NewIssue { repo } => {
                let text = body(&content);
                let (title, rest) = text.split_once('\n').unwrap_or((text.as_str(), ""));
                let title = title.trim_start_matches('#').trim();
                if title.is_empty() {
                    self.set_status("empty title, nothing created");
                } else {
                    self.backend.create_issue(repo, title.to_string(), rest.trim().to_string());
                    self.set_status("creating…");
                }
            }
        }
    }

    // ---- input ----

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.editor.is_some() {
            self.on_editor_key(key, ctrl);
            return;
        }
        if self.review.is_some() {
            self.on_review_key(key);
            return;
        }
        if ctrl && key.code == KeyCode::Char('k') {
            if self.search.is_some() {
                self.search = None;
            } else {
                self.repos = None;
                self.open_search();
            }
            return;
        }
        if self.search.is_some() {
            self.on_search_key(key);
            return;
        }
        if self.repos.is_some() {
            self.on_repos_key(key);
            return;
        }
        if self.meta.is_some() {
            self.on_meta_key(key);
            return;
        }
        if self.find_input {
            self.on_find_key(key);
            return;
        }
        // view picker owns Esc while it is open
        if matches!(key.code, KeyCode::Esc | KeyCode::Backspace)
            && let Some(Pane::List(t)) = self.stack.last_mut()
            && t.picker.is_some()
        {
            t.picker = None;
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            // esc clears an active find filter first, vim-style; a second esc goes back
            KeyCode::Esc => match self.stack.last_mut() {
                Some(Pane::Item(p)) if !p.find.is_empty() => {
                    p.find.clear();
                    p.recompute_matches();
                }
                Some(Pane::List(t)) if !t.find.is_empty() => {
                    t.find.clear();
                    t.state.select(Some(0));
                }
                _ => self.back(),
            },
            KeyCode::Backspace => self.back(),
            KeyCode::Char('R') => self.open_repos(),
            KeyCode::Char('r') => self.refresh_current(),
            KeyCode::Char('e') => self.start_edit(),
            KeyCode::Char('E') => self.start_external_body(),
            KeyCode::Char('m') => self.start_meta_edit(),
            KeyCode::Char('a') => self.start_new_issue(),
            KeyCode::Char('C') => self.start_comment(),
            KeyCode::Char('V') => self.start_review(),
            KeyCode::Char('c') => self.checkout_pr(),
            KeyCode::Char('o') => {
                let url = match self.stack.last() {
                    Some(Pane::Item(p)) => Some(format!("https://github.com/{}/issues/{}", p.repo, p.number)),
                    Some(Pane::List(t)) => Some(format!("https://github.com/{}/{}", t.repo, VIEWS[t.view].1)),
                    None => None,
                };
                if let Some(url) = url {
                    self.open_external(&url);
                }
            }
            _ => match self.stack.last_mut() {
                Some(Pane::Item(_)) => self.on_item_key(key, ctrl),
                Some(Pane::List(_)) => self.on_list_key(key, ctrl),
                None => {}
            },
        }
    }

    fn on_editor_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(ed) = &mut self.editor else { return };
        if ctrl && key.code == KeyCode::Char('s') {
            if !ed.saving {
                ed.saving = true;
                let content: String = ed.state.lines.flatten(&Some('\n')).into_iter().collect();
                self.backend.save_body(ed.repo.clone(), ed.number, ed.is_pr, content);
                self.set_status("saving…");
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('q') {
            self.editor = None;
            self.set_status("edit discarded");
            return;
        }
        ed.handler.on_key_event(key, &mut ed.state);
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        let Some(s) = &mut self.search else { return };
        match key.code {
            KeyCode::Esc => {
                self.search = None;
                if self.stack.is_empty() {
                    self.quit = true;
                }
            }
            KeyCode::Enter => {
                if let Some(hit) = s.hits.get(s.sel).cloned() {
                    self.search = None;
                    self.open_item(hit.repo, hit.number, hit.title);
                }
            }
            KeyCode::Up => s.sel = s.sel.saturating_sub(1),
            KeyCode::Down => s.sel = (s.sel + 1).min(s.hits.len().saturating_sub(1)),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                s.sel = s.sel.saturating_sub(1)
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                s.sel = (s.sel + 1).min(s.hits.len().saturating_sub(1))
            }
            KeyCode::Backspace => {
                s.input.pop();
                s.server_hits.clear();
                s.refilter();
                s.dirty_at = Some(Instant::now());
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                s.input.push(c);
                s.server_hits.clear();
                s.refilter();
                s.dirty_at = Some(Instant::now());
            }
            _ => {}
        }
    }

    fn on_repos_key(&mut self, key: KeyEvent) {
        let Some(p) = &mut self.repos else { return };
        match key.code {
            KeyCode::Esc => {
                self.repos = None;
                if self.stack.is_empty() {
                    self.quit = true;
                }
            }
            KeyCode::Enter => {
                // a typed owner/name opens even if unknown
                let typed = p.input.trim().to_string();
                let pick = p.hits.get(p.sel).cloned().or_else(|| typed.contains('/').then_some(typed));
                if let Some(repo) = pick {
                    self.repos = None;
                    self.open_list(repo);
                }
            }
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down => p.sel = (p.sel + 1).min(p.hits.len().saturating_sub(1)),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                p.sel = p.sel.saturating_sub(1)
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                p.sel = (p.sel + 1).min(p.hits.len().saturating_sub(1))
            }
            KeyCode::Backspace => {
                p.input.pop();
                self.repos_refilter();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                p.input.push(c);
                self.repos_refilter();
            }
            _ => {}
        }
    }

    fn on_find_key(&mut self, key: KeyEvent) {
        enum Act {
            Cancel,
            Done,
            Pop,
            Push(char),
        }
        let act = match key.code {
            KeyCode::Esc => Act::Cancel,
            KeyCode::Enter => Act::Done,
            KeyCode::Backspace => Act::Pop,
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => Act::Push(c),
            _ => return,
        };
        if matches!(act, Act::Done) {
            self.find_input = false;
            return;
        }
        match self.stack.last_mut() {
            Some(Pane::Item(p)) => {
                match act {
                    Act::Cancel => p.find.clear(),
                    Act::Pop => {
                        p.find.pop();
                    }
                    Act::Push(c) => p.find.push(c),
                    Act::Done => unreachable!(),
                }
                p.recompute_matches();
                // live-jump to the nearest match at/after the current position
                if !p.matches.is_empty() {
                    p.msel = p.matches.iter().position(|&l| l >= p.scroll).unwrap_or(0);
                    p.goto_match();
                }
            }
            Some(Pane::List(t)) => {
                match act {
                    Act::Cancel => t.find.clear(),
                    Act::Pop => {
                        t.find.pop();
                    }
                    Act::Push(c) => t.find.push(c),
                    Act::Done => unreachable!(),
                }
                t.state.select(Some(0));
            }
            None => {}
        }
        if matches!(act, Act::Cancel) {
            self.find_input = false;
        }
    }

    fn on_item_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(Pane::Item(p)) = self.stack.last_mut() else { return };
        let max = p.lines.len().saturating_sub(1);
        let page = (p.viewport / 2).max(1) as usize;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => p.scroll = (p.scroll + 1).min(max),
            KeyCode::Char('k') | KeyCode::Up => p.scroll = p.scroll.saturating_sub(1),
            KeyCode::Char('d') if ctrl => p.scroll = (p.scroll + page).min(max),
            KeyCode::Char('u') if ctrl => p.scroll = p.scroll.saturating_sub(page),
            KeyCode::PageDown => p.scroll = (p.scroll + page * 2).min(max),
            KeyCode::PageUp => p.scroll = p.scroll.saturating_sub(page * 2),
            KeyCode::Char('g') | KeyCode::Home => p.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => p.scroll = max,
            KeyCode::Char('x') => {
                if let Some(doc) = self.cache.items.get(&p.key()) {
                    p.toggle_bot_comments(doc);
                }
            }
            KeyCode::Char('/') => {
                p.find.clear();
                p.recompute_matches();
                self.find_input = true;
            }
            KeyCode::Char('n') if !p.matches.is_empty() => {
                p.msel = (p.msel + 1) % p.matches.len();
                p.goto_match();
            }
            KeyCode::Char('N') if !p.matches.is_empty() => {
                p.msel = (p.msel + p.matches.len() - 1) % p.matches.len();
                p.goto_match();
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if !p.links.is_empty() {
                    let n = p.links.len();
                    p.link_sel = Some(match (p.link_sel, key.code) {
                        (None, KeyCode::Tab) => p.links.iter().position(|l| l.line >= p.scroll).unwrap_or(0),
                        (None, _) => n - 1,
                        (Some(i), KeyCode::Tab) => (i + 1) % n,
                        (Some(i), _) => (i + n - 1) % n,
                    });
                    // keep selected link visible
                    if let Some(i) = p.link_sel {
                        let line = p.links[i].line;
                        let vp = p.viewport as usize;
                        if line < p.scroll || line >= p.scroll + vp {
                            p.scroll = line.saturating_sub(vp / 2);
                        }
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(link) = p.link_sel.and_then(|i| p.links.get(i)) {
                    let target = link.target.clone();
                    let hint = match &target {
                        Target::External(u) => u.clone(),
                        _ => p.lines[link.line].spans[link.spans.0..link.spans.1]
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>(),
                    };
                    self.open_target(target, hint);
                }
            }
            _ => {}
        }
    }

    fn on_list_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(Pane::List(t)) = self.stack.last_mut() else { return };
        if let Some(sel) = t.picker {
            let n = VIEWS.len();
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => t.picker = Some((sel + 1).min(n - 1)),
                KeyCode::Char('k') | KeyCode::Up => t.picker = Some(sel.saturating_sub(1)),
                KeyCode::Char('g') | KeyCode::Home => t.picker = Some(0),
                KeyCode::Char('G') | KeyCode::End => t.picker = Some(n - 1),
                KeyCode::Enter => self.list_select_view(sel),
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('v') {
            t.picker = Some(t.view);
            return;
        }
        let vis = t.visible();
        let nrows = vis.len();
        let sel = t.state.selected().unwrap_or(0);
        let mut want_more = false;
        match key.code {
            KeyCode::Char('/') => {
                t.find.clear();
                t.state.select(Some(0));
                self.find_input = true;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if sel + 1 < nrows {
                    t.state.select(Some(sel + 1));
                } else {
                    want_more = true;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => t.state.select(Some(sel.saturating_sub(1))),
            KeyCode::Char('d') if ctrl => t.state.select(Some((sel + 10).min(nrows.saturating_sub(1)))),
            KeyCode::Char('u') if ctrl => t.state.select(Some(sel.saturating_sub(10))),
            KeyCode::Char('g') | KeyCode::Home => t.state.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                t.state.select(Some(nrows.saturating_sub(1)));
                want_more = true;
            }
            KeyCode::Char('h') | KeyCode::Left => t.col_offset = t.col_offset.saturating_sub(1),
            KeyCode::Char('l') | KeyCode::Right => {
                t.col_offset = (t.col_offset + 1).min(t.columns().len().saturating_sub(3))
            }
            KeyCode::Char('x') => t.expanded = !t.expanded,
            KeyCode::Enter => {
                if let Some(row) = vis.get(sel).and_then(|&i| t.doc.rows.get(i)) {
                    let (repo, n) = (t.repo.clone(), row.number);
                    let hint = row.cells.get(1).cloned().unwrap_or_default();
                    self.open_item(repo, n, hint);
                    return;
                }
            }
            _ => {}
        }
        if want_more && t.doc.has_more {
            let next = t.doc.page + 1;
            self.list_fetch(next);
            self.set_status("loading more…");
        }
    }

    // ---- backend messages ----

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Item { key, res } => {
                self.backend.finish(Some(&format!("item:{key}")));
                match res {
                    Ok(doc) => {
                        let mut repo = String::new();
                        for pane in &mut self.stack {
                            if let Pane::Item(p) = pane
                                && p.key() == key
                            {
                                p.set_doc(&doc);
                                repo = p.repo.clone();
                            }
                        }
                        if repo.is_empty() {
                            repo = key.rsplit_once('#').map(|(r, _)| r.to_string()).unwrap_or_default();
                        }
                        self.cache.remember([SearchHit {
                            repo,
                            number: doc.number,
                            title: doc.title.clone(),
                            is_pr: doc.is_pr,
                            state: doc.state.clone(),
                        }]);
                        self.cache.items.insert(key, doc);
                        self.cache.save();
                    }
                    Err(e) => {
                        self.set_status(format!("load failed: {e}"));
                        for pane in &mut self.stack {
                            if let Pane::Item(p) = pane
                                && p.key() == key
                                && !p.loaded
                            {
                                p.lines = vec![Line::from(format!("  error: {e}"))];
                            }
                        }
                    }
                }
            }
            Msg::List { repo, key, page, res } => {
                self.backend.finish(Some(&format!("list:{key}")));
                match res {
                    Ok(doc) => {
                        // every row title becomes instantly searchable
                        self.cache.remember(doc.rows.iter().map(|r| SearchHit {
                            repo: repo.clone(),
                            number: r.number,
                            title: r.cells.get(1).cloned().unwrap_or_default(),
                            is_pr: r.is_pr,
                            state: r.cells.get(2).cloned().unwrap_or_default(),
                        }));
                        let mut merged: Option<ListDoc> = None;
                        for pane in &mut self.stack {
                            if let Pane::List(t) = pane
                                && t.key() == key
                            {
                                t.set_doc(doc.clone(), page > 1);
                                merged = Some(t.doc.clone());
                            }
                        }
                        // cache what the pane shows (all pages loaded so far)
                        self.cache.lists.insert(key, merged.unwrap_or(doc));
                        self.cache.save();
                    }
                    Err(e) => self.set_status(format!("list load failed: {e}")),
                }
            }
            Msg::Search { seq, repo, query, res } => {
                self.backend.finish(None);
                if let Ok(hits) = &res {
                    self.cache.remember(hits.clone());
                    self.cache.save();
                }
                if let Some(s) = &mut self.search
                    && s.repo == repo
                {
                    match res {
                        Ok(hits) => {
                            // any response enriches the pool; everything is then
                            // re-ranked locally (frecency included) — server order
                            // only survives as the last tiebreak via pool position
                            s.merge_pool(&hits);
                            if query == s.input {
                                s.server_hits = hits;
                            }
                            let keep = s.hits.get(s.sel).map(SearchHit::id);
                            s.refilter();
                            s.sel = keep
                                .and_then(|id| s.hits.iter().position(|h| h.id() == id))
                                .unwrap_or(0);
                        }
                        Err(e) => {
                            if seq == self.search_seq {
                                self.set_status(format!("search failed: {e}"));
                            }
                        }
                    }
                }
            }
            Msg::Saved { key, content, res } => {
                self.backend.finish(Some(&format!("save:{key}")));
                match res {
                    Ok(()) => {
                        if let Some(doc) = self.cache.items.get_mut(&key) {
                            doc.body = content;
                        }
                        self.editor = None;
                        self.set_status("saved ✓");
                        self.refetch(&key);
                    }
                    Err(e) => {
                        if let Some(ed) = &mut self.editor {
                            ed.saving = false;
                        }
                        self.set_status(format!("save failed: {e}"));
                    }
                }
            }
            Msg::Edited { key, res } => {
                self.backend.finish(Some(&format!("edit:{key}")));
                match res {
                    Ok(()) => {
                        self.set_status("saved ✓");
                        self.refetch(&key);
                        if let Some((repo, _)) = key.rsplit_once('#') {
                            self.refresh_lists(repo);
                        }
                    }
                    Err(e) => {
                        self.set_status(format!("edit failed: {e}"));
                        self.refetch(&key); // undo the optimistic local change
                    }
                }
            }
            Msg::Commented { key, res } => {
                self.backend.finish(Some(&format!("comment:{key}")));
                match res {
                    Ok(()) => {
                        self.set_status("comment posted ✓");
                        self.refetch(&key);
                    }
                    Err(e) => self.set_status(format!("comment failed: {e}")),
                }
            }
            Msg::Reviewed { key, res } => {
                self.backend.finish(Some(&format!("review:{key}")));
                match res {
                    Ok(()) => {
                        self.review = None;
                        self.set_status("review submitted ✓");
                        self.refetch(&key);
                        if let Some((repo, _)) = key.rsplit_once('#') {
                            // Other cached views must not keep the old decision.
                            self.cache.lists.retain(|k, _| !k.starts_with(&format!("{repo}:pulls:")));
                            self.cache.save();
                            self.refresh_lists(repo);
                        }
                    }
                    Err(e) => {
                        if let Some(review) = &mut self.review {
                            review.mode = ReviewMode::Confirm;
                        }
                        self.set_status(format!("review failed: {e}"));
                    }
                }
            }
            Msg::Created { repo, res } => {
                self.backend.finish(Some(&format!("create:{repo}")));
                match res {
                    Ok(n) => {
                        self.set_status(format!("created #{n} ✓"));
                        self.refresh_lists(&repo);
                        self.open_item(repo, n, format!("#{n}"));
                    }
                    Err(e) => self.set_status(format!("create failed: {e}")),
                }
            }
            Msg::Meta { repo, res } => {
                self.backend.finish(Some(&format!("meta:{repo}")));
                match res {
                    Ok(m) => {
                        self.cache.meta.insert(repo, m);
                        self.cache.save();
                    }
                    Err(e) => self.set_status(format!("repo metadata failed: {e}")),
                }
            }
            Msg::Repos { res } => {
                self.backend.finish(Some("repos"));
                match res {
                    Ok(repos) => {
                        self.cache.add_repos(&repos);
                        self.cache.save();
                        self.repos_refilter();
                    }
                    Err(e) => self.set_status(format!("repo list failed: {e}")),
                }
            }
            Msg::CwdPr { repo, res } => {
                self.backend.finish(Some("cwdpr"));
                match res {
                    Ok(Some((n, title))) => {
                        // only auto-open while still sitting on the startup list
                        if self.stack.len() == 1 && self.stack[0].repo() == repo {
                            self.open_item(repo, n, title);
                            self.set_status(format!("#{n} is the PR of the current branch"));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => self.set_status(format!("branch PR lookup failed: {e}")),
                }
            }
            Msg::Checkout { key, res } => {
                self.backend.finish(Some(&format!("checkout:{key}")));
                match res {
                    Ok(()) => self.set_status(format!("checked out {key} ✓")),
                    Err(e) => self.set_status(format!("checkout failed: {e}")),
                }
            }
        }
    }

    fn refetch(&mut self, key: &str) {
        if let Some((repo, n)) = key.rsplit_once('#')
            && let Ok(n) = n.parse()
        {
            self.backend.fetch_item(repo.to_string(), n);
        }
    }

    pub fn tick(&mut self) {
        if let Some((_, t)) = &self.status
            && t.elapsed().as_secs() >= 5
        {
            self.status = None;
        }
        let mut fire: Option<(String, String)> = None;
        if let Some(s) = &mut self.search
            && let Some(t) = s.dirty_at
            && t.elapsed().as_millis() >= 250
            && s.input != s.fired
        {
            s.dirty_at = None;
            s.fired = s.input.clone();
            fire = Some((s.repo.clone(), s.input.clone()));
        }
        if let Some((repo, q)) = fire {
            self.search_seq += 1;
            self.backend.search(repo, q, self.search_seq);
        }
    }
}

const NEW_ISSUE_TEMPLATE: &str = "# \n\n<!-- First line \"# Title\" becomes the issue title; the rest becomes the body. -->\n<!-- Save with an empty title (or empty file) to abort. -->\n";
const COMMENT_TEMPLATE: &str = "\n<!-- Write your comment above. Save empty to abort. -->\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new(std::sync::mpsc::channel().0, None, Some("o/r".into()));
        app.search = None;
        app.repos = None;
        app
    }

    // No worker threads or disk cache: review tests must never submit real reviews.
    fn review_app(is_pr: bool) -> App {
        let doc = ItemDoc { number: 7, is_pr, state: "open".into(), ..Default::default() };
        let mut view = ItemView::new("o/r".into(), 7, String::new());
        view.set_doc(&doc);
        let mut cache = Cache::default();
        cache.items.insert("o/r#7".into(), doc);
        App {
            stack: vec![Pane::Item(view)], search: None, repos: None, editor: None,
            backend: Backend::new(std::sync::mpsc::channel().0), cache, status: None,
            quit: false, find_input: false, external: None, meta: None, review: None,
            cwd_repo: None, search_seq: 0,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn comment(author: &str, is_bot: bool, body: String) -> crate::backend::Comment {
        crate::backend::Comment {
            author: author.into(), is_bot, date: "2026-09-05".into(), kind: String::new(), body,
        }
    }

    fn item_text(view: &ItemView) -> String {
        view.lines.iter().map(|line| line.to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn only_long_bot_comments_are_collapsed_by_default() {
        let doc = ItemDoc {
            body: "PR description".into(),
            comments: vec![
                comment("review-app", true, "Hidden bot detail\n".repeat(11)),
                comment("cached-app[bot]", false, "Hidden cached detail\n".repeat(11)),
                comment("review-app", true, "z".repeat(1001)),
                comment("review-app", true, "Short bot comment".into()),
                comment("review-app", true, "Ten lines\n".repeat(10)),
                comment("review-app", true, "é".repeat(1000)),
                comment("robotics-dev", false, "Human detail\n".repeat(11)),
            ],
            ..Default::default()
        };
        for is_pr in [false, true] {
            let mut view = ItemView::new("o/r".into(), 7, String::new());
            view.set_doc(&ItemDoc { is_pr, ..doc.clone() });
            let text = item_text(&view);
            assert_eq!(text.matches("long bot comment hidden").count(), 3);
            assert!(!text.contains("Hidden bot detail"));
            assert!(!text.contains("Hidden cached detail"));
            assert!(!text.contains(&"z".repeat(1001)));
            for visible in ["PR description", "Short bot comment", "Ten lines", "Human detail", &"é".repeat(1000)] {
                assert!(text.contains(visible));
            }
        }
    }

    #[test]
    fn bot_comment_toggle_updates_links_and_search_and_preserves_position() {
        let mut app = review_app(true);
        let doc = app.cache.items.get_mut("o/r#7").unwrap();
        let mut review = comment("review-app", true, format!("{}\n[Hidden detail](https://example.com/bot)", "Review detail\n".repeat(11)));
        review.kind = "changes requested".into();
        doc.comments = vec![review, comment("alice", false, "[Human reply](https://example.com/human)".into())];
        let Some(Pane::Item(view)) = app.stack.last_mut() else { unreachable!() };
        view.find = "Hidden detail".into();
        view.set_doc(doc);
        view.scroll = view.comment_lines[1];
        let collapsed_scroll = view.scroll;
        assert!(item_text(view).contains("review-app · 2026-09-05 · changes requested"));
        assert_eq!(view.links.len(), 1);
        assert!(view.matches.is_empty());

        app.on_key(key(KeyCode::Char('x')));
        let Some(Pane::Item(view)) = app.stack.last_mut() else { unreachable!() };
        assert!(view.show_long_bot_comments);
        assert_eq!(view.links.len(), 2);
        assert_eq!(view.matches.len(), 1);
        assert_eq!(view.scroll, view.comment_lines[1]);
        assert!(view.scroll > collapsed_scroll);
        assert!(view.lines[view.links[0].line].to_string().contains("Hidden detail"));
        assert!(view.lines[view.links[1].line].to_string().contains("Human reply"));
        // A background refresh keeps the user's expansion choice.
        view.set_doc(app.cache.items.get("o/r#7").unwrap());
        assert!(item_text(view).contains("Hidden detail"));
        view.link_sel = Some(0);

        app.on_key(key(KeyCode::Char('x')));
        let Some(Pane::Item(view)) = app.stack.last() else { unreachable!() };
        assert!(!view.show_long_bot_comments);
        assert_eq!(view.links.len(), 1);
        assert!(view.matches.is_empty());
        assert!(view.link_sel.is_none());
        assert_eq!(view.scroll, collapsed_scroll);
        assert!(view.lines[view.links[0].line].to_string().contains("Human reply"));
        assert_eq!(app.backend.pending, 0);
    }

    #[test]
    fn review_picker_is_pr_only_and_can_cancel() {
        let mut issue = review_app(false);
        issue.on_key(key(KeyCode::Char('V')));
        assert!(issue.review.is_none());

        let mut app = review_app(true);
        app.on_key(key(KeyCode::Char('V')));
        assert_eq!(app.review.as_ref().unwrap().kind(), ReviewKind::Comment);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        let ext = app.external.take().unwrap();
        assert!(matches!(ext.req, ExternalReq::Review { kind: ReviewKind::Approve, number: 7, .. }));
        app.on_external(ext.req, Some(ext.template));
        let review = app.review.as_ref().unwrap();
        assert!(review.mode == ReviewMode::Confirm);
        assert!(review.body.is_empty());
        assert_eq!(app.backend.pending, 0); // previewing does not submit
        app.on_key(key(KeyCode::Esc));
        assert!(app.review.is_none());
        assert_eq!(app.stack.len(), 1); // Escape belongs to the dialog
        assert!(!app.quit);
    }

    #[test]
    fn review_requires_text_except_for_approval() {
        for kind in [ReviewKind::Comment, ReviewKind::RequestChanges] {
            let mut app = review_app(true);
            app.on_external(ExternalReq::Review { repo: "o/r".into(), number: 7, kind },
                Some("\n<!-- template -->\n  ".into()));
            assert!(app.review.is_none());
            assert_eq!(app.backend.pending, 0);
            app.on_external(ExternalReq::Review { repo: "o/r".into(), number: 7, kind },
                Some("Please fix this.\n\nDetails.\n<!-- template -->".into()));
            let review = app.review.as_ref().unwrap();
            assert_eq!(review.kind(), kind);
            assert_eq!(review.body, "Please fix this.\n\nDetails.");
            assert!(review.mode == ReviewMode::Confirm);
        }
    }

    #[test]
    fn failed_review_keeps_draft_and_editor_abort_preserves_it() {
        let mut app = review_app(true);
        app.on_external(ExternalReq::Review {
            repo: "o/r".into(), number: 7, kind: ReviewKind::RequestChanges,
        }, Some("Please fix this.".into()));
        app.review.as_mut().unwrap().mode = ReviewMode::Submitting;
        app.on_key(key(KeyCode::Enter)); // no duplicate request while submitting
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.backend.pending, 0);
        assert!(app.review.is_some());
        app.on_msg(Msg::Reviewed { key: "o/r#7".into(), res: Err(anyhow::anyhow!("permission denied")) });
        let review = app.review.as_ref().unwrap();
        assert!(review.mode == ReviewMode::Confirm);
        assert_eq!(review.body, "Please fix this.");
        assert!(app.status.as_ref().unwrap().0.contains("permission denied"));
        app.on_key(key(KeyCode::Char('e')));
        let ext = app.external.take().unwrap();
        assert!(ext.template.starts_with("Please fix this."));
        app.on_external(ext.req, None);
        assert_eq!(app.review.as_ref().unwrap().body, "Please fix this.");
    }

    #[test]
    fn pr_columns_and_filter_include_review_status() {
        let mut list = ListView::new("o/r".into());
        assert!(!list.columns().contains(&"review"));
        list.view = 1;
        assert_eq!(list.columns()[3], "review");
        list.doc.rows.push(crate::backend::RowDoc {
            number: 7, is_pr: true, cells: vec!["#7".into(), "Title".into(), "open".into()],
            review_status: "changes requested".into(),
        });
        list.find = "CHANGES".into();
        assert_eq!(list.visible(), vec![0]);
        list.find = "approved".into();
        assert!(list.visible().is_empty());
    }

    #[test]
    fn review_table_and_dialog_render() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = review_app(true);
        let mut list = ListView::new("o/r".into());
        list.view = 1;
        list.loaded = true;
        list.doc.rows.push(crate::backend::RowDoc {
            number: 7, is_pr: true,
            cells: vec!["#7".into(), "Title".into(), "open".into(), "alice".into()],
            review_status: "changes requested".into(),
        });
        app.stack = vec![Pane::List(list)];
        for expanded in [false, true] {
            let Some(Pane::List(list)) = app.stack.last_mut() else { unreachable!() };
            list.expanded = expanded;
            let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let text = buffer.content.iter().map(|c| c.symbol()).collect::<String>();
            assert!(text.contains("changes requested"));
            assert!(text.contains("alice"));
            let red = buffer.content.iter().filter(|c| c.fg == Color::Red)
                .map(|c| c.symbol()).collect::<String>();
            assert!(red.contains("changes requested"));
        }
        app = review_app(true);
        app.on_key(key(KeyCode::Char('V')));
        for (width, height) in [(80, 24), (10, 3), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        }
    }

    #[test]
    fn meta_input_cursor_editing() {
        let mut app = app();
        app.meta = Some(MetaEditor {
            repo: "o/r".into(),
            number: 1,
            fields: vec![MetaField { name: "title", value: String::new() }],
            sel: 0,
            mode: MetaMode::Input { buf: "héllo wörld".into(), pos: 11 },
        });
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let buf_pos = |app: &App| match &app.meta.as_ref().unwrap().mode {
            MetaMode::Input { buf, pos } => (buf.clone(), *pos),
            _ => panic!("not input mode"),
        };

        app.on_key(ctrl('w')); // delete "wörld"
        assert_eq!(buf_pos(&app), ("héllo ".into(), 6));
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Backspace)); // delete the second 'l'
        assert_eq!(buf_pos(&app), ("hélo ".into(), 3));
        app.on_key(key(KeyCode::Char('L')));
        assert_eq!(buf_pos(&app), ("hélLo ".into(), 4));
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Delete));
        assert_eq!(buf_pos(&app), ("élLo ".into(), 0));
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::Right)); // clamped at end
        assert_eq!(buf_pos(&app).1, 5);
        app.on_key(ctrl('u'));
        assert_eq!(buf_pos(&app), (String::new(), 0));
    }

    #[test]
    fn startup_args_and_item_links() {
        let mut app = app();
        assert_eq!(app.stack.len(), 1);
        assert_eq!(app.cur_repo().as_deref(), Some("o/r"));
        // bare #N link resolves against the current pane's repo
        app.open_target(Target::Item { repo: None, number: 5 }, "x".into());
        assert!(matches!(app.stack.last(), Some(Pane::Item(p)) if p.repo == "o/r" && p.number == 5));
        // owner/repo#N argument opens both list and item
        let app2 = App::new(std::sync::mpsc::channel().0, None, Some("a/b#9".into()));
        assert_eq!(app2.stack.len(), 2);
        assert!(matches!(app2.stack.last(), Some(Pane::Item(p)) if p.repo == "a/b" && p.number == 9));
    }

    #[test]
    fn rank_ordering() {
        let mk = |n: u64, t: &str| SearchHit { repo: "o/r".into(), number: n, title: t.into(), is_pr: false, state: String::new() };
        let pool = vec![
            mk(1, "Weekly sync"),
            mk(2, "Roadmap 2023"),
            mk(3, "Roadmap"),
            mk(4, "cloud-roadmap"),
            mk(5, "Random page"),
        ];
        let none = HashMap::new();
        let key = |h: &SearchHit| (h.id(), h.title.clone());
        let out: Vec<&str> = rank(&pool, "road", &none, key).into_iter().map(|h| h.title.as_str()).collect();
        // word-start matches share the top tier; shorter title = denser match wins ties
        assert_eq!(out, vec!["Roadmap", "Roadmap 2023", "cloud-roadmap"]);
        // fuzzy subsequence still matches
        assert!(rank(&pool, "rdmp", &none, key).iter().any(|h| h.title == "Roadmap 2023"));
        assert_eq!(rank(&pool, "", &none, key).len(), 5);
        // frecency dominates within the same match tier
        let frec = HashMap::from([("o/r#2".to_string(), 5000i64)]);
        let out: Vec<&str> = rank(&pool, "road", &frec, key).into_iter().map(|h| h.title.as_str()).collect();
        assert_eq!(out[0], "Roadmap 2023");
        // multi-word: every word must match
        let out: Vec<&str> = rank(&pool, "road 2023", &none, key).into_iter().map(|h| h.title.as_str()).collect();
        assert_eq!(out, vec!["Roadmap 2023"]);
    }
}
