//! Talks to GitHub through the `gh` CLI, so the user's saved accounts are reused
//! and no token ever needs to be stored by the app.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::store::PendingReview;

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

/// Account metadata from gh; tokens are never part of this structure.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub login: String,
    pub state: String,
    #[serde(default)]
    pub active: bool,
}

#[derive(Deserialize)]
struct AuthStatus {
    hosts: BTreeMap<String, Vec<Account>>,
}

pub struct AccountReviews {
    pub account_id: u64,
    pub pending: Vec<PendingReview>,
}

pub struct AccountCheck {
    pub account: String,
    pub reviews: Result<AccountReviews>,
    /// Repository-specific failures do not block other repositories.
    pub access_errors: Vec<(String, String)>,
}

pub struct Poll {
    pub accounts: Vec<Account>,
    pub checks: Vec<AccountCheck>,
}

trait GhRunner {
    /// `token` lives only for this invocation, never in command arguments.
    fn run(&self, args: &[&str], token: Option<&str>) -> Result<String>;
}

struct Cli;

impl GhRunner for Cli {
    fn run(&self, args: &[&str], token: Option<&str>) -> Result<String> {
        run_command(command(args, token), token)
    }
}

fn accounts(runner: &impl GhRunner) -> Result<Vec<Account>> {
    let output = runner.run(&["auth", "status", "--hostname", "github.com", "--json", "hosts"], None)
        .context("Could not discover saved github.com accounts. Update gh if it lacks auth status --json.")?;
    let status: AuthStatus =
        serde_json::from_str(&output).context("unexpected gh account response")?;
    let mut accounts = status.hosts.get("github.com").cloned().unwrap_or_default();
    accounts.retain(|account| !account.login.is_empty());
    accounts.sort_by_key(|account| account.login.to_lowercase());
    accounts.dedup_by(|a, b| a.login.eq_ignore_ascii_case(&b.login));
    Ok(accounts)
}

/// Identity verified through the github.com user endpoint.
#[derive(Deserialize)]
struct Viewer {
    login: String,
    id: u64,
}

struct Session {
    token: String,
    viewer: Viewer,
}

/// Look up an explicitly named saved credential and verify who it belongs to
/// on every check. Neither a cached login nor gh's active account is trusted.
fn credential(runner: &impl GhRunner, account: &str) -> Result<Session> {
    let token = runner
        .run(
            &[
                "auth",
                "token",
                "--hostname",
                "github.com",
                "--user",
                account,
            ],
            None,
        )
        .with_context(|| {
            format!(
                "@{account}: saved credential unavailable; run gh auth login --hostname github.com"
            )
        })?;
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("@{account}: gh returned no saved credential");
    }
    let output = runner.run(&["api", "user"], Some(&token))?;
    let viewer: Viewer =
        serde_json::from_str(&output).context("unexpected GitHub identity response")?;
    if viewer.id == 0 || !viewer.login.eq_ignore_ascii_case(account) {
        bail!("@{account}: credential identity changed; refresh gh authentication");
    }
    Ok(Session { token, viewer })
}

pub fn poll(
    repos: &[String],
    disabled_accounts: &BTreeSet<String>,
    repo_accounts: &BTreeMap<String, BTreeSet<String>>,
) -> Result<Poll> {
    poll_with(&Cli, repos, disabled_accounts, repo_accounts)
}

fn poll_with(
    runner: &impl GhRunner,
    repos: &[String],
    disabled_accounts: &BTreeSet<String>,
    repo_accounts: &BTreeMap<String, BTreeSet<String>>,
) -> Result<Poll> {
    let accounts = accounts(runner)?;
    let mut checks = Vec::new();
    for account in accounts
        .iter()
        .filter(|a| !disabled_accounts.contains(&a.login.to_lowercase()))
    {
        let mut access_errors = Vec::new();
        let reviews =
            fetch_account_reviews(runner, account, repos, repo_accounts, &mut access_errors);
        checks.push(AccountCheck {
            account: account.login.clone(),
            reviews,
            access_errors,
        });
    }
    Ok(Poll { accounts, checks })
}

fn fetch_account_reviews(
    runner: &impl GhRunner,
    account: &Account,
    repos: &[String],
    repo_accounts: &BTreeMap<String, BTreeSet<String>>,
    access_errors: &mut Vec<(String, String)>,
) -> Result<AccountReviews> {
    if account.state != "success" {
        bail!(
            "@{}: authentication unavailable ({}); check gh auth status --hostname github.com and sign in again",
            account.login,
            account.state
        );
    }
    let session = credential(runner, &account.login)?;
    let token = session.token.as_str();
    let watched = repos.iter().filter(|repo| {
        repo_accounts
            .get(&repo.to_lowercase())
            .is_none_or(|allowed| allowed.contains(&account.login.to_lowercase()))
    });
    let mut accessible = BTreeSet::new();
    for repo in watched {
        match runner.run(
            &["api", &format!("repos/{repo}"), "--jq", ".full_name"],
            Some(token),
        ) {
            Ok(_) => {
                accessible.insert(repo.to_lowercase());
            }
            Err(err) => {
                let message = format!("{err:#}");
                // Transport, expired-auth and rate-limit failures invalidate
                // the check, preserving the entire account's cache.
                let permissions_error = message.contains("HTTP 404")
                    || (message.contains("HTTP 403")
                        && !message.to_lowercase().contains("rate limit"));
                if !permissions_error {
                    return Err(err);
                }
                access_errors.push((repo.clone(), format!("@{} cannot access {repo}: {message}. Check repository permissions and organization SSO authorization.", account.login)));
            }
        }
    }
    if accessible.is_empty() {
        return Ok(AccountReviews {
            account_id: session.viewer.id,
            pending: Vec::new(),
        });
    }
    // Search as this account, including team requests, then keep only
    // monitored and accessible local repositories.
    let search = "is:pr is:open archived:false review-requested:@me -author:@me";
    let output = runner.run(
        &[
            "api",
            "graphql",
            "-f",
            &format!("query={QUERY}"),
            "-f",
            &format!("q={search}"),
            "-f",
            &format!("me={}", account.login),
        ],
        Some(token),
    )?;
    Ok(AccountReviews {
        account_id: session.viewer.id,
        pending: parse_reviews(&output, &session.viewer)?
            .into_iter()
            .filter(|pr| accessible.contains(&pr.repo.to_lowercase()))
            .collect(),
    })
}

fn parse_reviews(output: &str, me: &Viewer) -> Result<Vec<PendingReview>> {
    let response: Response = serde_json::from_str(output).context("unexpected GitHub response")?;
    if !response.errors.is_empty() {
        bail!("GitHub returned an incomplete review response");
    }
    Ok(response
        .data
        .context("GitHub returned no review data")?
        .search
        .nodes
        .into_iter()
        .flatten()
        .filter_map(|pr| pr.into_pending(me))
        .collect())
}

impl Pr {
    fn into_pending(self, viewer: &Viewer) -> Option<PendingReview> {
        let me = &viewer.login;
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
            account: me.to_string(),
            account_id: viewer.id,
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

/// Updates use saved github.com credentials independently of monitoring
/// settings. Prefer the active healthy account, then try other saved accounts.
pub fn gh(args: &[&str]) -> Result<String> {
    update_gh(&Cli, args)
}

fn update_gh(runner: &impl GhRunner, args: &[&str]) -> Result<String> {
    let mut accounts = accounts(runner)?;
    accounts.sort_by_key(|a| !a.active);
    let mut last_error = None;
    for account in accounts.iter().filter(|a| a.state == "success") {
        match credential(runner, &account.login)
            .and_then(|session| runner.run(args, Some(&session.token)))
        {
            Ok(output) => return Ok(output),
            Err(err) => last_error = Some(err),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        anyhow::anyhow!(
            "No usable saved github.com account; run gh auth login --hostname github.com"
        )
    }))
}

fn command(args: &[&str], token: Option<&str>) -> Command {
    let mut command = Command::new(gh_binary());
    // Saved credentials only. Inherited token/host/debug settings must never
    // select another identity, redirect credentials, or expose them in logs.
    for name in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_DEBUG",
        "DEBUG",
        "GH_REPO",
        "GH_FORCE_TTY",
    ] {
        command.env_remove(name);
    }
    command
        .env("GH_HOST", "github.com")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    if let Some(token) = token {
        command.env("GH_TOKEN", token);
    }
    if args.first() == Some(&"api") {
        command
            .args(["api", "--hostname", "github.com"])
            .args(&args[1..]);
    } else {
        command.args(args);
    }
    command
}

fn run_command(mut command: Command, token: Option<&str>) -> Result<String> {
    let output = command
        .output()
        .context("could not run `gh`; is the GitHub CLI installed?")?;
    if !output.status.success() {
        let mut error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if let Some(token) = token {
            error = error.replace(token, "[redacted]");
        }
        bail!("gh failed: {error}");
    }
    Ok(String::from_utf8(output.stdout)?)
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
    errors: Vec<serde_json::Value>,
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
    use std::{
        cell::RefCell,
        collections::{BTreeMap, BTreeSet, VecDeque},
    };

    use super::*;
    use crate::store::Store;

    const ACCOUNTS: &str = include_str!("../tests/fixtures/accounts.json");
    const REVIEWS: &str = include_str!("../tests/fixtures/shared-review.json");
    const REPO: &str = "owner/repo";

    struct Step {
        args: Vec<String>,
        token: Option<String>,
        output: Result<String>,
    }

    #[derive(Default)]
    struct Script(RefCell<VecDeque<Step>>);

    impl Script {
        fn step(&self, args: &[&str], token: Option<&str>, output: Result<String>) {
            self.0.borrow_mut().push_back(Step {
                args: args.iter().map(|s| s.to_string()).collect(),
                token: token.map(str::to_string),
                output,
            });
        }

        fn status(&self, output: &str) {
            self.step(
                &[
                    "auth",
                    "status",
                    "--hostname",
                    "github.com",
                    "--json",
                    "hosts",
                ],
                None,
                Ok(output.into()),
            );
        }

        fn identity(&self, account: &str, id: u64) {
            let token = format!("fixture-token-{account}");
            self.step(
                &[
                    "auth",
                    "token",
                    "--hostname",
                    "github.com",
                    "--user",
                    account,
                ],
                None,
                Ok(token.clone()),
            );
            self.step(
                &["api", "user"],
                Some(&token),
                Ok(serde_json::json!({"login": account, "id": id}).to_string()),
            );
        }

        fn access(&self, account: &str, repo: &str, output: Result<String>) {
            self.step(
                &["api", &format!("repos/{repo}"), "--jq", ".full_name"],
                Some(&format!("fixture-token-{account}")),
                output,
            );
        }

        fn search(&self, account: &str, output: &str) {
            self.step(
                &[
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={QUERY}"),
                    "-f",
                    "q=is:pr is:open archived:false review-requested:@me -author:@me",
                    "-f",
                    &format!("me={account}"),
                ],
                Some(&format!("fixture-token-{account}")),
                Ok(output.into()),
            );
        }

        fn successful(&self, account: &str, id: u64) {
            self.identity(account, id);
            self.access(account, REPO, Ok(REPO.into()));
            self.search(account, REVIEWS);
        }

        fn finished(&self) {
            assert!(self.0.borrow().is_empty(), "not all expected gh calls ran");
        }
    }

    impl GhRunner for Script {
        fn run(&self, args: &[&str], token: Option<&str>) -> Result<String> {
            let step = self.0.borrow_mut().pop_front().expect("unexpected gh call");
            assert_eq!(args, step.args);
            assert_eq!(token, step.token.as_deref(), "wrong account credential");
            step.output
        }
    }

    fn poll_script(script: &Script) -> Poll {
        poll_with(script, &[REPO.into()], &BTreeSet::new(), &BTreeMap::new()).unwrap()
    }

    fn apply(store: &mut Store, poll: Poll) {
        store.available_accounts.clear();
        let mut checked = BTreeMap::new();
        let mut fetched = Vec::new();
        for check in poll.checks {
            if let Ok(reviews) = check.reviews {
                store
                    .available_accounts
                    .insert(check.account.to_lowercase());
                checked.insert(check.account.to_lowercase(), reviews.account_id);
                fetched.extend(reviews.pending);
            }
        }
        store.reconcile(fetched, &checked);
    }

    #[test]
    fn same_repo_is_polled_with_separate_credentials_despite_active_account_changes() {
        let script = Script::default();
        let mut changed: serde_json::Value = serde_json::from_str(ACCOUNTS).unwrap();
        changed["hosts"]["github.com"][0]["active"] = false.into();
        changed["hosts"]["github.com"][1]["active"] = true.into();
        let mut expected = None;
        for status in [ACCOUNTS.to_string(), changed.to_string()] {
            script.status(&status);
            script.successful("alice", 1);
            script.successful("bob", 2);
            let poll = poll_script(&script);
            assert_eq!(
                poll.accounts.len(),
                2,
                "Enterprise accounts must be excluded"
            );
            let pending: Vec<_> = poll
                .checks
                .into_iter()
                .flat_map(|c| c.reviews.unwrap().pending)
                .collect();
            assert_eq!(pending.len(), 2);
            assert_ne!(pending[0].key(), pending[1].key());
            if let Some(previous) = expected.as_ref() {
                assert_eq!(&pending, previous);
            }
            expected = Some(pending);
        }
        script.finished();
    }

    #[test]
    fn disabled_account_and_explicit_repo_restriction_do_not_poll_another_identity() {
        let script = Script::default();
        script.status(ACCOUNTS);
        // Alice is disabled. Bob is verified, but this repo allows only Alice.
        script.identity("bob", 2);
        let poll = poll_with(
            &script,
            &[REPO.into()],
            &BTreeSet::from(["alice".into()]),
            &BTreeMap::from([(REPO.into(), BTreeSet::from(["alice".into()]))]),
        )
        .unwrap();
        assert_eq!(poll.checks.len(), 1);
        assert_eq!(poll.checks[0].account, "bob");
        assert!(poll.checks[0].reviews.as_ref().unwrap().pending.is_empty());
        script.finished();
    }

    #[test]
    fn expired_auth_hides_only_its_account_and_keeps_its_snooze() {
        let script = Script::default();
        script.status(include_str!("../tests/fixtures/expired-account.json"));
        script.successful("bob", 2);
        let poll = poll_script(&script);
        let error = format!("{:#}", poll.checks[0].reviews.as_ref().err().unwrap());
        assert!(error.contains("@alice"));
        assert!(error.contains("gh auth status"));
        let mut store = Store::from_json(include_str!(
            "../tests/fixtures/multiple-account-state.json"
        ))
        .unwrap();
        store.local_repos.insert(REPO.into());
        apply(&mut store, poll);
        assert_eq!(store.visible_pending().len(), 1);
        assert_eq!(store.visible_pending()[0].account, "bob");
        assert_eq!(store.pending.len(), 2);
        assert_eq!(store.snoozed.len(), 1);
        script.finished();
    }

    #[test]
    fn sign_out_hides_all_cached_reviews_without_reassigning_them() {
        let script = Script::default();
        script.status(include_str!("../tests/fixtures/signed-out.json"));
        let mut store = Store::from_json(include_str!(
            "../tests/fixtures/multiple-account-state.json"
        ))
        .unwrap();
        store.local_repos.insert(REPO.into());
        store
            .available_accounts
            .extend(["alice".into(), "bob".into()]);
        apply(&mut store, poll_script(&script));
        assert!(store.awake().is_empty());
        assert!(store.visible_pending().is_empty());
        assert_eq!(store.pending.len(), 2);
        assert_eq!(store.snoozed.len(), 1);
        script.finished();
    }

    #[test]
    fn missing_saved_credential_does_not_fall_back_to_the_active_account() {
        let script = Script::default();
        script.status(ACCOUNTS);
        script.step(
            &[
                "auth",
                "token",
                "--hostname",
                "github.com",
                "--user",
                "alice",
            ],
            None,
            Err(anyhow::anyhow!("saved token was removed")),
        );
        script.successful("bob", 2);
        let poll = poll_script(&script);
        assert!(poll.checks[0].reviews.is_err());
        assert!(poll.checks[1].reviews.is_ok());
        script.finished();
    }

    #[test]
    fn mismatched_credential_identity_never_searches_as_the_wrong_account() {
        let script = Script::default();
        script.status(ACCOUNTS);
        script.step(
            &[
                "auth",
                "token",
                "--hostname",
                "github.com",
                "--user",
                "alice",
            ],
            None,
            Ok("wrong-fixture-token".into()),
        );
        script.step(
            &["api", "user"],
            Some("wrong-fixture-token"),
            Ok(r#"{"login":"bob","id":2}"#.into()),
        );
        script.successful("bob", 2);
        let poll = poll_script(&script);
        let error = format!("{:#}", poll.checks[0].reviews.as_ref().err().unwrap());
        assert!(error.contains("identity changed"));
        assert!(poll.checks[1].reviews.is_ok());
        script.finished();
    }

    #[test]
    fn inaccessible_repo_reports_account_and_permission_help_while_others_continue() {
        let script = Script::default();
        script.status(ACCOUNTS);
        script.identity("alice", 1);
        script.access("alice", REPO, Ok(REPO.into()));
        script.access(
            "alice",
            "private/repo",
            Err(anyhow::anyhow!("Not Found (HTTP 404)")),
        );
        script.search("alice", REVIEWS);
        script.successful("bob", 2);
        let poll = poll_with(
            &script,
            &[REPO.into(), "private/repo".into()],
            &BTreeSet::new(),
            &BTreeMap::from([("private/repo".into(), BTreeSet::from(["alice".into()]))]),
        )
        .unwrap();
        assert_eq!(poll.checks[0].access_errors.len(), 1);
        let error = &poll.checks[0].access_errors[0].1;
        assert!(error.contains("@alice"));
        assert!(error.contains("private/repo"));
        assert!(error.contains("SSO"));
        assert_eq!(poll.checks[0].reviews.as_ref().unwrap().pending.len(), 1);
        assert_eq!(poll.checks[1].reviews.as_ref().unwrap().pending.len(), 1);
        script.finished();
    }

    #[test]
    fn transport_auth_and_rate_limit_errors_invalidate_the_account_check() {
        for error in [
            "connection refused",
            "Bad credentials (HTTP 401)",
            "API rate limit exceeded (HTTP 403)",
        ] {
            let script = Script::default();
            script.status(ACCOUNTS);
            script.identity("alice", 1);
            script.access("alice", REPO, Err(anyhow::anyhow!(error)));
            script.successful("bob", 2);
            let poll = poll_script(&script);
            assert!(poll.checks[0].reviews.is_err(), "{error}");
            assert!(poll.checks[1].reviews.is_ok());
            script.finished();
        }
    }

    #[test]
    fn partial_graphql_result_is_not_applied_as_a_complete_account_snapshot() {
        let mut response: serde_json::Value = serde_json::from_str(REVIEWS).unwrap();
        response["errors"] = serde_json::json!([{"message": "rate limit"}]);
        assert!(
            parse_reviews(
                &response.to_string(),
                &Viewer {
                    login: "alice".into(),
                    id: 1
                }
            )
            .is_err()
        );
    }

    #[test]
    fn updater_uses_a_healthy_saved_account_without_monitoring_selection() {
        let script = Script::default();
        script.status(include_str!("../tests/fixtures/expired-account.json"));
        script.identity("bob", 2);
        let args = [
            "release",
            "download",
            "v0.5.0",
            "--repo",
            "mattsverse/octowatch",
            "--dir",
            "/fixture-staging",
        ];
        script.step(&args, Some("fixture-token-bob"), Ok(String::new()));
        assert!(update_gh(&script, &args).is_ok());
        script.finished();
    }

    #[test]
    fn command_is_host_pinned_and_clears_inherited_tokens_and_debugging() {
        let invocation = command(&["api", "user"], Some("fixture-token"));
        let args: Vec<_> = invocation.get_args().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(args, ["api", "--hostname", "github.com", "user"]);
        let env: BTreeMap<_, _> = invocation
            .get_envs()
            .map(|(name, value)| (name.to_str().unwrap(), value.and_then(|v| v.to_str())))
            .collect();
        assert_eq!(env["GH_HOST"], Some("github.com"));
        assert_eq!(env["GH_TOKEN"], Some("fixture-token"));
        for name in [
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_DEBUG",
            "DEBUG",
            "GH_REPO",
        ] {
            assert_eq!(env[name], None, "{name}");
        }
        let token_lookup = command(
            &[
                "auth",
                "token",
                "--hostname",
                "github.com",
                "--user",
                "alice",
            ],
            None,
        );
        assert!(
            token_lookup
                .get_envs()
                .any(|(key, value)| key == "GH_TOKEN" && value.is_none())
        );
    }

    #[test]
    fn child_failure_redacts_credentials_from_error_text() {
        let mut child = Command::new("sh");
        child
            .args(["-c", "printf '%s' \"$GH_TOKEN\" >&2; exit 1"])
            .env("GH_TOKEN", "fixture-secret");
        let error = format!(
            "{:#}",
            run_command(child, Some("fixture-secret")).unwrap_err()
        );
        assert!(error.contains("[redacted]"));
        assert!(!error.contains("fixture-secret"));
    }
}
