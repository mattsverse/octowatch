//! Talks to GitHub through the `gh` CLI, so the user's existing login is reused
//! and no token ever needs to be stored by the app.

use std::{
    collections::BTreeSet,
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
  search(query: $q, type: ISSUE, first: 100) {
    nodes {
      ... on PullRequest {
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

#[derive(Debug, Default)]
pub struct FetchResults {
    pub reviews: Vec<PendingReview>,
    pub successful_hosts: BTreeSet<String>,
    pub errors: Vec<String>,
}

/// Poll only hosts with enabled local repositories. Each failure is isolated.
pub fn fetch_awaiting_reviews(hosts: &BTreeSet<String>) -> FetchResults {
    fetch_hosts_with(hosts, |host| fetch_host_with(host, gh_request))
}

fn fetch_hosts_with(
    hosts: &BTreeSet<String>,
    mut fetch: impl FnMut(&str) -> Result<Vec<PendingReview>>,
) -> FetchResults {
    let mut results = FetchResults::default();
    for host in hosts {
        match fetch(host) {
            Ok(reviews) => {
                results.successful_hosts.insert(host.clone());
                results.reviews.extend(reviews);
            }
            Err(err) => results.errors.push(format!("{host}: {err:#}")),
        }
    }
    results
}

fn fetch_host_with(
    host: &str,
    mut run: impl FnMut(&[&str]) -> Result<String>,
) -> Result<Vec<PendingReview>> {
    // Resolve the viewer per request, per host. A process-global login could
    // leak the identity from github.com into an Enterprise review query.
    let me = run(&["api", "--hostname", host, "user", "--jq", ".login"])
        .with_context(|| {
            format!("could not read viewer; check `gh auth status --hostname {host}`")
        })?
        .trim()
        .to_string();
    if me.is_empty() {
        bail!("`gh api user` returned no login; check `gh auth status --hostname {host}`");
    }
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
    ])
    .context("could not fetch reviews (check authentication and server GraphQL compatibility)")?;
    parse_reviews(&output, host, &me)
}

fn parse_reviews(output: &str, host: &str, me: &str) -> Result<Vec<PendingReview>> {
    let response: Response =
        serde_json::from_str(output).context("unexpected GitHub GraphQL response")?;
    if !response.errors.is_empty() {
        bail!(
            "GitHub GraphQL error (check permissions and server compatibility): {}",
            response
                .errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    let data = response
        .data
        .context("GitHub GraphQL response contained no data")?;
    Ok(data
        .search
        .nodes
        .into_iter()
        .flatten()
        .filter_map(|pr| pr.into_pending(host, me))
        .collect())
}

impl Pr {
    fn into_pending(self, host: &str, me: &str) -> Option<PendingReview> {
        let author = self
            .author
            .map(|a| a.login)
            .unwrap_or_else(|| "ghost".into());
        if author.eq_ignore_ascii_case(me) {
            return None;
        }
        // A request counts when it names the viewer or a team (the search
        // already guarantees the viewer belongs to it).
        let concerns_me = |reviewer: &Option<Reviewer>| match reviewer {
            Some(Reviewer::User { login }) => login.eq_ignore_ascii_case(me),
            Some(Reviewer::Team) => true,
            _ => false,
        };
        if !self
            .review_requests
            .nodes
            .iter()
            .any(|r| concerns_me(&r.requested_reviewer))
        {
            return None;
        }
        let requested_at = self
            .timeline_items
            .nodes
            .into_iter()
            .flatten()
            .filter(|event| concerns_me(&event.requested_reviewer))
            .filter_map(|event| event.created_at)
            .max();
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
    let output = Command::new(gh_binary())
        .args(args)
        .output()
        .context("could not run `gh`; is the GitHub CLI installed?")?;
    gh_output(args, output)
}

/// A stalled CLI request must not prevent the remaining hosts from polling.
/// Release downloads use `gh` above because large assets can take longer.
fn gh_request(args: &[&str]) -> Result<String> {
    let mut command = Command::new(gh_binary());
    command.args(args);
    let output = command_output(&mut command, Duration::from_secs(60))
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
    errors: Vec<GraphQLError>,
}

#[derive(Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Deserialize)]
struct Data {
    search: Nodes<Option<Pr>>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pr {
    number: u64,
    title: String,
    url: String,
    is_draft: bool,
    author: Option<Login>,
    repository: Repository,
    review_requests: Nodes<ReviewRequest>,
    reviews: Nodes<Option<Review>>,
    timeline_items: Nodes<Option<RequestEvent>>,
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
    use serde_json::json;

    fn response(host: &str, viewer: &str) -> String {
        json!({"data":{"search":{"nodes":[{
            "number":17,"title":"Review me","url":format!("https://{host}/owner/repo/pull/17"),
            "isDraft":false,"author":{"login":"author"},"repository":{"nameWithOwner":"Owner/Repo"},
            "reviewRequests":{"nodes":[{"requestedReviewer":{"__typename":"User","login":viewer}}]},
            "reviews":{"nodes":[]},"timelineItems":{"nodes":[]}
        }]}}})
        .to_string()
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
                fetch_host_with(host, |args| {
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
                r#"{"data":{"search":{"nodes":[]}}}"#,
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
        let results = fetch_hosts_with(&hosts, |host| {
            if host == "github.example.com" {
                bail!("unsupported GraphQL schema");
            }
            parse_reviews(&response(host, "me"), host, "me")
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
