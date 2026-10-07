//! Talks to GitHub through the `gh` CLI, so the user's saved accounts are reused
//! and no token ever needs to be stored by the app.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
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
    repository::{PUBLIC_HOST, RepositoryId, normalize_host},
    store::{PendingReview, Store},
};

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

mod queue;
pub use queue::FetchedReviews;

pub fn fetch_awaiting_reviews(repos: &HashSet<RepositoryId>) -> Result<FetchedReviews> {
    queue::fetch_awaiting_reviews(repos)
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

/// Account metadata from gh; tokens are never part of this structure.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub login: String,
    pub state: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub account_id: Option<u64>,
}

#[derive(Deserialize)]
struct AuthStatus {
    hosts: BTreeMap<String, Vec<Account>>,
}

pub struct AccountReviews {
    pub account_id: u64,
    pub pending: Vec<PendingReview>,
    pub unavailable_repos: BTreeSet<String>,
    pub completed_repos: BTreeSet<String>,
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
    pub access_cache: AccessCache,
}

/// Session-only positive access results. No credentials are cached.
#[derive(Clone, Default)]
pub struct AccessCache {
    allowed: BTreeMap<(u64, String), Instant>,
}

const ACCESS_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

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
fn credential(runner: &impl GhRunner, account: &str, known_id: Option<u64>) -> Result<Session> {
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
    if viewer.id == 0
        || (!viewer.login.eq_ignore_ascii_case(account) && known_id != Some(viewer.id))
    {
        bail!("@{account}: credential identity changed; refresh gh authentication");
    }
    Ok(Session { token, viewer })
}

pub fn poll(repos: &[String], preferences: Store, cache: AccessCache) -> Result<Poll> {
    poll_with(&Cli, repos, preferences, cache, Instant::now())
}

fn poll_with(
    runner: &impl GhRunner,
    repos: &[String],
    mut preferences: Store,
    mut cache: AccessCache,
    now: Instant,
) -> Result<Poll> {
    let mut accounts = accounts(runner)?;
    cache.allowed.retain(|_, expires| *expires > now);
    let mut checks = Vec::new();
    let mut checked_ids = BTreeSet::new();
    for account in &mut accounts {
        // Verify disabled identities too, so renames cannot re-enable them.
        let identity = if account.state != "success" {
            Err(anyhow::anyhow!(
                "@{}: authentication unavailable ({}); check gh auth status --hostname github.com and sign in again",
                account.login,
                account.state
            ))
        } else {
            credential(
                runner,
                &account.login,
                preferences
                    .account_ids
                    .get(&account.login.to_lowercase())
                    .copied(),
            )
        };
        let session = match identity {
            Ok(session) => session,
            Err(err) => {
                if let Some(id) = preferences.account_ids.get(&account.login.to_lowercase()) {
                    cache.allowed.retain(|(account_id, _), _| account_id != id);
                }
                if preferences.account_enabled(&account.login) {
                    checks.push(AccountCheck {
                        account: account.login.clone(),
                        reviews: Err(err),
                        access_errors: Vec::new(),
                    });
                }
                continue;
            }
        };
        preferences.record_account(&session.viewer.login, session.viewer.id);
        account.login = session.viewer.login.clone();
        account.account_id = Some(session.viewer.id);
        if !preferences.account_enabled(&account.login) || !checked_ids.insert(session.viewer.id) {
            continue;
        }
        let mut access_errors = Vec::new();
        let reviews = fetch_account_reviews(
            runner,
            &session,
            repos,
            &preferences,
            &mut cache,
            now,
            &mut access_errors,
        );
        if reviews.is_err() {
            cache.allowed.retain(|(id, _), _| *id != session.viewer.id);
        }
        checks.push(AccountCheck {
            account: account.login.clone(),
            reviews,
            access_errors,
        });
    }
    accounts.sort_by_key(|account| account.login.to_lowercase());
    accounts.dedup_by(|a, b| a.login.eq_ignore_ascii_case(&b.login));
    cache.allowed.retain(|(id, _), _| checked_ids.contains(id));
    Ok(Poll {
        accounts,
        checks,
        access_cache: cache,
    })
}

fn fetch_account_reviews(
    runner: &impl GhRunner,
    session: &Session,
    repos: &[String],
    preferences: &Store,
    cache: &mut AccessCache,
    now: Instant,
    access_errors: &mut Vec<(String, String)>,
) -> Result<AccountReviews> {
    let me = &session.viewer.login;
    let token = session.token.as_str();
    let watched = repos.iter().filter(|repo| preferences.monitors(me, repo));
    let mut accessible = BTreeSet::new();
    let mut cached_access = BTreeSet::new();
    let mut unavailable_repos = BTreeSet::new();
    for repo in watched {
        let key = (session.viewer.id, repo.to_lowercase());
        if cache
            .allowed
            .get(&key)
            .is_some_and(|expires| *expires > now)
        {
            accessible.insert(repo.to_lowercase());
            cached_access.insert(repo.to_lowercase());
            continue;
        }
        if check_repo_access(runner, session, repo, cache, now, access_errors)? {
            accessible.insert(repo.to_lowercase());
        } else {
            unavailable_repos.insert(repo.to_lowercase());
        }
    }
    if accessible.is_empty() {
        return Ok(AccountReviews {
            account_id: session.viewer.id,
            pending: Vec::new(),
            unavailable_repos,
            completed_repos: BTreeSet::new(),
        });
    }
    let repositories: HashSet<_> = accessible
        .iter()
        .map(|repo| RepositoryId::new(PUBLIC_HOST, repo))
        .collect();
    let mut account_failure = None;
    // Share membership results only within this verified account's current poll.
    let result = queue::fetch_with(&repositories, me, &mut |query, variables| {
        let mut args = vec![
            "api".to_string(),
            "graphql".into(),
            "-f".into(),
            format!("query={query}"),
        ];
        for (name, value) in variables {
            args.extend(["-f".into(), format!("{name}={value}")]);
        }
        let result = runner.run(
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            Some(token),
        );
        if let Err(error) = &result {
            let message = format!("{error:#}");
            let lower = message.to_lowercase();
            if message.contains("HTTP 401")
                || lower.contains("rate limit")
                || lower.contains("connection")
                || lower.contains("network")
                || lower.contains("timed out")
                || lower.contains("could not run")
            {
                account_failure = Some(message);
            }
        }
        result
    })?;
    if let Some(error) = account_failure {
        bail!("{error}");
    }
    let mut completed_repos: BTreeSet<_> = result
        .completed_repos
        .into_iter()
        .map(|repo| repo.slug)
        .collect();
    for repo in accessible.difference(&completed_repos) {
        cache.allowed.remove(&(session.viewer.id, repo.clone()));
        unavailable_repos.insert(repo.clone());
    }
    for error in result.errors {
        let repo = repositories
            .iter()
            .find(|repo| error.starts_with(&format!("{repo}: ")))
            .map(|repo| repo.slug.clone())
            .unwrap_or_else(|| "unknown repository".into());
        access_errors.push((
            repo,
            format!("@{me}: {error}. Unconfirmed cache and snoozes are retained."),
        ));
    }
    let pending: Vec<_> = result
        .pending
        .into_iter()
        .map(|mut pr| {
            pr.account_id = session.viewer.id;
            pr.account = me.clone();
            pr
        })
        .collect();
    // Before removing a saved request after a cached access result,
    // verify the repository again; incomplete snapshots cannot remove state.
    let missing_repos: BTreeSet<_> = preferences
        .pending
        .iter()
        .filter(|old| {
            old.account_id == session.viewer.id
                && cached_access.contains(&old.repo.to_lowercase())
                && completed_repos.contains(&old.repo.to_lowercase())
                && !pending.iter().any(|pr| pr.key() == old.key())
        })
        .map(|pr| pr.repo.to_lowercase())
        .collect();
    for repo in missing_repos {
        if !check_repo_access(runner, session, &repo, cache, now, access_errors)? {
            accessible.remove(&repo);
            completed_repos.remove(&repo);
            unavailable_repos.insert(repo);
        }
    }
    Ok(AccountReviews {
        account_id: session.viewer.id,
        pending: pending
            .into_iter()
            .filter(|pr| accessible.contains(&pr.repo.to_lowercase()))
            .collect(),
        unavailable_repos,
        completed_repos,
    })
}

fn check_repo_access(
    runner: &impl GhRunner,
    session: &Session,
    repo: &str,
    cache: &mut AccessCache,
    now: Instant,
    access_errors: &mut Vec<(String, String)>,
) -> Result<bool> {
    let key = (session.viewer.id, repo.to_lowercase());
    match runner.run(
        &["api", &format!("repos/{repo}"), "--jq", ".full_name"],
        Some(&session.token),
    ) {
        Ok(_) => {
            cache.allowed.insert(key, now + ACCESS_CACHE_TTL);
            Ok(true)
        }
        Err(err) => {
            cache.allowed.remove(&key);
            let message = format!("{err:#}");
            if !message.contains("HTTP 404")
                && (!message.contains("HTTP 403") || message.to_lowercase().contains("rate limit"))
            {
                return Err(err);
            }
            // Repository failures never prove its review requests disappeared.
            let me = &session.viewer.login;
            access_errors.push((repo.into(), format!("@{me} cannot check {repo}: {message}. Cached reviews and snoozes are retained. Check repository permissions and organization SSO authorization.")));
            Ok(false)
        }
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
        match credential(runner, &account.login, None)
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
    let output = if command
        .get_args()
        .next()
        .is_some_and(|arg| arg == "release")
    {
        command
            .output()
            .context("could not run `gh`; is the GitHub CLI installed?")?
    } else {
        command_output(&mut command, Duration::from_secs(60))?
    };
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
            self.renamed_identity(account, account, id);
        }

        fn renamed_identity(&self, account: &str, current_login: &str, id: u64) {
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
                Ok(serde_json::json!({"login": current_login, "id": id}).to_string()),
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
            self.snapshot(account, account, REPO, output);
        }

        fn snapshot(&self, credential: &str, account: &str, repo: &str, output: &str) {
            let response: serde_json::Value = serde_json::from_str(output).unwrap();
            let mut nodes = response["data"]["search"]["nodes"]
                .as_array()
                .unwrap()
                .clone();
            let mut histories = Vec::new();
            for pr in &mut nodes {
                let number = pr["number"].as_u64().unwrap();
                pr["id"] = format!("PR_{number}").into();
                pr["state"] = "OPEN".into();
                pr["reviewRequests"]["pageInfo"] =
                    serde_json::json!({"hasNextPage":false,"endCursor":null});
                let mut history = pr["timelineItems"]["nodes"].as_array().unwrap().clone();
                for item in &mut history {
                    item["__typename"] = "ReviewRequestedEvent".into();
                }
                for review in pr["reviews"]["nodes"].as_array().unwrap() {
                    let mut review = review.clone();
                    review["__typename"] = "PullRequestReview".into();
                    history.push(review);
                }
                histories.push((format!("PR_{number}"), history));
            }
            let (owner, name) = repo.split_once('/').unwrap();
            let token = format!("fixture-token-{credential}");
            self.step(&["api", "graphql", "-f", &format!("query={}", queue::REPOSITORY_QUERY),
                "-f", &format!("owner={owner}"), "-f", &format!("name={name}")], Some(&token),
                Ok(serde_json::json!({"data":{"repository":{"nameWithOwner":repo,"isArchived":false,"pullRequests":{
                    "nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}
                }}}}).to_string()));
            for (id, nodes) in histories {
                self.step(
                    &[
                        "api",
                        "graphql",
                        "-f",
                        &format!("query={}", queue::HISTORY_QUERY),
                        "-f",
                        &format!("id={id}"),
                    ],
                    Some(&token),
                    Ok(serde_json::json!({"data":{"node":{"timelineItems":{
                        "nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}
                    }}}})
                    .to_string()),
                );
            }
            let _ = account; // Identity is already verified separately; request fixtures carry reviewer names.
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
        poll_with(
            script,
            &[REPO.into()],
            Store::default(),
            AccessCache::default(),
            Instant::now(),
        )
        .unwrap()
    }

    fn apply(store: &mut Store, poll: Poll) {
        store.available_accounts.clear();
        store.unavailable_repos.clear();
        store.confirmed_requests.clear();
        for account in poll.accounts {
            if let Some(id) = account.account_id {
                store.record_account(&account.login, id);
            }
        }
        let mut completed = BTreeSet::new();
        let mut fetched = Vec::new();
        for check in poll.checks {
            if let Ok(reviews) = check.reviews {
                store
                    .available_accounts
                    .insert(check.account.to_lowercase());
                completed.extend(
                    reviews
                        .completed_repos
                        .iter()
                        .map(|repo| (reviews.account_id, repo.clone())),
                );
                store.unavailable_repos.extend(
                    reviews
                        .unavailable_repos
                        .into_iter()
                        .map(|repo| (reviews.account_id, repo)),
                );
                fetched.extend(reviews.pending);
            }
        }
        store
            .confirmed_requests
            .extend(fetched.iter().map(|pr| (pr.key(), pr.requested_at.clone())));
        let watched = store
            .pending
            .iter()
            .chain(fetched.iter())
            .map(|pr| pr.repository().store_key())
            .chain(completed.iter().map(|(_, repo)| repo.clone()))
            .collect();
        store.reconcile_partitions(fetched, &completed, &watched);
    }

    #[test]
    fn paginated_public_queries_keep_each_receiving_accounts_credential() {
        let script = Script::default();
        script.status(ACCOUNTS);
        for (account, id) in [("alice", 1), ("bob", 2)] {
            script.identity(account, id);
            script.access(account, REPO, Ok(REPO.into()));
            script.search(account, REVIEWS);
            {
                let mut steps = script.0.borrow_mut();
                let step = steps
                    .iter_mut()
                    .rev()
                    .find(|s| {
                        s.args
                            .iter()
                            .any(|a| a == &format!("query={}", queue::REPOSITORY_QUERY))
                    })
                    .unwrap();
                let mut output: serde_json::Value =
                    serde_json::from_str(step.output.as_ref().unwrap()).unwrap();
                output["data"]["repository"]["pullRequests"]["pageInfo"] =
                    serde_json::json!({"hasNextPage":true,"endCursor":"next"});
                step.output = Ok(output.to_string());
            }
            script.step(&["api", "graphql", "-f", &format!("query={}", queue::REPOSITORY_QUERY),
                "-f", "owner=owner", "-f", "name=repo", "-f", "after=next"],
                Some(&format!("fixture-token-{account}")), Ok(serde_json::json!({"data":{"repository":{
                    "nameWithOwner":REPO,"isArchived":false,"pullRequests":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}
                }}}).to_string()));
        }
        let poll = poll_script(&script);
        assert_eq!(
            poll.checks[0].reviews.as_ref().unwrap().pending[0].account_id,
            1
        );
        assert_eq!(
            poll.checks[1].reviews.as_ref().unwrap().pending[0].account_id,
            2
        );
        script.finished();
    }

    #[test]
    fn unreadable_team_keeps_only_confirmed_requests_visible_under_that_account() {
        let mut store = Store::from_json(include_str!(
            "../tests/fixtures/multiple-account-state.json"
        ))
        .unwrap();
        store.local_repos.insert(REPO.into());
        let mut uncertain = store.pending[0].clone();
        uncertain.number = 8;
        store.pending.push(uncertain.clone());
        let mut snooze = store.snoozed[0].clone();
        snooze.number = 8;
        snooze.until = 50;
        store.snoozed.push(snooze.clone());
        let script = Script::default();
        script.status(ACCOUNTS);
        script.successful("alice", 1);
        {
            let mut steps = script.0.borrow_mut();
            let repository_ix = steps
                .iter()
                .position(|s| {
                    s.args
                        .iter()
                        .any(|a| a == &format!("query={}", queue::REPOSITORY_QUERY))
                })
                .unwrap();
            let step = &mut steps[repository_ix];
            let mut output: serde_json::Value =
                serde_json::from_str(step.output.as_ref().unwrap()).unwrap();
            output["data"]["repository"]["pullRequests"]["nodes"][0]["reviewRequests"]["nodes"].as_array_mut().unwrap()
                .push(serde_json::json!({"requestedReviewer":{"__typename":"Team","id":"hidden-team"}}));
            step.output = Ok(output.to_string());
            steps.insert(
                repository_ix + 1,
                Step {
                    args: vec![
                        "api".into(),
                        "graphql".into(),
                        "-f".into(),
                        format!("query={}", queue::MEMBERS_QUERY),
                        "-f".into(),
                        "id=hidden-team".into(),
                        "-f".into(),
                        "me=alice".into(),
                    ],
                    token: Some("fixture-token-alice".into()),
                    output: Err(anyhow::anyhow!(
                        "GraphQL: Resource not accessible by integration"
                    )),
                },
            );
        }
        script.successful("bob", 2);
        let poll = poll_script(&script);
        let alice = poll.checks[0].reviews.as_ref().unwrap();
        assert!(alice.completed_repos.is_empty());
        assert_eq!(alice.pending.len(), 1);
        assert_eq!(poll.checks[0].access_errors.len(), 1);
        apply(&mut store, poll);
        assert_eq!(store.pending.len(), 3);
        assert_eq!(store.visible_pending().len(), 2);
        assert!(!store.visible(&uncertain));
        assert!(store.take_expired(100).is_empty());
        assert!(store.snoozed.contains(&snooze));
        script.status(ACCOUNTS);
        script.successful("alice", 1);
        script.successful("bob", 2);
        apply(&mut store, poll_script(&script));
        assert_eq!(store.pending.len(), 2);
        assert!(!store.snoozed.contains(&snooze));
        script.finished();
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
        // Disabled Alice is still verified; Bob cannot poll Alice's restricted repo.
        script.identity("alice", 1);
        script.identity("bob", 2);
        let poll = poll_with(
            &script,
            &[REPO.into()],
            Store {
                disabled_accounts: BTreeSet::from(["alice".into()]),
                repo_accounts: BTreeMap::from([(REPO.into(), BTreeSet::from(["alice".into()]))]),
                ..Store::default()
            },
            AccessCache::default(),
            Instant::now(),
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
            Store {
                repo_accounts: BTreeMap::from([(
                    "private/repo".into(),
                    BTreeSet::from(["alice".into()]),
                )]),
                ..Store::default()
            },
            AccessCache::default(),
            Instant::now(),
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
    fn repository_failures_hide_and_preserve_snoozes_until_access_recovers() {
        for error in ["Forbidden (HTTP 403)", "Not Found (HTTP 404)"] {
            let mut store = Store::from_json(include_str!(
                "../tests/fixtures/multiple-account-state.json"
            ))
            .unwrap();
            store.local_repos.insert(REPO.into());
            store.snoozed[0].until = 50;
            let original = store.snoozed.clone();
            let script = Script::default();
            script.status(ACCOUNTS);
            script.identity("alice", 1);
            script.access("alice", REPO, Err(anyhow::anyhow!(error)));
            script.successful("bob", 2);
            apply(&mut store, poll_script(&script));
            assert_eq!(store.pending.len(), 2);
            assert_eq!(store.visible_pending().len(), 1);
            assert_eq!(store.visible_pending()[0].account, "bob");
            assert_eq!(store.snoozed, original);
            assert!(store.take_expired(100).is_empty());
            script.status(ACCOUNTS);
            script.successful("alice", 1);
            script.successful("bob", 2);
            apply(&mut store, poll_script(&script));
            assert_eq!(store.visible_pending().len(), 2);
            assert_eq!(store.take_expired(100)[0].account, "alice");
            script.finished();
        }
    }

    #[test]
    fn failing_repo_does_not_prevent_removing_completed_reviews_in_another_repo() {
        let mut store = Store::from_json(include_str!(
            "../tests/fixtures/multiple-account-state.json"
        ))
        .unwrap();
        let mut completed = store.pending[0].clone();
        completed.repo = "other/repo".into();
        store.pending.push(completed);
        let script = Script::default();
        script.status(ACCOUNTS);
        script.identity("alice", 1);
        script.access("alice", REPO, Err(anyhow::anyhow!("Forbidden (HTTP 403)")));
        script.access("alice", "other/repo", Ok("other/repo".into()));
        script.snapshot(
            "alice",
            "alice",
            "other/repo",
            r#"{"data":{"search":{"nodes":[]}}}"#,
        );
        script.successful("bob", 2);
        let poll = poll_with(
            &script,
            &[REPO.into(), "other/repo".into()],
            Store {
                repo_accounts: BTreeMap::from([(
                    "other/repo".into(),
                    BTreeSet::from(["alice".into()]),
                )]),
                ..store.clone()
            },
            AccessCache::default(),
            Instant::now(),
        )
        .unwrap();
        apply(&mut store, poll);
        assert_eq!(store.pending.len(), 2);
        assert!(
            store
                .pending
                .iter()
                .all(|pr| pr.repo.to_lowercase() == REPO)
        );
        assert_eq!(store.snoozed.len(), 1);
        script.finished();
    }

    #[test]
    fn renamed_accounts_preserve_disabled_and_repository_choices() {
        for disabled in [false, true] {
            let mut store = Store::from_json(include_str!(
                "../tests/fixtures/multiple-account-state.json"
            ))
            .unwrap();
            store
                .repo_accounts
                .insert(REPO.into(), BTreeSet::from(["alice".into()]));
            if disabled {
                store.toggle_account("alice");
            }
            let script = Script::default();
            script.status(ACCOUNTS);
            // gh's saved credential still has its old name, but /user is canonical.
            script.renamed_identity("alice", "alice-renamed", 1);
            if !disabled {
                script.access("alice", REPO, Ok(REPO.into()));
                let renamed = REVIEWS.replace("alice", "alice-renamed");
                script.snapshot("alice", "alice-renamed", REPO, &renamed);
            }
            script.identity("bob", 2);
            let poll = poll_with(
                &script,
                &[REPO.into()],
                store.clone(),
                AccessCache::default(),
                Instant::now(),
            )
            .unwrap();
            assert_eq!(poll.checks.len(), if disabled { 1 } else { 2 });
            apply(&mut store, poll);
            assert_eq!(store.account_enabled("alice-renamed"), !disabled);
            assert!(store.repo_account_selected(REPO, "alice-renamed"));
            assert!(!store.repo_account_selected(REPO, "bob"));
            assert_eq!(store.snoozed[0].account, "alice-renamed");
            assert!(!store.known_accounts.contains("alice"));
            let restarted = Store::from_json(&serde_json::to_string(&store).unwrap()).unwrap();
            assert_eq!(restarted.account_enabled("alice-renamed"), !disabled);
            assert!(restarted.repo_account_selected(REPO, "alice-renamed"));
            script.finished();
        }
    }

    #[test]
    fn access_cache_skips_probes_but_still_verifies_and_searches_each_account() {
        let script = Script::default();
        let now = Instant::now();
        let mut cache = AccessCache::default();
        for elapsed in [0, 120, 1800, 1920] {
            script.status(ACCOUNTS);
            for (account, id) in [("alice", 1), ("bob", 2)] {
                script.identity(account, id);
                if elapsed == 0 || elapsed == 1800 {
                    script.access(account, REPO, Ok(REPO.into()));
                }
                script.search(account, REVIEWS);
            }
            let poll = poll_with(
                &script,
                &[REPO.into()],
                Store::default(),
                cache,
                now + Duration::from_secs(elapsed),
            )
            .unwrap();
            assert!(
                poll.checks
                    .iter()
                    .all(|check| check.reviews.as_ref().unwrap().pending.len() == 1)
            );
            cache = poll.access_cache;
        }
        // Manual Refresh deliberately drops the cache and probes immediately.
        script.status(ACCOUNTS);
        script.successful("alice", 1);
        script.successful("bob", 2);
        poll_with(
            &script,
            &[REPO.into()],
            Store::default(),
            AccessCache::default(),
            now + Duration::from_secs(1921),
        )
        .unwrap();
        script.finished();
    }

    #[test]
    fn access_results_are_never_reused_for_another_user_id() {
        let script = Script::default();
        script.status(ACCOUNTS);
        script.successful("alice", 1);
        script.successful("bob", 2);
        let now = Instant::now();
        let first = poll_with(
            &script,
            &[REPO.into()],
            Store::default(),
            AccessCache::default(),
            now,
        )
        .unwrap();
        script.status(ACCOUNTS);
        script.successful("alice", 99);
        script.identity("bob", 2);
        script.search("bob", REVIEWS);
        let second = poll_with(
            &script,
            &[REPO.into()],
            Store::default(),
            first.access_cache,
            now + Duration::from_secs(120),
        )
        .unwrap();
        assert_eq!(second.checks[0].reviews.as_ref().unwrap().account_id, 99);
        script.finished();
    }

    #[test]
    fn auth_failure_invalidates_only_that_accounts_cached_access() {
        let script = Script::default();
        let now = Instant::now();
        let store = Store::from_json(include_str!(
            "../tests/fixtures/multiple-account-state.json"
        ))
        .unwrap();
        script.status(ACCOUNTS);
        script.successful("alice", 1);
        script.successful("bob", 2);
        let first = poll_with(
            &script,
            &[REPO.into()],
            store.clone(),
            AccessCache::default(),
            now,
        )
        .unwrap();
        script.status(include_str!("../tests/fixtures/expired-account.json"));
        script.identity("bob", 2);
        script.search("bob", REVIEWS);
        let failed = poll_with(
            &script,
            &[REPO.into()],
            store.clone(),
            first.access_cache,
            now + Duration::from_secs(120),
        )
        .unwrap();
        assert!(failed.checks[0].reviews.is_err());
        script.status(ACCOUNTS);
        script.successful("alice", 1);
        script.identity("bob", 2);
        script.search("bob", REVIEWS);
        let recovered = poll_with(
            &script,
            &[REPO.into()],
            store,
            failed.access_cache,
            now + Duration::from_secs(240),
        )
        .unwrap();
        assert!(recovered.checks.iter().all(|check| check.reviews.is_ok()));
        script.finished();
    }

    #[test]
    fn missing_requests_recheck_cached_access_before_deleting_state() {
        for accessible in [false, true] {
            let script = Script::default();
            let now = Instant::now();
            let mut store = Store::from_json(include_str!(
                "../tests/fixtures/multiple-account-state.json"
            ))
            .unwrap();
            script.status(ACCOUNTS);
            script.successful("alice", 1);
            script.successful("bob", 2);
            let first = poll_with(
                &script,
                &[REPO.into()],
                store.clone(),
                AccessCache::default(),
                now,
            )
            .unwrap();
            script.status(ACCOUNTS);
            script.identity("alice", 1);
            script.search("alice", r#"{"data":{"search":{"nodes":[]}}}"#);
            script.access(
                "alice",
                REPO,
                if accessible {
                    Ok(REPO.into())
                } else {
                    Err(anyhow::anyhow!("Forbidden (HTTP 403)"))
                },
            );
            script.identity("bob", 2);
            script.search("bob", REVIEWS);
            let next = poll_with(
                &script,
                &[REPO.into()],
                store.clone(),
                first.access_cache,
                now + Duration::from_secs(120),
            )
            .unwrap();
            apply(&mut store, next);
            assert_eq!(store.pending.len(), if accessible { 1 } else { 2 });
            assert_eq!(store.snoozed.len(), usize::from(!accessible));
            script.finished();
        }
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
