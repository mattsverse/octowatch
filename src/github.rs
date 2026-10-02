//! Talks to GitHub through the `gh` CLI, so the user's existing login is reused
//! and no token ever needs to be stored by the app.

use std::{path::Path, process::Command, sync::OnceLock};

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

pub fn viewer_login() -> Result<String> {
    static LOGIN: OnceLock<String> = OnceLock::new();
    if let Some(login) = LOGIN.get() {
        return Ok(login.clone());
    }
    let login = gh(&["api", "user", "--jq", ".login"])?.trim().to_string();
    if login.is_empty() {
        bail!("`gh api user` returned no login");
    }
    Ok(LOGIN.get_or_init(|| login).clone())
}

/// Every pull request across GitHub currently waiting on the viewer's review.
pub fn fetch_awaiting_reviews() -> Result<Vec<PendingReview>> {
    let me = viewer_login()?;
    let search = "is:pr is:open archived:false review-requested:@me -author:@me";
    let output = gh(&[
        "api",
        "graphql",
        "-f",
        &format!("query={QUERY}"),
        "-f",
        &format!("q={search}"),
        "-f",
        &format!("me={me}"),
    ])?;
    let response: Response = serde_json::from_str(&output).context("unexpected GitHub response")?;
    Ok(response
        .data
        .search
        .nodes
        .into_iter()
        .flatten()
        .filter_map(|pr| pr.into_pending(&me))
        .collect())
}

impl Pr {
    fn into_pending(self, me: &str) -> Option<PendingReview> {
        let author = self.author.map(|a| a.login).unwrap_or_else(|| "ghost".into());
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

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new(gh_binary())
        .args(args)
        .output()
        .context("could not run `gh`; is the GitHub CLI installed?")?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
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
    data: Data,
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
