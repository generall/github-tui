//! gh CLI subprocess calls, worker threads, and the disk cache.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

/// An issue or pull request, fully loaded.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ItemDoc {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub is_pr: bool,
    /// open / closed / merged
    pub state: String,
    pub draft: bool,
    pub author: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub reviewers: Vec<String>,
    #[serde(default)]
    pub review_status: String,
    pub milestone: String,
    pub head: String,
    pub base: String,
    pub created: String,
    pub updated: String,
    pub url: String,
    pub comments: Vec<Comment>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    #[serde(default)]
    pub is_bot: bool,
    pub date: String,
    /// empty for plain comments, review state (approved, …) for reviews
    pub kind: String,
    pub body: String,
}

impl ItemDoc {
    pub fn key(repo: &str, number: u64) -> String {
        format!("{repo}#{number}")
    }

    /// Header lines shown above the body.
    pub fn props(&self) -> Vec<(String, String)> {
        let mut p = vec![(
            "state".to_string(),
            if self.draft { format!("{} (draft)", self.state) } else { self.state.clone() },
        )];
        p.push(("author".into(), self.author.clone()));
        if self.is_pr {
            p.push(("branch".into(), format!("{} → {}", self.head, self.base)));
            p.push(("review".into(), self.review_status.clone()));
        }
        for (k, v) in [("labels", &self.labels), ("assignees", &self.assignees), ("reviewers", &self.reviewers)] {
            if !v.is_empty() {
                p.push((k.into(), v.join(", ")));
            }
        }
        if !self.milestone.is_empty() {
            p.push(("milestone".into(), self.milestone.clone()));
        }
        p.push(("created".into(), self.created.clone()));
        p.push(("updated".into(), self.updated.clone()));
        p
    }
}

pub const COLUMNS: &[&str] = &["#", "title", "state", "author", "labels", "assignees", "milestone", "updated"];
pub const PR_COLUMNS: &[&str] = &["#", "title", "state", "review", "author", "labels", "assignees", "milestone", "updated"];
const PAGE: usize = 50;

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ListDoc {
    pub rows: Vec<RowDoc>,
    pub has_more: bool,
    /// last page loaded (1-based)
    pub page: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RowDoc {
    pub number: u64,
    pub is_pr: bool,
    pub cells: Vec<String>,
    // Keep the original cell order so caches from older versions still load.
    #[serde(default)]
    pub review_status: String,
}

impl RowDoc {
    pub fn cell(&self, column: &str) -> &str {
        if column == "review" {
            return if self.review_status.is_empty() { "—" } else { &self.review_status };
        }
        COLUMNS.iter().position(|&c| c == column)
            .and_then(|i| self.cells.get(i)).map_or("", String::as_str)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewKind {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewKind {
    pub const ALL: [Self; 3] = [Self::Comment, Self::Approve, Self::RequestChanges];

    pub fn label(self) -> &'static str {
        match self {
            Self::Comment => "Comment",
            Self::Approve => "Approve",
            Self::RequestChanges => "Request changes",
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Comment => "--comment",
            Self::Approve => "--approve",
            Self::RequestChanges => "--request-changes",
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub is_pr: bool,
    #[serde(default)]
    pub state: String,
}

impl SearchHit {
    pub fn id(&self) -> String {
        ItemDoc::key(&self.repo, self.number)
    }
}

/// Per-repo option lists for the metadata editor.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct RepoMeta {
    pub labels: Vec<String>,
    pub milestones: Vec<String>,
    pub assignees: Vec<String>,
}

#[allow(clippy::large_enum_variant)] // one message at a time on the channel
pub enum Msg {
    Item { key: String, res: Result<ItemDoc> },
    List { repo: String, key: String, page: u32, res: Result<ListDoc> },
    Search { seq: u64, repo: String, query: String, res: Result<Vec<SearchHit>> },
    Saved { key: String, content: String, res: Result<()> },
    Edited { key: String, res: Result<()> },
    Created { repo: String, res: Result<u64> },
    Commented { key: String, res: Result<()> },
    Reviewed { key: String, res: Result<()> },
    Meta { repo: String, res: Result<RepoMeta> },
    Repos { res: Result<Vec<String>> },
    CwdPr { repo: String, res: Result<Option<(u64, String)>> },
    Checkout { key: String, res: Result<()> },
}

#[derive(Serialize, Deserialize, Default)]
pub struct Cache {
    pub items: HashMap<String, ItemDoc>,
    pub lists: HashMap<String, ListDoc>,
    /// everything ever seen (visited items, list rows, search results),
    /// most recently touched first — the ctrl+k instant-search pool
    pub pool: Vec<SearchHit>,
    /// per-item open counts + last-open time, the frecency ranking signal
    pub visits: HashMap<String, Visit>,
    /// known repos, most recently used first
    pub repos: Vec<String>,
    pub meta: HashMap<String, RepoMeta>,
}

#[derive(Clone, Copy, Serialize, Deserialize, Default)]
pub struct Visit {
    pub n: u32,
    pub last: u64, // unix seconds
}

pub fn epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// zoxide-style frecency: log visit count, boosted when recently opened.
pub fn frecency(v: Visit, now: u64) -> f64 {
    let age = now.saturating_sub(v.last);
    let mult = if age < 3600 {
        4.0
    } else if age < 86_400 {
        2.0
    } else if age < 604_800 {
        1.0
    } else {
        0.25
    };
    f64::from(v.n).ln_1p() * mult
}

pub fn list_key(repo: &str, kind: &str, state: &str) -> String {
    format!("{repo}:{kind}:{state}")
}

fn cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("github-tui/cache.json")
}

impl Cache {
    pub fn load() -> Self {
        std::fs::read_to_string(cache_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = cache_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(path, json);
        }
    }

    pub fn visit(&mut self, id: &str) {
        // ponytail: unbounded map (one small entry per distinct item); prune if it ever matters
        let v = self.visits.entry(id.to_string()).or_default();
        v.n += 1;
        v.last = epoch();
    }

    /// Move a repo to the front of the known-repos list.
    pub fn use_repo(&mut self, repo: &str) {
        self.repos.retain(|r| r != repo);
        self.repos.insert(0, repo.to_string());
    }

    /// Append repos we have not seen (existing order = recency, kept).
    pub fn add_repos(&mut self, repos: &[String]) {
        for r in repos {
            if !self.repos.contains(r) {
                self.repos.push(r.clone());
            }
        }
    }

    /// Upsert hits into the searchable pool (most recently touched first).
    pub fn remember(&mut self, hits: impl IntoIterator<Item = SearchHit>) {
        for h in hits {
            if h.repo.is_empty() || h.title.trim().is_empty() {
                continue;
            }
            let id = h.id();
            self.pool.retain(|p| p.id() != id);
            self.pool.insert(0, h);
        }
        self.pool.truncate(2000);
    }
}

pub struct Backend {
    tx: Sender<Msg>,
    inflight: HashSet<String>,
    pub pending: usize,
}

impl Backend {
    pub fn new(tx: Sender<Msg>) -> Self {
        Self { tx, inflight: HashSet::new(), pending: 0 }
    }

    fn spawn(&mut self, key: Option<String>, job: impl FnOnce() -> Msg + Send + 'static) {
        if let Some(k) = &key
            && !self.inflight.insert(k.clone())
        {
            return;
        }
        self.pending += 1;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    pub fn finish(&mut self, key: Option<&str>) {
        self.pending = self.pending.saturating_sub(1);
        if let Some(k) = key {
            self.inflight.remove(k);
        }
    }

    pub fn fetch_item(&mut self, repo: String, number: u64) {
        let key = ItemDoc::key(&repo, number);
        self.spawn(Some(format!("item:{key}")), move || {
            let res = fetch_item_sync(&repo, number);
            Msg::Item { key, res }
        });
    }

    pub fn fetch_list(&mut self, repo: String, kind: &str, state: &str, page: u32) {
        let key = list_key(&repo, kind, state);
        let (kind, state) = (kind.to_string(), state.to_string());
        self.spawn(Some(format!("list:{key}")), move || {
            let res = fetch_list_sync(&repo, &kind, &state, page);
            Msg::List { repo, key, page, res }
        });
    }

    pub fn search(&mut self, repo: String, query: String, seq: u64) {
        self.spawn(None, move || {
            let res = search_sync(&repo, &query);
            Msg::Search { seq, repo, query, res }
        });
    }

    pub fn save_body(&mut self, repo: String, number: u64, is_pr: bool, content: String) {
        let key = ItemDoc::key(&repo, number);
        self.spawn(Some(format!("save:{key}")), move || {
            let n = number.to_string();
            let res = run_gh(
                &[sub(is_pr), "edit", &n, "--repo", &repo, "--body-file", "-"],
                Some(&content),
            )
            .map(|_| ());
            Msg::Saved { key, content, res }
        });
    }

    /// Run gh commands in order, stopping at the first failure.
    pub fn edit(&mut self, key: String, cmds: Vec<Vec<String>>) {
        self.spawn(Some(format!("edit:{key}")), move || {
            let res = cmds.iter().try_for_each(|c| {
                let args: Vec<&str> = c.iter().map(String::as_str).collect();
                run_gh(&args, None).map(|_| ())
            });
            Msg::Edited { key, res }
        });
    }

    pub fn create_issue(&mut self, repo: String, title: String, body: String) {
        self.spawn(Some(format!("create:{repo}")), move || {
            let res = run_gh(
                &["issue", "create", "--repo", &repo, "--title", &title, "--body-file", "-"],
                Some(&body),
            )
            .and_then(|out| {
                // gh prints the new issue url
                out.trim()
                    .rsplit('/')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| anyhow!("unexpected gh output: {}", out.trim()))
            });
            Msg::Created { repo, res }
        });
    }

    pub fn comment(&mut self, repo: String, number: u64, is_pr: bool, body: String) {
        let key = ItemDoc::key(&repo, number);
        self.spawn(Some(format!("comment:{key}")), move || {
            let n = number.to_string();
            let res = run_gh(&[sub(is_pr), "comment", &n, "--repo", &repo, "--body-file", "-"], Some(&body))
                .map(|_| ());
            Msg::Commented { key, res }
        });
    }

    pub fn fetch_meta(&mut self, repo: String) {
        self.spawn(Some(format!("meta:{repo}")), move || {
            let res = fetch_meta_sync(&repo);
            Msg::Meta { repo, res }
        });
    }

    pub fn review(&mut self, repo: String, number: u64, kind: ReviewKind, body: String) {
        let key = ItemDoc::key(&repo, number);
        self.spawn(Some(format!("review:{key}")), move || {
            let res = run_gh(
                &["pr", "review", &number.to_string(), "--repo", &repo, kind.flag(), "--body-file", "-"],
                Some(&body),
            ).map(|_| ());
            Msg::Reviewed { key, res }
        });
    }

    pub fn fetch_repos(&mut self) {
        self.spawn(Some("repos".into()), move || {
            let res = api("user/repos?per_page=100&sort=pushed")
                .map(|v| names(&v, "full_name"));
            Msg::Repos { res }
        });
    }

    /// The PR for the branch checked out in the working directory, if any.
    pub fn fetch_cwd_pr(&mut self, repo: String) {
        self.spawn(Some("cwdpr".into()), move || {
            let res = match run_gh(&["pr", "view", "--json", "number,title"], None) {
                Ok(out) => serde_json::from_str::<Value>(&out)
                    .map_err(Into::into)
                    .map(|v| v["number"].as_u64().map(|n| (n, v["title"].as_str().unwrap_or("").to_string()))),
                Err(e) if e.to_string().contains("no pull requests found") => Ok(None),
                Err(e) => Err(e),
            };
            Msg::CwdPr { repo, res }
        });
    }

    pub fn checkout(&mut self, repo: String, number: u64) {
        let key = ItemDoc::key(&repo, number);
        self.spawn(Some(format!("checkout:{key}")), move || {
            let res = run_gh(&["pr", "checkout", &number.to_string(), "--repo", &repo], None).map(|_| ());
            Msg::Checkout { key, res }
        });
    }
}

fn sub(is_pr: bool) -> &'static str {
    if is_pr { "pr" } else { "issue" }
}

fn run_gh(args: &[&str], input: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("gh");
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().map_err(|e| anyhow!("failed to run gh: {e}"))?;
    if let Some(text) = input {
        child.stdin.take().expect("stdin piped").write_all(text.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("gh failed");
        return Err(anyhow!("{}", first.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn api(path: &str) -> Result<Value> {
    Ok(serde_json::from_str(&run_gh(&["api", path], None)?)?)
}

/// `field` of every object in a JSON array.
fn names(v: &Value, field: &str) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|o| o[field].as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// The remote's `owner/repo` for the working directory, if it is a GitHub checkout.
pub fn cwd_repo() -> Option<String> {
    let out = Command::new("git").args(["remote", "get-url", "origin"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_remote(String::from_utf8_lossy(&out.stdout).trim())
}

pub fn parse_remote(url: &str) -> Option<String> {
    let rest = url.split("github.com").nth(1)?;
    let rest = rest.trim_start_matches([':', '/']).trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn date(v: &Value) -> String {
    v.as_str().unwrap_or("").chars().take(10).collect()
}

fn login(v: &Value) -> String {
    v["login"].as_str().or_else(|| v["name"].as_str()).unwrap_or("ghost").to_string()
}

const ITEM_QUERY: &str = "query($owner:String!,$name:String!,$n:Int!){repository(owner:$owner,name:$name){issueOrPullRequest(number:$n){__typename \
... on Issue{number title body state url createdAt updatedAt author{login} labels(first:50){nodes{name}} assignees(first:20){nodes{login}} milestone{title} comments(first:100){nodes{author{login __typename} createdAt body}}} \
... on PullRequest{number title body state url createdAt updatedAt isDraft headRefName baseRefName reviewDecision author{login} labels(first:50){nodes{name}} assignees(first:20){nodes{login}} milestone{title} \
reviewRequests(first:20){totalCount nodes{requestedReviewer{... on User{login} ... on Team{name}}}} reviews(first:50){nodes{author{login __typename} state createdAt body}} comments(first:100){nodes{author{login __typename} createdAt body}}}}}}";

fn fetch_item_sync(repo: &str, number: u64) -> Result<ItemDoc> {
    let (owner, name) = repo.split_once('/').ok_or_else(|| anyhow!("bad repo {repo}"))?;
    let out = run_gh(
        &[
            "api", "graphql",
            "-F", &format!("owner={owner}"),
            "-F", &format!("name={name}"),
            "-F", &format!("n={number}"),
            "-f", &format!("query={ITEM_QUERY}"),
        ],
        None,
    )?;
    let v: Value = serde_json::from_str(&out)?;
    parse_item(&v, repo, number)
}

fn parse_item(v: &Value, repo: &str, number: u64) -> Result<ItemDoc> {
    let it = &v["data"]["repository"]["issueOrPullRequest"];
    if it.is_null() {
        return Err(anyhow!("{repo}#{number} not found"));
    }
    let nodes = |v: &Value| -> Vec<String> {
        v["nodes"].as_array().map(|a| a.iter().map(login).collect()).unwrap_or_default()
    };
    let mut comments: Vec<(String, Comment)> = Vec::new();
    for c in it["comments"]["nodes"].as_array().unwrap_or(&Vec::new()) {
        comments.push((
            c["createdAt"].as_str().unwrap_or("").into(),
            Comment {
                author: login(&c["author"]), is_bot: c["author"]["__typename"] == "Bot",
                date: date(&c["createdAt"]), kind: String::new(), body: c["body"].as_str().unwrap_or("").into(),
            },
        ));
    }
    for r in it["reviews"]["nodes"].as_array().unwrap_or(&Vec::new()) {
        let kind = r["state"].as_str().unwrap_or("").to_lowercase().replace('_', " ");
        let body = r["body"].as_str().unwrap_or("");
        if body.is_empty() && kind == "commented" {
            continue; // inline-only review, nothing to show at this level
        }
        comments.push((
            r["createdAt"].as_str().unwrap_or("").into(),
            Comment {
                author: login(&r["author"]), is_bot: r["author"]["__typename"] == "Bot",
                date: date(&r["createdAt"]), kind, body: body.into(),
            },
        ));
    }
    comments.sort_by(|a, b| a.0.cmp(&b.0));
    let is_pr = it["__typename"] == "PullRequest";
    Ok(ItemDoc {
        number,
        title: it["title"].as_str().unwrap_or("").into(),
        body: it["body"].as_str().unwrap_or("").into(),
        is_pr,
        state: it["state"].as_str().unwrap_or("").to_lowercase(),
        draft: it["isDraft"].as_bool().unwrap_or(false),
        author: login(&it["author"]),
        labels: nodes(&it["labels"]),
        assignees: nodes(&it["assignees"]),
        reviewers: it["reviewRequests"]["nodes"]
            .as_array()
            .map(|a| a.iter().map(|n| login(&n["requestedReviewer"])).collect())
            .unwrap_or_default(),
        review_status: if is_pr { review_status(it).into() } else { String::new() },
        milestone: it["milestone"]["title"].as_str().unwrap_or("").into(),
        head: it["headRefName"].as_str().unwrap_or("").into(),
        base: it["baseRefName"].as_str().unwrap_or("").into(),
        created: date(&it["createdAt"]),
        updated: date(&it["updatedAt"]),
        url: it["url"].as_str().unwrap_or("").into(),
        comments: comments.into_iter().map(|(_, c)| c).collect(),
    })
}

/// `kind` is the REST collection: `issues` (PRs filtered out) or `pulls`.
fn fetch_list_sync(repo: &str, kind: &str, state: &str, page: u32) -> Result<ListDoc> {
    let v = api(&format!("repos/{repo}/{kind}?state={state}&per_page={PAGE}&page={page}&sort=updated&direction=desc"))?;
    let arr = v.as_array().ok_or_else(|| anyhow!("unexpected response"))?;
    let mut rows: Vec<RowDoc> = arr
        .iter()
        .filter(|it| kind == "pulls" || it.get("pull_request").is_none())
        .map(|it| {
            let is_pr = kind == "pulls" || it.get("pull_request").is_some();
            let state = if it["merged_at"].is_string() {
                "merged".to_string()
            } else if it["draft"].as_bool().unwrap_or(false) {
                "draft".to_string()
            } else {
                it["state"].as_str().unwrap_or("").to_string()
            };
            RowDoc {
                number: it["number"].as_u64().unwrap_or(0),
                is_pr,
                review_status: String::new(),
                cells: vec![
                    format!("#{}", it["number"].as_u64().unwrap_or(0)),
                    it["title"].as_str().unwrap_or("").to_string(),
                    state,
                    login(&it["user"]),
                    names(&it["labels"], "name").join(", "),
                    names(&it["assignees"], "login").join(", "),
                    it["milestone"]["title"].as_str().unwrap_or("").to_string(),
                    date(&it["updated_at"]),
                ],
            }
        })
        .collect();
    if kind == "pulls" && !rows.is_empty() {
        fetch_review_statuses(repo, &mut rows)?;
    }
    Ok(ListDoc { rows, has_more: arr.len() == PAGE, page })
}

fn review_status(pr: &Value) -> &'static str {
    match pr["reviewDecision"].as_str() {
        Some("APPROVED") => "approved",
        Some("CHANGES_REQUESTED") => "changes requested",
        Some("REVIEW_REQUIRED") => "needs review",
        _ if pr["reviewRequests"]["totalCount"].as_u64().unwrap_or(0) > 0 => "needs review",
        _ => "—",
    }
}

/// Fetch review decisions for a whole REST page in one GraphQL request.
fn fetch_review_statuses(repo: &str, rows: &mut [RowDoc]) -> Result<()> {
    let (owner, name) = repo.split_once('/').ok_or_else(|| anyhow!("bad repo {repo}"))?;
    let fields = rows.iter().map(|r| format!(
        "pr{}:pullRequest(number:{}){{reviewDecision reviewRequests{{totalCount}}}}", r.number, r.number,
    )).collect::<Vec<_>>().join(" ");
    let query = format!("query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{fields}}}}}");
    let input = serde_json::json!({"query": query, "variables": {"owner": owner, "name": name}});
    let out = run_gh(&["api", "graphql", "--input", "-"], Some(&input.to_string()))?;
    let v: Value = serde_json::from_str(&out)?;
    apply_review_statuses(repo, rows, &v)
}

fn apply_review_statuses(repo: &str, rows: &mut [RowDoc], response: &Value) -> Result<()> {
    for row in rows {
        let pr = &response["data"]["repository"][format!("pr{}", row.number)];
        if pr.is_null() {
            return Err(anyhow!("review status missing for {repo}#{}", row.number));
        }
        row.review_status = review_status(pr).into();
    }
    Ok(())
}

fn search_sync(repo: &str, query: &str) -> Result<Vec<SearchHit>> {
    let q = format!("q=repo:{repo} {}", query.trim());
    let mut args = vec!["api", "-X", "GET", "search/issues", "-F", "per_page=30", "-f", &q];
    if query.trim().is_empty() {
        args.extend(["-f", "sort=updated"]);
    }
    let v: Value = serde_json::from_str(&run_gh(&args, None)?)?;
    Ok(v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|it| {
                    Some(SearchHit {
                        repo: it["repository_url"].as_str()?.rsplit('/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/"),
                        number: it["number"].as_u64()?,
                        title: it["title"].as_str()?.to_string(),
                        is_pr: it.get("pull_request").is_some(),
                        state: it["state"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn fetch_meta_sync(repo: &str) -> Result<RepoMeta> {
    Ok(RepoMeta {
        labels: names(&api(&format!("repos/{repo}/labels?per_page=100"))?, "name"),
        milestones: names(&api(&format!("repos/{repo}/milestones?state=open&per_page=100"))?, "title"),
        assignees: names(&api(&format!("repos/{repo}/assignees?per_page=100"))?, "login"),
    })
}

/// Item fields the metadata editor can change.
pub fn editable_fields(doc: &ItemDoc) -> Vec<(&'static str, String)> {
    let mut f = vec![
        ("title", doc.title.clone()),
        ("state", doc.state.clone()),
        ("labels", doc.labels.join(", ")),
        ("assignees", doc.assignees.join(", ")),
        ("milestone", doc.milestone.clone()),
    ];
    if doc.is_pr {
        f.push(("draft", if doc.draft { "☑" } else { "☐" }.into()));
        f.push(("reviewers", doc.reviewers.join(", ")));
        f.push(("base", doc.base.clone()));
    }
    f
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// gh invocations (argument vectors, without the leading `gh`) that change
/// `field` of `doc` to `value`. Empty when nothing would change.
pub fn edit_cmds(repo: &str, doc: &ItemDoc, field: &str, value: &str) -> Vec<Vec<String>> {
    let n = doc.number.to_string();
    let sub = sub(doc.is_pr);
    let cmd = |verb: &str, extra: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = [sub, verb, &n, "--repo", repo].iter().map(|s| s.to_string()).collect();
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    let value = value.trim();
    let diff = |cur: &[String], add: &str, rm: &str| -> Vec<Vec<String>> {
        let want = split_list(value);
        let mut extra: Vec<String> = Vec::new();
        let added: Vec<&str> = want.iter().filter(|w| !cur.contains(w)).map(String::as_str).collect();
        let removed: Vec<&str> = cur.iter().filter(|c| !want.contains(c)).map(String::as_str).collect();
        if !added.is_empty() {
            extra.extend([add.to_string(), added.join(",")]);
        }
        if !removed.is_empty() {
            extra.extend([rm.to_string(), removed.join(",")]);
        }
        if extra.is_empty() {
            return Vec::new();
        }
        let e: Vec<&str> = extra.iter().map(String::as_str).collect();
        vec![cmd("edit", &e)]
    };
    match field {
        "title" if !value.is_empty() && value != doc.title => vec![cmd("edit", &["--title", value])],
        "base" if !value.is_empty() && value != doc.base => vec![cmd("edit", &["--base", value])],
        "labels" => diff(&doc.labels, "--add-label", "--remove-label"),
        "assignees" => diff(&doc.assignees, "--add-assignee", "--remove-assignee"),
        "reviewers" => diff(&doc.reviewers, "--add-reviewer", "--remove-reviewer"),
        "milestone" if value != doc.milestone => {
            if value.is_empty() {
                vec![cmd("edit", &["--remove-milestone"])]
            } else {
                vec![cmd("edit", &["--milestone", value])]
            }
        }
        "state" if value != doc.state => match value {
            "open" => vec![cmd("reopen", &[])],
            "closed" => vec![cmd("close", &[])],
            _ => Vec::new(),
        },
        "draft" => match (value == "true", doc.draft) {
            (true, false) => vec![cmd("ready", &["--undo"])],
            (false, true) => vec![cmd("ready", &[])],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Reflect a saved field locally so a second edit diffs against the new value.
pub fn apply_field(doc: &mut ItemDoc, field: &str, value: &str) {
    let value = value.trim();
    match field {
        "title" => doc.title = value.into(),
        "base" => doc.base = value.into(),
        "labels" => doc.labels = split_list(value),
        "assignees" => doc.assignees = split_list(value),
        "reviewers" => doc.reviewers = split_list(value),
        "milestone" => doc.milestone = value.into(),
        "state" => doc.state = value.into(),
        "draft" => doc.draft = value == "true",
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_and_review_bot_identity_survives_cache_roundtrip() {
        let response = serde_json::json!({"data": {"repository": {"issueOrPullRequest": {
            "__typename": "PullRequest",
            "comments": {"nodes": [
                {"author": {"login": "review-app", "__typename": "Bot"}, "body": "Bot comment"},
                {"author": {"login": "robotics-dev", "__typename": "User"}, "body": "Human comment"},
                {"author": null, "body": "Deleted author"}
            ]},
            "reviews": {"nodes": [
                {"author": {"login": "review-app", "__typename": "Bot"}, "body": "Bot review", "state": "CHANGES_REQUESTED"}
            ]}
        }}}});
        let doc = parse_item(&response, "o/r", 7).unwrap();
        let cached: ItemDoc = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(cached.comments.len(), 4);
        assert!(cached.comments[0].is_bot);
        assert!(!cached.comments[1].is_bot);
        assert!(!cached.comments[2].is_bot);
        assert_eq!(cached.comments[2].author, "ghost");
        assert!(cached.comments[3].is_bot);
        assert_eq!(cached.comments[3].kind, "changes requested");

        let old_comment: Comment = serde_json::from_value(serde_json::json!({
            "author": "review-app[bot]", "date": "2026-09-05", "kind": "", "body": "Cached comment"
        })).unwrap();
        assert!(!old_comment.is_bot); // old caches still deserialize; the view checks the suffix
        assert_eq!(old_comment.author, "review-app[bot]");
    }

    #[test]
    fn review_decisions_and_requested_reviewers() {
        for (decision, requests, expected) in [
            (Some("APPROVED"), 2, "approved"),
            (Some("CHANGES_REQUESTED"), 1, "changes requested"),
            (Some("REVIEW_REQUIRED"), 0, "needs review"),
            (None, 1, "needs review"),
            (None, 0, "—"),
        ] {
            let pr = serde_json::json!({"reviewDecision": decision, "reviewRequests": {"totalCount": requests}});
            assert_eq!(review_status(&pr), expected);
        }
    }

    #[test]
    fn review_statuses_match_pr_numbers_and_reject_missing_results() {
        let mut rows = vec![7, 3].into_iter().map(|number| RowDoc {
            number, is_pr: true, cells: vec![], review_status: String::new(),
        }).collect::<Vec<_>>();
        let response = serde_json::json!({"data": {"repository": {
            "pr3": {"reviewDecision": "CHANGES_REQUESTED"},
            "pr7": {"reviewDecision": "APPROVED"}
        }}});
        apply_review_statuses("o/r", &mut rows, &response).unwrap();
        assert_eq!(rows[0].review_status, "approved");
        assert_eq!(rows[1].review_status, "changes requested");
        assert!(apply_review_statuses("o/r", &mut rows, &serde_json::json!({})).is_err());
    }

    #[test]
    fn old_cache_preserves_column_alignment() {
        let row: RowDoc = serde_json::from_value(serde_json::json!({
            "number": 7, "is_pr": true,
            "cells": ["#7", "Title", "open", "author", "bug", "alice", "v1", "2026-09-05"]
        })).unwrap();
        assert_eq!(row.cell("review"), "—");
        assert_eq!(row.cell("author"), "author");
        assert_eq!(row.cell("updated"), "2026-09-05");
        let mut old_item = serde_json::to_value(ItemDoc::default()).unwrap();
        old_item.as_object_mut().unwrap().remove("review_status");
        assert!(serde_json::from_value::<ItemDoc>(old_item).unwrap().review_status.is_empty());
    }

    #[test]
    fn remote_urls() {
        assert_eq!(parse_remote("git@github.com:qdrant/qdrant.git").as_deref(), Some("qdrant/qdrant"));
        assert_eq!(parse_remote("https://github.com/qdrant/qdrant").as_deref(), Some("qdrant/qdrant"));
        assert_eq!(parse_remote("https://github.com/qdrant/qdrant.git/").as_deref(), Some("qdrant/qdrant"));
        assert_eq!(parse_remote("ssh://git@github.com/a/b.git").as_deref(), Some("a/b"));
        assert_eq!(parse_remote("https://gitlab.com/a/b.git"), None);
        assert_eq!(parse_remote("git@github.com:onlyowner"), None);
    }

    #[test]
    fn remember_dedups_and_orders() {
        let mk = |n: u64, t: &str| SearchHit { repo: "o/r".into(), number: n, title: t.into(), is_pr: false, state: String::new() };
        let mut c = Cache::default();
        c.remember([mk(1, "Alpha"), mk(2, "Beta")]);
        c.remember([mk(1, "Alpha v2"), mk(3, "  ")]);
        assert_eq!(c.pool.len(), 2);
        assert_eq!(c.pool[0].title, "Alpha v2");
        assert_eq!(c.pool[1].number, 2);
    }

    #[test]
    fn edit_commands() {
        let doc = ItemDoc {
            number: 7,
            title: "T".into(),
            is_pr: true,
            state: "open".into(),
            labels: vec!["bug".into(), "p1".into()],
            ..Default::default()
        };
        let j = |c: Vec<Vec<String>>| c.iter().map(|v| v.join(" ")).collect::<Vec<_>>();
        assert_eq!(j(edit_cmds("o/r", &doc, "labels", "p1, docs")),
                   vec!["pr edit 7 --repo o/r --add-label docs --remove-label bug"]);
        assert!(edit_cmds("o/r", &doc, "labels", "bug,p1").is_empty()); // no change
        assert_eq!(j(edit_cmds("o/r", &doc, "state", "closed")), vec!["pr close 7 --repo o/r"]);
        assert!(edit_cmds("o/r", &doc, "state", "open").is_empty());
        assert_eq!(j(edit_cmds("o/r", &doc, "draft", "true")), vec!["pr ready 7 --repo o/r --undo"]);
        assert_eq!(j(edit_cmds("o/r", &doc, "milestone", "")), Vec::<String>::new()); // already none
        assert_eq!(j(edit_cmds("o/r", &doc, "milestone", "v1")), vec!["pr edit 7 --repo o/r --milestone v1"]);
        assert!(edit_cmds("o/r", &doc, "title", "  ").is_empty()); // empty rename refused
        let issue = ItemDoc { number: 3, ..Default::default() };
        assert_eq!(j(edit_cmds("o/r", &issue, "title", "New")), vec!["issue edit 3 --repo o/r --title New"]);
        let mut d = doc.clone();
        apply_field(&mut d, "labels", "x, y");
        assert_eq!(d.labels, vec!["x", "y"]);
    }
}
