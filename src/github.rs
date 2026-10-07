//! Talks to GitHub through the `gh` CLI, so the user's existing login is reused
//! and no token ever needs to be stored by the app.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::{self, Read},
    os::unix::{io::AsRawFd, process::CommandExt as _},
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    repository::{PUBLIC_HOST, RepositoryId, normalize_host},
    store::PendingReview,
};

// Enumerate watched repositories rather than global search: search has a
// 1,000-result ceiling and its dashboard qualifiers need not share API behavior.
const REPOSITORY_QUERY: &str = r#"
query($owner: String!, $name: String!, $after: String) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    nameWithOwner
    isArchived
    pullRequests(first: 100, after: $after, states: OPEN) {
      pageInfo { hasNextPage endCursor }
      nodes {
        id number title url isDraft state author { login }
        reviewRequests(first: 100) {
          pageInfo { hasNextPage endCursor }
          nodes { requestedReviewer { __typename ... on User { login } ... on Team { id } } }
        }
      }
    }
  }
}
"#;

const REQUESTS_QUERY: &str = r#"
query($id: ID!, $after: String) {
  viewer { login }
  node(id: $id) {
    ... on PullRequest {
      reviewRequests(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { requestedReviewer { __typename ... on User { login } ... on Team { id } } }
      }
    }
  }
}
"#;

const MEMBERS_QUERY: &str = r#"
query($id: ID!, $me: String!, $after: String) {
  viewer { login }
  node(id: $id) {
    ... on Team {
      members(first: 100, after: $after, query: $me, membership: ALL) {
        pageInfo { hasNextPage endCursor }
        nodes { login }
      }
    }
  }
}
"#;

const HISTORY_QUERY: &str = r#"
query($id: ID!, $after: String) {
  viewer { login }
  node(id: $id) {
    ... on PullRequest {
      timelineItems(first: 100, after: $after, itemTypes: [REVIEW_REQUESTED_EVENT, PULL_REQUEST_REVIEW]) {
        pageInfo { hasNextPage endCursor }
        nodes {
          __typename
          ... on ReviewRequestedEvent {
            createdAt
            requestedReviewer { __typename ... on User { login } ... on Team { id } }
          }
          ... on PullRequestReview { submittedAt state author { login } }
        }
      }
    }
  }
}
"#;

/// gh is the authority for configured hosts. Ask only for names, never tokens.
/// JSON mode keeps hosts with expired credentials in the list, allowing their
/// local clones and cached reviews to remain visible until authentication recovers.
/// Unlike text mode, JSON mode exits successfully even for authentication errors
/// (gh 2.81+: https://cli.github.com/manual/gh_auth_status).
pub fn known_hosts() -> Result<Vec<String>> {
    known_hosts_with(gh_request)
}

fn known_hosts_with(mut run: impl FnMut(&[&str]) -> Result<String>) -> Result<Vec<String>> {
    check_cli_version(&run(&["--version"])?)?;
    let output = run(&[
        "auth",
        "status",
        "--active",
        "--json",
        "hosts",
        "--jq",
        ".hosts | keys",
    ])
    .context("could not discover GitHub hosts; run `gh auth status`")?;
    parse_hosts(&output)
}

fn check_cli_version(output: &str) -> Result<()> {
    let version = output
        .strip_prefix("gh version ")
        .and_then(|s| s.split_whitespace().next())
        .context("could not determine GitHub CLI version; gh 2.81 or newer is required")?;
    let version = semver::Version::parse(version).context("unexpected GitHub CLI version")?;
    if version < semver::Version::new(2, 81, 0) {
        bail!("GitHub CLI {version} is too old; install gh 2.81 or newer for host discovery");
    }
    Ok(())
}

fn parse_hosts(output: &str) -> Result<Vec<String>> {
    let names: Vec<String> = serde_json::from_str(output).context("unexpected gh host list")?;
    let mut hosts = BTreeSet::from([PUBLIC_HOST.to_string()]);
    for name in names {
        let host = normalize_host(&name).with_context(|| {
            format!("unsupported GitHub host {name:?}; use a bare hostname with standard HTTPS")
        })?;
        hosts.insert(host);
    }
    Ok(hosts.into_iter().collect())
}

const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Readiness {
    #[default]
    Checking,
    Missing,
    NotRunnable,
    SignedOut,
    Offline,
    Unavailable,
    Ready(String),
}

#[cfg(test)]
pub struct Check {
    pub readiness: Readiness,
    pub reviews: Result<Vec<PendingReview>>,
}

/// Confirmed requests can enter the queue even when some team lookups fail.
/// Only complete repository snapshots may remove saved requests.
#[derive(Default)]
pub struct FetchedReviews {
    pub pending: Vec<PendingReview>,
    pub completed_repos: HashSet<RepositoryId>,
    pub errors: Vec<String>,
    pub readiness: BTreeMap<String, Readiness>,
    pub successful_hosts: BTreeSet<String>,
}

pub fn fetch_awaiting_reviews(repos: &HashSet<RepositoryId>) -> Result<FetchedReviews> {
    let mut active_host = String::new();
    let mut started = Instant::now();
    fetch_repositories_with(repos, |args| {
        let host = args.get(2).copied().unwrap_or(PUBLIC_HOST);
        if active_host != host {
            active_host = host.into();
            started = Instant::now();
        }
        let remaining = CHECK_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            bail!(
                "GitHub check timed out after 60 seconds. Cached reviews remain unverified; Refresh to retry."
            );
        }
        gh_output(
            args,
            command_output(
                Command::new(gh_binary()).args(args),
                remaining.min(Duration::from_secs(30)),
            )?,
        )
    })
}

fn viewer_login(host: &str, run: &mut impl FnMut(&[&str]) -> Result<String>) -> Result<String> {
    let login = run(&["api", "--hostname", host, "user", "--jq", ".login"]).with_context(|| {
        format!("could not read viewer; check `gh auth status --hostname {host}`")
    })?;
    let login = login.trim();
    if login.is_empty() {
        bail!("GitHub returned no account");
    }
    Ok(login.into())
}

fn fetch_repositories_with(
    repos: &HashSet<RepositoryId>,
    mut run: impl FnMut(&[&str]) -> Result<String>,
) -> Result<FetchedReviews> {
    let mut hosts: BTreeSet<_> = repos.iter().map(|repo| repo.host.clone()).collect();
    // Zero repositories still needs CLI/auth setup details, but cannot establish
    // that no reviews are waiting or advance the successful-sync timestamp.
    if hosts.is_empty() {
        hosts.insert(PUBLIC_HOST.into());
    }
    let mut fetched = FetchedReviews::default();
    for host in hosts {
        let me = match viewer_login(&host, &mut run) {
            Ok(me) => me,
            Err(err) => {
                fetched
                    .readiness
                    .insert(host.clone(), classify_failure(&err));
                fetched.errors.push(format!("{host}: {err:#}"));
                continue;
            }
        };
        let mut readiness = Readiness::Ready(me.clone());
        let host_repos: HashSet<_> = repos
            .iter()
            .filter(|repo| repo.host == host)
            .cloned()
            .collect();
        if host_repos.is_empty() {
            fetched.readiness.insert(host, readiness);
            continue;
        }
        let result = fetch_with(&host_repos, &me, &mut |query, variables| {
            if readiness != Readiness::Ready(me.clone()) {
                bail!("GitHub account changed during this check. Refresh to retry.");
            }
            let mut args = vec![
                "api".to_string(),
                "--hostname".into(),
                host.clone(),
                "graphql".into(),
                "-f".into(),
                format!("query={query}"),
            ];
            for (name, value) in variables {
                args.extend(["-f".into(), format!("{name}={value}")]);
            }
            let output = run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
            let response: GraphqlResponse =
                serde_json::from_str(&output).context("unexpected GitHub response")?;
            let viewer = response
                .data
                .as_ref()
                .and_then(|data| data.get("viewer"))
                .and_then(|viewer| viewer.get("login"))
                .and_then(serde_json::Value::as_str)
                .filter(|login| !login.is_empty());
            if let Some(viewer) = viewer {
                if !viewer.eq_ignore_ascii_case(&me) {
                    readiness = Readiness::Ready(viewer.into());
                    bail!("GitHub account changed during this check. Refresh to retry.");
                }
            } else if response.errors.is_empty() {
                bail!("GitHub response omitted the active account");
            }
            // Permission/schema errors explain absent viewer data. A valid
            // changed viewer still invalidates old identity even with errors.
            Ok(output)
        })?;
        // Any page under another identity invalidates this entire host result.
        if readiness == Readiness::Ready(me) {
            if result.errors.is_empty() && result.completed_repos == host_repos {
                fetched.successful_hosts.insert(host.clone());
            }
            fetched.pending.extend(result.pending);
            fetched.completed_repos.extend(result.completed_repos);
        }
        fetched.errors.extend(result.errors);
        fetched.readiness.insert(host, readiness);
    }
    Ok(fetched)
}

type Variables = Vec<(&'static str, String)>;
type Api<'a> = dyn FnMut(&str, Variables) -> Result<String> + 'a;
type Memberships = HashMap<(String, String), std::result::Result<bool, String>>;

fn fetch_with(
    repos: &HashSet<RepositoryId>,
    me: &str,
    api: &mut Api<'_>,
) -> Result<FetchedReviews> {
    let mut fetched = FetchedReviews::default();
    // Membership can change between polls; cache it only within this poll.
    let mut memberships = HashMap::new();
    let mut repos: Vec<_> = repos.iter().collect();
    repos.sort();
    for repo in repos {
        let mut errors = Vec::new();
        match fetch_repo(repo, me, &mut memberships, &mut errors, api) {
            Ok(pending) => {
                fetched.pending.extend(pending);
                if errors.is_empty() {
                    fetched.completed_repos.insert(repo.clone());
                }
            }
            Err(err) => errors.push(format!("{err:#}")),
        }
        fetched
            .errors
            .extend(errors.into_iter().map(|error| format!("{repo}: {error}")));
    }
    Ok(fetched)
}

fn fetch_repo(
    repo: &RepositoryId,
    me: &str,
    memberships: &mut Memberships,
    errors: &mut Vec<String>,
    api: &mut Api<'_>,
) -> Result<Vec<PendingReview>> {
    let (owner, name) = repo
        .slug
        .split_once('/')
        .context("invalid repository slug")?;
    let variables = vec![("owner", owner.into()), ("name", name.into())];
    let mut cursor = Cursor::default();
    let mut pending = HashMap::new();
    loop {
        let data: RepositoryData = request(api, REPOSITORY_QUERY, cursor.variables(&variables))?;
        let repository = data.repository.context("repository unavailable")?;
        if repository.is_archived {
            return Ok(Vec::new());
        }
        for mut pr in repository.pull_requests.nodes {
            if pr.state != "OPEN"
                || pr
                    .author
                    .as_ref()
                    .is_some_and(|a| a.login.eq_ignore_ascii_case(me))
            {
                continue;
            }
            let id = vec![("id", pr.id.clone())];
            let mut requests_cursor = Cursor::default();
            while requests_cursor.advance(&pr.review_requests.page_info)? {
                let data: NodeData<RequestsData> =
                    request(api, REQUESTS_QUERY, requests_cursor.variables(&id))?;
                let page = data
                    .node
                    .context("pull request unavailable")?
                    .review_requests;
                pr.review_requests.nodes.extend(page.nodes);
                pr.review_requests.page_info = page.page_info;
            }
            let mut relevant = HashSet::new();
            for review in &pr.review_requests.nodes {
                let reviewer = review
                    .requested_reviewer
                    .as_ref()
                    .context("requested reviewer unavailable")?;
                match reviewer {
                    Reviewer::User { login } if login.eq_ignore_ascii_case(me) => {
                        relevant.insert(reviewer.key().unwrap());
                    }
                    Reviewer::Team { id } => {
                        // An unreadable team does not negate a known direct or
                        // other team request. Report uncertainty so this repo
                        // cannot clear saved state, but keep checking known targets.
                        match memberships
                            .entry((repo.host.clone(), id.clone()))
                            .or_insert_with(|| {
                                is_member(id, me, api).map_err(|err| format!("{err:#}"))
                            }) {
                            Ok(true) => {
                                relevant.insert(reviewer.key().unwrap());
                            }
                            Ok(false) => {}
                            Err(err) => errors.push(format!("PR #{}: team {id}: {err}", pr.number)),
                        }
                    }
                    _ => {}
                }
            }
            if relevant.is_empty() {
                continue;
            }
            let mut history = Vec::new();
            let mut history_cursor = Cursor::default();
            loop {
                let data: NodeData<HistoryData> =
                    request(api, HISTORY_QUERY, history_cursor.variables(&id))?;
                let page = data
                    .node
                    .context("pull request unavailable")?
                    .timeline_items;
                history.extend(page.nodes);
                if !history_cursor.advance(&page.page_info)? {
                    break;
                }
            }
            if let Some(review) = pr.into_pending(
                &repo.host,
                &repository.name_with_owner,
                me,
                &relevant,
                history,
            )? {
                pending.insert(review.key(), review);
            }
        }
        if !cursor.advance(&repository.pull_requests.page_info)? {
            break;
        }
    }
    let mut pending: Vec<_> = pending.into_values().collect();
    pending.sort_by_key(PendingReview::key);
    Ok(pending)
}

fn is_member(id: &str, me: &str, api: &mut Api<'_>) -> Result<bool> {
    let variables = vec![("id", id.into()), ("me", me.into())];
    let mut cursor = Cursor::default();
    loop {
        let data: NodeData<MembersData> =
            request(api, MEMBERS_QUERY, cursor.variables(&variables))?;
        let page = data.node.context("team membership unavailable")?.members;
        if page
            .nodes
            .iter()
            .any(|member| member.login.eq_ignore_ascii_case(me))
        {
            return Ok(true);
        }
        if !cursor.advance(&page.page_info)? {
            return Ok(false);
        }
    }
}

impl Pr {
    fn into_pending(
        self,
        host: &str,
        repo: &str,
        me: &str,
        relevant: &HashSet<String>,
        history: Vec<HistoryItem>,
    ) -> Result<Option<PendingReview>> {
        let mut requests = HashMap::new();
        let mut last_review = None;
        for item in history {
            match item {
                HistoryItem::ReviewRequestedEvent {
                    created_at,
                    requested_reviewer,
                } => {
                    if let Some(key) = requested_reviewer.and_then(|reviewer| reviewer.key())
                        && relevant.contains(&key)
                    {
                        let date = timestamp(&created_at)?;
                        let previous = requests.entry(key).or_insert(date);
                        *previous = (*previous).max(date);
                    }
                }
                HistoryItem::PullRequestReview {
                    submitted_at,
                    state,
                    author,
                } => {
                    if state != "PENDING"
                        && author.is_some_and(|a| a.login.eq_ignore_ascii_case(me))
                    {
                        let date = timestamp(
                            &submitted_at.context("submitted review timestamp unavailable")?,
                        )?;
                        last_review = Some(
                            last_review.map_or(date, |previous: DateTime<Utc>| previous.max(date)),
                        );
                    }
                }
            }
        }
        // Never infer completion or freshness from a truncated/inaccessible history.
        if requests.len() != relevant.len() {
            bail!("review request history incomplete for PR #{}", self.number);
        }
        let requested_at = requests
            .into_values()
            .max()
            .context("no relevant request timestamp")?;
        // A timestamp tie cannot prove that the review satisfied the latest
        // request. Preserve the existing conservative behavior: keep it visible.
        if last_review.is_some_and(|reviewed| reviewed > requested_at) {
            return Ok(None);
        }
        Ok(Some(PendingReview {
            host: host.into(),
            repo: repo.into(),
            number: self.number,
            title: self.title,
            url: self.url,
            author: self
                .author
                .map(|a| a.login)
                .unwrap_or_else(|| "ghost".into()),
            is_draft: self.is_draft,
            rereview: last_review.is_some(),
            requested_at: Some(requested_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
        }))
    }
}

fn timestamp(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .context("invalid GitHub timestamp")?
        .with_timezone(&Utc))
}

// Reject GraphQL partial data even when `gh` exits successfully. Strict node
// decoding rejects null connection entries instead of silently dropping them.
fn request<T: DeserializeOwned>(api: &mut Api<'_>, query: &str, variables: Variables) -> Result<T> {
    let output = api(query, variables)?;
    let response: GraphqlResponse =
        serde_json::from_str(&output).context("unexpected GitHub response")?;
    if !response.errors.is_empty() {
        bail!(
            "GitHub GraphQL: {}",
            response
                .errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    // Read errors before decoding typed data: partial fields can be missing
    // precisely because GitHub denied access, and the error explains why.
    serde_json::from_value(response.data.context("GitHub returned no data")?)
        .context("incomplete GitHub response")
}

#[derive(Default)]
struct Cursor {
    after: Option<String>,
    seen: HashSet<String>,
}

impl Cursor {
    fn variables(&self, base: &Variables) -> Variables {
        let mut variables = base.clone();
        if let Some(after) = &self.after {
            variables.push(("after", after.clone()));
        }
        variables
    }

    fn advance(&mut self, page: &PageInfo) -> Result<bool> {
        if !page.has_next_page {
            return Ok(false);
        }
        let cursor = page
            .end_cursor
            .clone()
            .filter(|cursor| !cursor.is_empty())
            .context("missing pagination cursor")?;
        if !self.seen.insert(cursor.clone()) {
            bail!("GitHub pagination cursor repeated");
        }
        self.after = Some(cursor);
        Ok(true)
    }
}

pub fn gh(args: &[&str]) -> Result<String> {
    let timeout = if args.starts_with(&["release", "download"]) {
        Duration::from_secs(10 * 60)
    } else {
        Duration::from_secs(30)
    };
    let output = command_output(Command::new(gh_binary()).args(args), timeout)?;
    gh_output(args, output)
}

/// A stalled CLI request must not prevent the remaining hosts from polling.
/// Release downloads use `gh` above because large assets can take longer.
fn gh_request(args: &[&str]) -> Result<String> {
    let mut command = Command::new(gh_binary());
    command.args(args);
    let output = command_output(&mut command, Duration::from_secs(30))
        .with_context(|| format!("gh {} request failed", args.first().unwrap_or(&"")))?;
    gh_output(args, output)
}

fn gh_output(args: &[&str], output: Output) -> Result<String> {
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Drain both pipes without blocking: GraphQL responses can exceed a pipe buffer,
/// and descendants can keep a pipe open after the direct child exits. One deadline
/// covers both process execution and pipe reads, with no reader threads to join.
fn command_output(command: &mut Command, timeout: Duration) -> Result<Output> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .process_group(0)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env_remove("GH_DEBUG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run `gh`; is the GitHub CLI installed?")?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let result = (|| {
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (mut out_closed, mut err_closed) = (false, false);
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                bail!("GitHub CLI timed out after {} seconds", timeout.as_secs());
            }
            if !out_closed {
                out_closed = drain_pipe(&mut stdout, &mut out)?;
            }
            if !err_closed {
                err_closed = drain_pipe(&mut stderr, &mut err)?;
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if out_closed
                && err_closed
                && let Some(status) = status
            {
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    if result.is_err() {
        terminate_request(&mut child);
    }
    result
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: the pipe owns this live descriptor throughout both fcntl calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Limit each drain so continuous output cannot starve stderr or the deadline.
fn drain_pipe(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    for _ in 0..16 {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn terminate_request(child: &mut Child) {
    // SAFETY: process_group(0) gives this request its own group whose id is the
    // child's pid; the negative id targets only it and its descendants.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    // Also kill the direct child if it changed groups, and reap it on every error.
    child.kill().ok();
    child.wait().ok();
}

pub fn classify_failure(err: &anyhow::Error) -> Readiness {
    if err
        .downcast_ref::<std::io::Error>()
        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
    {
        return Readiness::Missing;
    }
    if err
        .chain()
        .any(|cause| cause.to_string().starts_with("could not run `gh`"))
    {
        return Readiness::NotRunnable;
    }
    let message = format!("{err:#}").to_lowercase();
    if message.contains("http 401")
        || message.contains("gh auth login")
        || message.contains("authentication token")
    {
        Readiness::SignedOut
    } else if is_connection_error(err) {
        Readiness::Offline
    } else {
        Readiness::Unavailable
    }
}

/// `gh` reports transport failures as text; only recognizable transport
/// diagnostics establish unreachable status. Other failures remain generic.
pub fn is_connection_error(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}").to_lowercase();
    [
        "network is unreachable",
        "no such host",
        "could not resolve host",
        "dial tcp",
        "connection refused",
        "connection reset",
        "tls handshake timeout",
        "i/o timeout",
        "error connecting to api.github.com",
        "check your internet connection",
    ]
    .iter()
    .any(|hint| message.contains(hint))
}

/// Apps launched from Finder get a bare PATH, so look in the usual places too.
fn gh_binary() -> &'static str {
    ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"]
        .into_iter()
        .find(|path| Path::new(path).exists())
        .unwrap_or("gh")
}

#[derive(Deserialize)]
struct GraphqlResponse {
    data: Option<serde_json::Value>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Deserialize)]
struct RepositoryData {
    repository: Option<Repository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    name_with_owner: String,
    is_archived: bool,
    pull_requests: Connection<Pr>,
}

#[derive(Deserialize)]
struct NodeData<T> {
    node: Option<T>,
}

#[derive(Deserialize)]
struct Connection<T> {
    nodes: Vec<T>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pr {
    id: String,
    number: u64,
    title: String,
    url: String,
    is_draft: bool,
    state: String,
    author: Option<Login>,
    review_requests: Connection<ReviewRequest>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequest {
    requested_reviewer: Option<Reviewer>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum Reviewer {
    User {
        login: String,
    },
    Team {
        id: String,
    },
    #[serde(other)]
    Other,
}

impl Reviewer {
    fn key(&self) -> Option<String> {
        match self {
            Self::User { login } => Some(format!("user:{}", login.to_lowercase())),
            Self::Team { id } => Some(format!("team:{id}")),
            Self::Other => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestsData {
    review_requests: Connection<ReviewRequest>,
}

#[derive(Deserialize)]
struct MembersData {
    members: Connection<Login>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryData {
    timeline_items: Connection<HistoryItem>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum HistoryItem {
    #[serde(rename_all = "camelCase")]
    ReviewRequestedEvent {
        created_at: String,
        requested_reviewer: Option<Reviewer>,
    },
    #[serde(rename_all = "camelCase")]
    PullRequestReview {
        submitted_at: Option<String>,
        state: String,
        author: Option<Login>,
    },
}

#[cfg(test)]
mod tests;
