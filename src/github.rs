//! Talks to GitHub through the `gh` CLI, so the user's existing login is reused
//! and no token ever needs to be stored by the app.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    os::unix::{io::AsRawFd, process::CommandExt as _},
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::{
    repository::{PUBLIC_HOST, normalize_host},
    store::PendingReview,
};

/// Search for open PRs, by someone else, where the viewer (or one of their
/// teams) is a requested reviewer. GitHub drops the viewer from the request
/// list once they review, so a PR leaves this search when the review lands.
const QUERY: &str = r#"
query($q: String!, $me: String!) {
  viewer { login }
  search(query: $q, type: ISSUE, first: 100) {
    nodes {
      ... on PullRequest {
        id
        number
        title
        url
        isDraft
        author { login }
        repository { nameWithOwner }
        reviewRequests(first: 50) {
          nodes { requestedReviewer { __typename ... on User { login } } }
        }
        reviews(last: 1, author: $me) { nodes { submittedAt } }
        timelineItems(last: 20, itemTypes: [REVIEW_REQUESTED_EVENT]) {
          pageInfo { hasPreviousPage startCursor }
          nodes {
            ... on ReviewRequestedEvent {
              createdAt
              requestedReviewer { __typename ... on User { login } }
            }
          }
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

/// Walk back only when the latest page omits the viewer's request. A bounded
/// number of pages and an overall deadline keep pathological histories finite.
const MAX_HISTORY_PAGES: usize = 20;
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const HISTORY_QUERY: &str = r#"
query($id: ID!, $before: String) {
  viewer { login }
  node(id: $id) {
    ... on PullRequest {
      timelineItems(last: 100, before: $before, itemTypes: [REVIEW_REQUESTED_EVENT]) {
        pageInfo { hasPreviousPage startCursor }
        nodes {
          ... on ReviewRequestedEvent {
            createdAt
            requestedReviewer { __typename ... on User { login } }
          }
        }
      }
    }
  }
}
"#;

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

pub struct Check {
    pub readiness: Readiness,
    pub reviews: Result<Vec<PendingReview>>,
}

/// Reviews and readiness are isolated by host. `gh` remains responsible for
/// credentials, including environment-token precedence.
#[derive(Debug, Default)]
pub struct FetchResults {
    pub reviews: Vec<PendingReview>,
    pub successful_hosts: BTreeSet<String>,
    pub errors: Vec<String>,
    pub readiness: BTreeMap<String, Readiness>,
}

/// Poll only hosts with enabled local repositories. Each failure is isolated.
pub fn fetch_awaiting_reviews(hosts: &BTreeSet<String>) -> FetchResults {
    if hosts.is_empty() {
        return probe_readiness_with(PUBLIC_HOST, gh_request);
    }
    fetch_hosts_with(hosts, check_host)
}

fn probe_readiness_with(
    host: &str,
    mut run: impl FnMut(&[&str]) -> Result<String>,
) -> FetchResults {
    let mut results = FetchResults::default();
    let readiness = match run(&["api", "--hostname", host, "user", "--jq", ".login"]) {
        Ok(login) if !login.trim().is_empty() => Readiness::Ready(login.trim().into()),
        result => {
            let error = result
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("GitHub returned no account"));
            results.errors.push(format!("{host}: {error:#}"));
            classify_failure(&error)
        }
    };
    results.readiness.insert(host.to_string(), readiness);
    results
}

fn fetch_hosts_with(
    hosts: &BTreeSet<String>,
    mut fetch: impl FnMut(&str) -> Check,
) -> FetchResults {
    let mut results = FetchResults::default();
    for host in hosts {
        let check = fetch(host);
        results.readiness.insert(host.clone(), check.readiness);
        match check.reviews {
            Ok(reviews) => {
                results.successful_hosts.insert(host.clone());
                results.reviews.extend(reviews);
            }
            Err(err) => results.errors.push(format!("{host}: {err:#}")),
        }
    }
    results
}

fn check_host(host: &str) -> Check {
    let started = Instant::now();
    check_host_with(host, |args| {
        let remaining = CHECK_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            bail!(
                "GitHub check timed out after 60 seconds. Cached reviews remain unverified; Refresh to retry."
            );
        }
        run_command(
            Command::new(gh_binary()).args(args),
            remaining.min(Duration::from_secs(30)),
        )
    })
}

#[cfg(test)]
fn check_with(run: impl FnMut(&[&str]) -> Result<String>) -> Check {
    check_host_with(PUBLIC_HOST, run)
}

fn check_host_with(host: &str, mut run: impl FnMut(&[&str]) -> Result<String>) -> Check {
    let login = run(&["api", "--hostname", host, "user", "--jq", ".login"]).with_context(|| {
        format!("could not read viewer; check `gh auth status --hostname {host}`")
    });
    let me = match login {
        Ok(login) if !login.trim().is_empty() => login.trim().to_string(),
        result => {
            let err = result
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("GitHub returned no account"));
            let readiness = classify_failure(&err);
            return Check {
                readiness,
                reviews: Err(err),
            };
        }
    };
    let search = "is:pr is:open archived:false review-requested:@me -author:@me";
    let output = run(&[
        "api",
        "--hostname",
        host,
        "graphql",
        "-f",
        &format!("query={QUERY}"),
        "-f",
        &format!("q={search}"),
        "-f",
        &format!("me={me}"),
    ]);
    let mut readiness = Readiness::Ready(me.clone());
    let reviews = output.and_then(|output| {
        let response: Response =
            serde_json::from_str(&output).context("unexpected GitHub response")?;
        check_graphql_errors(&response.errors)?;
        let data = response.data.context("GitHub GraphQL response contained no data")?;
        if !data.viewer.login.eq_ignore_ascii_case(&me) {
            readiness = Readiness::Ready(data.viewer.login);
            bail!("GitHub account changed during the check. Refresh to check the active account.");
        }
        let mut pending = Vec::new();
        let mut pages = 0;
        for mut pr in data.search.nodes.into_iter().flatten() {
            while pr.needs_request_history(&me) {
                if pages >= MAX_HISTORY_PAGES {
                    bail!("Review request history exceeded this check's pagination limit. Cached reviews remain unverified; Refresh to retry.");
                }
                let cursor = pr.timeline_items.page_info.start_cursor.as_deref()
                    .context("GitHub omitted the review history cursor")?;
                let output = run(&[
                    "api", "--hostname", host, "graphql",
                    "-f", &format!("query={HISTORY_QUERY}"),
                    "-f", &format!("id={}", pr.id),
                    "-f", &format!("before={cursor}"),
                ])?;
                pages += 1;
                let response: HistoryResponse = serde_json::from_str(&output)
                    .context("unexpected GitHub review history response")?;
                check_graphql_errors(&response.errors)?;
                let data = response.data.context("GitHub GraphQL history response contained no data")?;
                if !data.viewer.login.eq_ignore_ascii_case(&me) {
                    readiness = Readiness::Ready(data.viewer.login);
                    bail!("GitHub account changed during the check. Refresh to check the active account.");
                }
                let history = data.node
                    .context("GitHub could not read the pull request history")?.timeline_items;
                if history.page_info.has_previous_page
                    && history.page_info.start_cursor.as_deref() == Some(cursor) {
                    bail!("GitHub review history pagination did not advance. Refresh to retry.");
                }
                pr.timeline_items.nodes.extend(history.nodes);
                pr.timeline_items.page_info = history.page_info;
            }
            if let Some(pr) = pr.into_pending(host, &me) {
                pending.push(pr);
            }
        }
        Ok(pending)
    });
    if let Err(err) = &reviews {
        let failure = classify_failure(err);
        if matches!(
            failure,
            Readiness::Missing | Readiness::NotRunnable | Readiness::SignedOut
        ) {
            readiness = failure;
        }
    }
    Check { readiness, reviews }
}

fn check_graphql_errors(errors: &[GraphQlError]) -> Result<()> {
    if !errors.is_empty() {
        bail!(
            "GitHub GraphQL error: {}",
            errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(())
}

#[cfg(test)]
fn fetch_host_with(
    host: &str,
    run: impl FnMut(&[&str]) -> Result<String>,
) -> Result<Vec<PendingReview>> {
    check_host_with(host, run).reviews
}
#[cfg(test)]
fn parse_reviews(output: &str, host: &str, me: &str) -> Result<Vec<PendingReview>> {
    check_host_with(host, |args| {
        if args[3] == "user" {
            Ok(me.into())
        } else {
            Ok(output.into())
        }
    })
    .reviews
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

fn concerns_me(reviewer: &Option<Reviewer>, me: &str) -> bool {
    match reviewer {
        Some(Reviewer::User { login }) => login.eq_ignore_ascii_case(me),
        Some(Reviewer::Team) => true,
        _ => false,
    }
}

impl Pr {
    fn latest_request(&self, me: &str) -> Option<&str> {
        self.timeline_items
            .nodes
            .iter()
            .flatten()
            .filter(|event| concerns_me(&event.requested_reviewer, me))
            .filter_map(|event| event.created_at.as_deref())
            .max()
    }

    fn needs_request_history(&self, me: &str) -> bool {
        self.timeline_items.page_info.has_previous_page
            && self.latest_request(me).is_none()
            && !self
                .author
                .as_ref()
                .is_some_and(|author| author.login.eq_ignore_ascii_case(me))
            && self
                .review_requests
                .nodes
                .iter()
                .any(|request| concerns_me(&request.requested_reviewer, me))
    }

    fn into_pending(self, host: &str, me: &str) -> Option<PendingReview> {
        let requested_at = self.latest_request(me).map(str::to_owned);
        let author = self
            .author
            .map(|a| a.login)
            .unwrap_or_else(|| "ghost".into());
        if author.eq_ignore_ascii_case(me) {
            return None;
        }
        // A request counts when it names the viewer or a team (the search
        // already guarantees the viewer belongs to it).
        if !self
            .review_requests
            .nodes
            .iter()
            .any(|r| concerns_me(&r.requested_reviewer, me))
        {
            return None;
        }
        let last_review = self
            .reviews
            .nodes
            .into_iter()
            .flatten()
            .filter_map(|r| r.submitted_at)
            .max();
        // A team request survives a member's review, so check the dates too.
        // GitHub timestamps share one format, so string order is time order.
        if let (Some(requested), Some(reviewed)) = (&requested_at, &last_review)
            && reviewed > requested
        {
            return None;
        }
        Some(PendingReview {
            host: host.to_ascii_lowercase(),
            repo: self.repository.name_with_owner,
            number: self.number,
            title: self.title,
            url: self.url,
            author,
            is_draft: self.is_draft,
            rereview: last_review.is_some(),
            requested_at,
        })
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

fn run_command(command: &mut Command, timeout: Duration) -> Result<String> {
    gh_output(&[], command_output(command, timeout)?)
}

/// Apps launched from Finder get a bare PATH, so look in the usual places too.
fn gh_binary() -> &'static str {
    ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"]
        .into_iter()
        .find(|path| Path::new(path).exists())
        .unwrap_or("gh")
}

#[derive(Deserialize)]
struct Response {
    data: Option<Data>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}
#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct Data {
    viewer: Login,
    search: Nodes<Option<Pr>>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pr {
    id: String,
    number: u64,
    title: String,
    url: String,
    is_draft: bool,
    author: Option<Login>,
    repository: Repository,
    review_requests: Nodes<ReviewRequest>,
    reviews: Nodes<Option<Review>>,
    timeline_items: Timeline,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Timeline {
    nodes: Vec<Option<RequestEvent>>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_previous_page: bool,
    start_cursor: Option<String>,
}

#[derive(Deserialize)]
struct HistoryResponse {
    data: Option<HistoryData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct HistoryData {
    viewer: Login,
    node: Option<HistoryPr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryPr {
    timeline_items: Timeline,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    name_with_owner: String,
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
    Team,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Review {
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestEvent {
    created_at: Option<String>,
    requested_reviewer: Option<Reviewer>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn fake_gh(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gh");
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path)
    }

    fn probe(path: &Path) -> Check {
        check_with(|args| run_command(Command::new(path).args(args), Duration::from_secs(5)))
    }

    fn busy_pr() -> serde_json::Value {
        serde_json::json!({
            "id": "PR_busy", "number": 1, "title": "Review", "url": "https://github.com/o/r/pull/1",
            "isDraft": false, "author": {"login": "other"}, "repository": {"nameWithOwner": "o/r"},
            "reviewRequests": {"nodes": [{"requestedReviewer": {"__typename": "User", "login": "alice"}}]},
            "reviews": {"nodes": []},
            "timelineItems": {
                "pageInfo": {"hasPreviousPage": true, "startCursor": "recent-page"},
                "nodes": (0..20).map(|_| serde_json::json!({
                    "createdAt": "2026-01-03T00:00:00Z",
                    "requestedReviewer": {"__typename": "User", "login": "someone-else"}
                })).collect::<Vec<_>>()
            }
        })
    }

    #[test]
    fn request_outside_latest_twenty_events_ends_old_snooze_and_queues_alert() {
        let mut store = crate::store::Store::default();
        store.activate_account("alice");
        let mut previous = busy_pr();
        previous["timelineItems"]["nodes"] = serde_json::json!([{
            "createdAt": "2026-01-01T00:00:00Z",
            "requestedReviewer": {"__typename": "User", "login": "alice"}
        }]);
        store.reconcile(vec![
            serde_json::from_value::<Pr>(previous)
                .unwrap()
                .into_pending(PUBLIC_HOST, "alice")
                .unwrap(),
        ]);
        store.snooze(
            &(crate::repository::RepositoryId::new(PUBLIC_HOST, "o/r"), 1),
            30,
            1_000,
        );
        let check = check_with(|args| {
            if args[3] == "user" {
                return Ok("alice".into());
            }
            if args.iter().any(|arg| arg.starts_with("q=")) {
                Ok(serde_json::json!({"data": {"viewer": {"login": "alice"}, "search": {"nodes": [busy_pr()]}}}).to_string())
            } else {
                Ok(serde_json::json!({"data": {"viewer": {"login": "alice"}, "node": {"timelineItems": {
                    "pageInfo": {"hasPreviousPage": false, "startCursor": "older-page"},
                    "nodes": [{"createdAt": "2026-01-02T00:00:00Z", "requestedReviewer": {"__typename": "User", "login": "alice"}}]
                }}}}).to_string())
            }
        });
        let result = store.reconcile(check.reviews.unwrap());
        assert_eq!(result.fresh.len(), 1, "the newer request must notify");
        assert!(store.snoozed.is_empty(), "the older snooze must end");
        assert_eq!(store.notifications_due().len(), 1);
        assert_eq!(
            store.pending[0].requested_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
    }

    #[test]
    fn history_walk_uses_previous_cursors_and_stops_at_the_latest_matching_request() {
        let mut calls = 0;
        let check = check_with(|args| {
            calls += 1;
            if args[3] == "user" {
                return Ok("alice".into());
            }
            if args.iter().any(|arg| arg.starts_with("q=")) {
                return Ok(serde_json::json!({"data": {"viewer": {"login": "alice"}, "search": {"nodes": [busy_pr()]}}}).to_string());
            }
            assert_eq!(&args[..3], &["api", "--hostname", "github.com"]);
            assert!(args.contains(&"id=PR_busy"));
            let (cursor, next_cursor, login) = if calls == 3 {
                ("before=recent-page", "older-page", "other")
            } else {
                ("before=older-page", "oldest-page", "alice")
            };
            assert!(args.contains(&cursor));
            Ok(serde_json::json!({"data": {"viewer": {"login": "alice"}, "node": {"timelineItems": {
                "pageInfo": {"hasPreviousPage": true, "startCursor": next_cursor},
                "nodes": [{"createdAt": "2026-01-02T00:00:00Z", "requestedReviewer": {"__typename": "User", "login": login}}]
            }}}}).to_string())
        });
        assert_eq!(
            check.reviews.unwrap()[0].requested_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
        assert_eq!(calls, 4, "stop even if still older pages exist");
    }

    #[test]
    fn latest_matching_request_does_not_fetch_older_pages() {
        let mut calls = 0;
        let check = check_with(|args| {
            calls += 1;
            if args[3] == "user" {
                return Ok("alice".into());
            }
            let mut pr = busy_pr();
            pr["timelineItems"]["nodes"][0]["requestedReviewer"]["login"] = "alice".into();
            Ok(serde_json::json!({"data": {"viewer": {"login": "alice"}, "search": {"nodes": [pr]}}}).to_string())
        });
        assert_eq!(check.reviews.unwrap().len(), 1);
        assert_eq!(calls, 2);
    }

    #[test]
    fn incomplete_history_fails_sync_instead_of_reconciling_ambiguous_requests() {
        for failure in ["offline", "limit", "stuck-cursor", "missing-cursor"] {
            let mut history_calls = 0;
            let check = check_with(|args| {
                if args[3] == "user" {
                    return Ok("alice".into());
                }
                if args.iter().any(|arg| arg.starts_with("q=")) {
                    let mut pr = busy_pr();
                    if failure == "missing-cursor" {
                        pr["timelineItems"]["pageInfo"]["startCursor"] = serde_json::Value::Null;
                    }
                    return Ok(serde_json::json!({"data": {"viewer": {"login": "alice"}, "search": {"nodes": [pr]}}}).to_string());
                }
                history_calls += 1;
                if failure == "offline" {
                    bail!("dial tcp: network is unreachable");
                }
                let cursor = if failure == "stuck-cursor" {
                    "recent-page".into()
                } else {
                    format!("page-{history_calls}")
                };
                Ok(serde_json::json!({"data": {"viewer": {"login": "alice"}, "node": {"timelineItems": {
                    "pageInfo": {"hasPreviousPage": true, "startCursor": cursor}, "nodes": []
                }}}}).to_string())
            });
            assert!(check.reviews.is_err(), "{failure}");
            if failure == "limit" {
                assert_eq!(history_calls, MAX_HISTORY_PAGES);
            }
            let mut store = crate::store::Store::default();
            store.activate_account("alice");
            store.last_successful_sync = Some(100);
            let previous = serde_json::from_value::<Pr>(busy_pr())
                .unwrap()
                .into_pending(PUBLIC_HOST, "alice")
                .unwrap();
            store.reconcile(vec![previous.clone()]);
            store.snooze(&previous.key(), 30, 1_000);
            let mut health = crate::health::SyncHealth::default();
            let (fetched, changed) = health.apply(check, &mut store, 200);
            assert!(fetched.is_none());
            assert!(!changed);
            assert_eq!(store.pending, vec![previous]);
            assert_eq!(store.snoozed.len(), 1);
            assert_eq!(store.last_successful_sync, Some(100));
            assert!(health.error.is_some());
        }
    }

    #[test]
    fn account_switch_during_history_walk_discards_the_whole_check() {
        let check = check_with(|args| {
            if args[3] == "user" {
                return Ok("alice".into());
            }
            if args.iter().any(|arg| arg.starts_with("q=")) {
                Ok(serde_json::json!({"data": {"viewer": {"login": "alice"}, "search": {"nodes": [busy_pr()]}}}).to_string())
            } else {
                Ok(serde_json::json!({"data": {"viewer": {"login": "bob"}, "node": {"timelineItems": {
                    "pageInfo": {"hasPreviousPage": false, "startCursor": null}, "nodes": []
                }}}}).to_string())
            }
        });
        assert_eq!(check.readiness, Readiness::Ready("bob".into()));
        assert!(
            check
                .reviews
                .unwrap_err()
                .to_string()
                .contains("account changed")
        );
    }

    #[test]
    fn missing_signed_out_and_offline_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let missing = probe(&dir.path().join("missing-gh"));
        assert_eq!(missing.readiness, Readiness::Missing);
        assert!(missing.reviews.is_err());
        let (_dir, blocked) = fake_gh("exit 0");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(probe(&blocked).readiness, Readiness::NotRunnable);
        let (_dir, signed_out) =
            fake_gh("echo 'To get started with GitHub CLI, please run: gh auth login' >&2\nexit 4");
        let check = probe(&signed_out);
        assert_eq!(check.readiness, Readiness::SignedOut, "{:?}", check.reviews);
        let (_dir, expired) = fake_gh("echo 'HTTP 401: Bad credentials' >&2\nexit 1");
        assert_eq!(probe(&expired).readiness, Readiness::SignedOut);
        let (_dir, offline) = fake_gh("echo 'dial tcp: network is unreachable' >&2\nexit 1");
        assert_eq!(probe(&offline).readiness, Readiness::Offline);
        let (_dir, offline) = fake_gh("echo 'error connecting to api.github.com' >&2\nexit 1");
        assert_eq!(probe(&offline).readiness, Readiness::Offline);
    }

    #[test]
    fn resolves_account_each_check_and_pins_github_com() {
        let (dir, path) = fake_gh(
            r#"
account=$(cat "$(dirname "$0")/account")
if [ "$4" = user ]; then
  printf '%s\n' "$account"
else
  printf '{"data":{"viewer":{"login":"%s"},"search":{"nodes":[]}}}' "$account"
fi
"#,
        );
        for account in ["alice", "bob"] {
            fs::write(dir.path().join("account"), account).unwrap();
            let check = check_with(|args| {
                assert_eq!(&args[..3], &["api", "--hostname", "github.com"]);
                if args[3] == "graphql" {
                    assert!(args.contains(&format!("me={account}").as_str()));
                    assert!(args.contains(
                        &"q=is:pr is:open archived:false review-requested:@me -author:@me"
                    ));
                }
                run_command(Command::new(&path).args(args), Duration::from_secs(5))
            });
            assert_eq!(check.readiness, Readiness::Ready(account.into()));
            assert!(check.reviews.unwrap().is_empty());
        }
    }

    #[test]
    fn account_switch_mid_check_discards_result() {
        let (_dir, path) = fake_gh(
            r#"
if [ "$4" = user ]; then echo alice; else
  echo '{"data":{"viewer":{"login":"bob"},"search":{"nodes":[]}}}'
fi
"#,
        );
        let check = probe(&path);
        assert_eq!(check.readiness, Readiness::Ready("bob".into()));
        assert!(
            check
                .reviews
                .unwrap_err()
                .to_string()
                .contains("account changed")
        );
    }

    #[test]
    fn malformed_review_response_is_a_failed_sync() {
        let (_dir, path) = fake_gh("if [ \"$4\" = user ]; then echo alice; else echo '{}'; fi");
        let check = probe(&path);
        assert_eq!(check.readiness, Readiness::Ready("alice".into()));
        assert!(check.reviews.is_err());
    }

    #[test]
    fn hung_process_is_bounded_and_next_check_can_recover() {
        let (_dir, path) = fake_gh("exec /bin/sleep 10");
        let started = Instant::now();
        let error = run_command(&mut Command::new(path), Duration::from_millis(60)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let (_dir, path) = fake_gh("printf recovered");
        assert_eq!(
            run_command(&mut Command::new(path), Duration::from_secs(5)).unwrap(),
            "recovered"
        );
    }
    use serde_json::json;

    fn response(host: &str, viewer: &str) -> String {
        json!({"data":{"viewer":{"login":viewer}, "search":{"nodes":[{
            "id":"PR_test", "number":17,"title":"Review me","url":format!("https://{host}/owner/repo/pull/17"),
            "isDraft":false,"author":{"login":"author"},"repository":{"nameWithOwner":"Owner/Repo"},
            "reviewRequests":{"nodes":[{"requestedReviewer":{"__typename":"User","login":viewer}}]},
            "reviews":{"nodes":[]},"timelineItems":{"nodes":[], "pageInfo":{"hasPreviousPage":false,"startCursor":null}}
        }]}}})
        .to_string()
    }

    #[test]
    fn zero_repositories_still_resolves_cli_account_without_claiming_a_review_sync() {
        let results = probe_readiness_with(PUBLIC_HOST, |args| {
            assert_eq!(
                args,
                ["api", "--hostname", PUBLIC_HOST, "user", "--jq", ".login"]
            );
            Ok("alice\n".into())
        });
        assert_eq!(
            results.readiness[PUBLIC_HOST],
            Readiness::Ready("alice".into())
        );
        assert!(results.successful_hosts.is_empty());
        assert!(results.reviews.is_empty());
        let mut store = crate::store::Store::default();
        let mut health = crate::health::SyncHealth::default();
        health.apply_hosts(&results, &mut store, 100);
        assert_eq!(store.last_successful_sync, None);
        assert!(!health.verified);
        let signed_out = probe_readiness_with(PUBLIC_HOST, |_| Err(anyhow::anyhow!("HTTP 401")));
        assert_eq!(signed_out.readiness[PUBLIC_HOST], Readiness::SignedOut);
    }

    #[test]
    fn discovers_configured_hosts_including_unhealthy_ones() {
        // Auth status JSON can succeed even when one host's credentials fail.
        // Host names, rather than successful account states, are the authority.
        assert_eq!(
            parse_hosts(r#"["GITHUB.EXAMPLE.COM","acme.ghe.com","github.com"]"#).unwrap(),
            vec!["acme.ghe.com", "github.com", "github.example.com"]
        );
        assert_eq!(parse_hosts("[]").unwrap(), vec!["github.com"]);
        for invalid in [
            r#"["https://ghe.example"]"#,
            r#"["ghe.example:8443"]"#,
            r#"["-option"]"#,
            "{}",
        ] {
            assert!(parse_hosts(invalid).is_err());
        }
    }

    #[test]
    fn requires_machine_readable_cli_version() {
        assert!(
            check_cli_version("gh version 2.81.0 (2025-10-01)\nhttps://github.com/cli/cli").is_ok()
        );
        assert!(check_cli_version("gh version 2.102.0 (2026-09-30)").is_ok());
        let error = check_cli_version("gh version 2.80.0 (2025-09-23)").unwrap_err();
        assert!(error.to_string().contains("2.81"));
        assert!(check_cli_version("unrecognized version").is_err());
    }

    #[test]
    fn routes_viewer_and_reviews_explicitly_for_each_host() {
        let mut reviews = Vec::new();
        for (host, viewer) in [
            ("github.com", "public-user"),
            ("github.example.com", "server-user"),
            ("acme.ghe.com", "cloud-user"),
        ] {
            let mut calls = Vec::new();
            reviews.extend(
                check_host_with(host, |args| {
                    calls.push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                    assert_eq!(&args[..3], ["api", "--hostname", host]);
                    match args[3] {
                        "user" => Ok(format!("{viewer}\n")),
                        "graphql" => {
                            assert!(args.contains(&format!("me={viewer}").as_str()));
                            Ok(response(host, viewer))
                        }
                        _ => panic!("unexpected endpoint"),
                    }
                })
                .reviews
                .unwrap(),
            );
            assert_eq!(calls.len(), 2);
        }
        let keys: BTreeSet<_> = reviews.iter().map(PendingReview::key).collect();
        assert_eq!(keys.len(), 3);
        assert!(
            reviews
                .iter()
                .all(|pr| pr.repo == "Owner/Repo" && pr.number == 17)
        );
    }

    #[test]
    fn authentication_and_schema_failures_do_not_become_empty_successes() {
        let mut calls = 0;
        let error = fetch_host_with("github.example.com", |_| {
            calls += 1;
            bail!("HTTP 401: bad credentials")
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(format!("{error:#}").contains("gh auth status --hostname github.example.com"));
        assert!(fetch_host_with("github.com", |_| Ok("\n".into())).is_err());
        for output in [
            r#"{"errors":[{"message":"Field isDraft does not exist"}]}"#,
            r#"{"data":{"search":{"nodes":[]}},"errors":[{"message":"permission denied"}]}"#,
            r#"{"data":null}"#,
        ] {
            assert!(
                parse_reviews(output, "github.example.com", "me").is_err(),
                "{output}"
            );
        }
        assert!(
            parse_reviews(
                r#"{"data":{"viewer":{"login":"me"},"search":{"nodes":[]}}}"#,
                "github.example.com",
                "me"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn host_failures_do_not_block_other_hosts() {
        let hosts = BTreeSet::from([
            "github.com".into(),
            "github.example.com".into(),
            "acme.ghe.com".into(),
        ]);
        let results = fetch_hosts_with(&hosts, |host| Check {
            readiness: Readiness::Ready("me".into()),
            reviews: if host == "github.example.com" {
                Err(anyhow::anyhow!("unsupported GraphQL schema"))
            } else {
                parse_reviews(&response(host, "me"), host, "me")
            },
        });
        assert_eq!(results.reviews.len(), 2);
        assert_eq!(
            results.successful_hosts,
            BTreeSet::from(["github.com".into(), "acme.ghe.com".into()])
        );
        assert_eq!(
            results.errors,
            ["github.example.com: unsupported GraphQL schema"]
        );
    }

    #[test]
    fn host_discovery_requests_only_names_and_reports_cli_failures() {
        let mut calls = 0;
        let hosts = known_hosts_with(|args| {
            calls += 1;
            match args {
                ["--version"] => Ok("gh version 2.81.0 (2025-10-01)".into()),
                [
                    "auth",
                    "status",
                    "--active",
                    "--json",
                    "hosts",
                    "--jq",
                    ".hosts | keys",
                ] => Ok(r#"["github.example.com"]"#.into()),
                _ => panic!("unexpected gh invocation: {args:?}"),
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(hosts, ["github.com", "github.example.com"]);
        let error = known_hosts_with(|args| {
            if args == ["--version"] {
                Ok("gh version 2.81.0".into())
            } else {
                bail!("could not read configuration")
            }
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("could not discover GitHub hosts"));
    }

    #[test]
    fn cli_timeout_terminates_the_request_instead_of_blocking_polling() {
        let started = Instant::now();
        let error = command_output(
            Command::new("/bin/sh").args(["-c", "exec sleep 5"]),
            Duration::from_millis(30),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cli_timeout_does_not_wait_for_descendants_holding_pipes() {
        for script in ["sleep 5 & exec sleep 5", "sleep 5 & exit 0"] {
            let started = Instant::now();
            let error = command_output(
                Command::new("/bin/sh").args(["-c", script]),
                Duration::from_millis(50),
            )
            .unwrap_err();
            assert!(error.to_string().contains("timed out"), "{script}: {error}");
            assert!(started.elapsed() < Duration::from_secs(2), "{script}");
            // A failed request must leave the next host free to run immediately.
            let next = command_output(
                Command::new("/bin/sh").args(["-c", "printf healthy"]),
                Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(next.stdout, b"healthy");
        }
    }

    #[test]
    fn continuous_output_cannot_starve_the_cli_deadline() {
        let started = Instant::now();
        let error = command_output(
            Command::new("/bin/sh").args(["-c", "yes stdout & yes stderr >&2 & wait"]),
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cli_output_drains_large_responses_and_preserves_failure_details() {
        let output = command_output(
            Command::new("/bin/sh").args(["-c", "printf 'bad credentials' >&2; exit 1"]),
            Duration::from_secs(2),
        )
        .unwrap();
        let error = gh_output(&["api"], output).unwrap_err();
        assert!(error.to_string().contains("bad credentials"));
        let output = command_output(
            Command::new("/bin/sh").args([
                "-c",
                "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2",
            ]),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 131072);
        assert_eq!(output.stderr.len(), 131072);
    }
}
