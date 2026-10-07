//! Complete, host-scoped repository snapshots independent of credential selection.
#[cfg(test)]
use super::{check_cli_version, command_output, gh_output, known_hosts_with, parse_hosts};
#[cfg(test)]
use crate::repository::PUBLIC_HOST;
use crate::{repository::RepositoryId, store::PendingReview};
use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, de::DeserializeOwned};
use std::collections::{BTreeSet, HashMap, HashSet};
#[cfg(test)]
use std::{
    process::Command,
    time::{Duration, Instant},
};
// Enumerate watched repositories rather than global search: search has a
// 1,000-result ceiling and its dashboard qualifiers need not share API behavior.
pub(super) const REPOSITORY_QUERY: &str = r#"
query($owner: String!, $name: String!, $after: String) {
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

pub(super) const MEMBERS_QUERY: &str = r#"
query($id: ID!, $me: String!, $after: String) {
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

pub(super) const HISTORY_QUERY: &str = r#"
query($id: ID!, $after: String) {
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

/// Confirmed requests can enter the queue even when some team lookups fail.
/// Only complete repository snapshots may remove saved requests.
#[derive(Default)]
pub struct FetchedReviews {
    pub pending: Vec<PendingReview>,
    pub completed_repos: HashSet<RepositoryId>,
    pub errors: Vec<String>,
}

pub fn fetch_awaiting_reviews(repos: &HashSet<RepositoryId>) -> Result<FetchedReviews> {
    fetch_repositories_with(repos, super::gh_request)
}

fn fetch_repositories_with(
    repos: &HashSet<RepositoryId>,
    mut run: impl FnMut(&[&str]) -> Result<String>,
) -> Result<FetchedReviews> {
    let hosts: BTreeSet<_> = repos.iter().map(|repo| repo.host.clone()).collect();
    let mut fetched = FetchedReviews::default();
    for host in hosts {
        let me = run(&["api", "--hostname", &host, "user", "--jq", ".login"])
            .with_context(|| {
                format!("could not read viewer; check `gh auth status --hostname {host}`")
            })
            .and_then(|login| {
                let login = login.trim().to_string();
                if login.is_empty() {
                    bail!(
                        "`gh api user` returned no login; check `gh auth status --hostname {host}`"
                    );
                }
                Ok(login)
            });
        let me = match me {
            Ok(me) => me,
            Err(err) => {
                fetched.errors.push(format!("{host}: {err:#}"));
                continue;
            }
        };
        let host_repos = repos
            .iter()
            .filter(|repo| repo.host == host)
            .cloned()
            .collect();
        let result = fetch_with(&host_repos, &me, &mut |query, variables| {
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
            run(&args.iter().map(String::as_str).collect::<Vec<_>>())
        })?;
        fetched.pending.extend(result.pending);
        fetched.completed_repos.extend(result.completed_repos);
        fetched.errors.extend(result.errors);
    }
    Ok(fetched)
}

type Variables = Vec<(&'static str, String)>;
type Api<'a> = dyn FnMut(&str, Variables) -> Result<String> + 'a;
type Memberships = HashMap<(String, String), std::result::Result<bool, String>>;

pub(super) fn fetch_with(
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
            account: me.into(),
            account_id: 0,
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
