//! Talks to GitHub through the `gh` CLI, so the user's existing login is reused
//! and no token ever needs to be stored by the app.

use std::{
    io::{Read, Seek},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::store::PendingReview;

/// Search for open PRs, by someone else, where the viewer (or one of their
/// teams) is a requested reviewer. GitHub drops the viewer from the request
/// list once they review, so a PR leaves this search when the review lands.
const QUERY: &str = r#"
query($q: String!, $me: String!) {
  viewer { login }
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

/// Re-resolves the effective github.com account on every check. `gh` remains
/// responsible for credentials, including environment-token precedence.
pub fn check() -> Check {
    check_with(gh)
}

fn check_with(mut run: impl FnMut(&[&str]) -> Result<String>) -> Check {
    let login = run(&["api", "--hostname", "github.com", "user", "--jq", ".login"]);
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
        "github.com",
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
        if !response.data.viewer.login.eq_ignore_ascii_case(&me) {
            readiness = Readiness::Ready(response.data.viewer.login);
            bail!("GitHub account changed during the check. Refresh to check the active account.");
        }
        Ok(response
            .data
            .search
            .nodes
            .into_iter()
            .flatten()
            .filter_map(|pr| pr.into_pending(&me))
            .collect())
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

fn classify_failure(err: &anyhow::Error) -> Readiness {
    if err
        .downcast_ref::<std::io::Error>()
        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
    {
        return Readiness::Missing;
    }
    if err.to_string().starts_with("could not run `gh`") {
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

impl Pr {
    fn into_pending(self, me: &str) -> Option<PendingReview> {
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
    let mut command = Command::new(gh_binary());
    command.args(args);
    // Downloading an update needs a larger budget than a review/API check.
    let timeout = if args.starts_with(&["release", "download"]) {
        Duration::from_secs(10 * 60)
    } else {
        Duration::from_secs(30)
    };
    run_command(&mut command, timeout)
}

/// Files drain output without pipe-buffer deadlocks or reader threads that
/// might outlive a timed-out subprocess. Both platforms supported are Unix.
fn run_command(command: &mut Command, timeout: Duration) -> Result<String> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env_remove("GH_DEBUG")
        .spawn()
        .context("could not run `gh`; install the GitHub CLI and Refresh")?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20))
            }
            result => {
                child.kill().ok();
                child.wait().ok();
                if let Err(err) = result {
                    return Err(err.into());
                }
                bail!(
                    "GitHub CLI timed out after {} seconds. Check your connection and Refresh.",
                    timeout.as_secs()
                );
            }
        }
    };
    if !status.success() {
        stderr.rewind()?;
        let mut message = String::new();
        stderr.read_to_string(&mut message)?;
        bail!("GitHub CLI failed: {}", message.trim());
    }
    stdout.rewind()?;
    let mut output = String::new();
    stdout.read_to_string(&mut output)?;
    Ok(output)
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
    data: Data,
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
}
