use std::collections::VecDeque;

use serde_json::{Value, json};

use super::*;

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";
const T3: &str = "2026-01-03T00:00:00Z";

fn user(login: &str) -> Value {
    json!({"__typename": "User", "login": login})
}

fn team(id: &str) -> Value {
    json!({"__typename": "Team", "id": id})
}

fn page(nodes: Vec<Value>, next: Option<&str>) -> Value {
    json!({"nodes": nodes, "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}})
}

fn pr(number: u64, reviewers: Vec<Value>) -> Value {
    json!({
        "id": format!("PR_{number}"), "number": number, "title": format!("Review {number}"),
        "url": format!("https://github.com/o/r/pull/{number}"), "isDraft": false,
        "state": "OPEN", "author": {"login": "author"},
        "reviewRequests": page(reviewers.into_iter().map(|reviewer| json!({"requestedReviewer": reviewer})).collect(), None)
    })
}

fn requested(reviewer: Value, at: &str) -> Value {
    json!({"__typename": "ReviewRequestedEvent", "createdAt": at, "requestedReviewer": reviewer})
}

fn reviewed(login: &str, at: Option<&str>, state: &str) -> Value {
    json!({"__typename": "PullRequestReview", "submittedAt": at, "state": state, "author": {"login": login}})
}

fn repository(nodes: Vec<Value>, next: Option<&str>) -> Value {
    json!({"repository": {"nameWithOwner": "o/r", "isArchived": false, "pullRequests": page(nodes, next)}})
}

fn vars(values: &[(&'static str, &str)]) -> Variables {
    values
        .iter()
        .map(|(name, value)| (*name, (*value).into()))
        .collect()
}

struct Step {
    query: &'static str,
    variables: Variables,
    output: Result<String>,
}

fn step(query: &'static str, variables: &[(&'static str, &str)], data: Value) -> Step {
    Step {
        query,
        variables: vars(variables),
        output: Ok(json!({"data": data}).to_string()),
    }
}

fn run(repos: &[&str], steps: Vec<Step>) -> FetchedReviews {
    let mut steps: VecDeque<_> = steps.into();
    let fetched = fetch_with(
        &repos.iter().map(|repo| (*repo).into()).collect(),
        "me",
        &mut |query, variables| {
            let expected = steps.pop_front().expect("unexpected API call");
            assert_eq!(query, expected.query);
            assert_eq!(variables, expected.variables);
            expected.output
        },
    )
    .unwrap();
    assert!(steps.is_empty(), "not all pages fetched");
    fetched
}

fn history_step(number: &str, nodes: Vec<Value>) -> Step {
    step(
        HISTORY_QUERY,
        &[("id", number)],
        json!({"node": {"timelineItems": page(nodes, None)}}),
    )
}

#[test]
fn enumerates_watched_repo_beyond_the_search_ceiling() {
    // All 1,100 earlier PRs are irrelevant; the request on page 12 must arrive.
    let mut steps = Vec::new();
    for index in 0..12 {
        let after = format!("page-{index}");
        let next = format!("page-{}", index + 1);
        let mut variables = vec![("owner", "o"), ("name", "r")];
        if index > 0 {
            variables.push(("after", &after));
        }
        let nodes = if index == 11 {
            vec![pr(1101, vec![user("ME")])]
        } else {
            (index * 100 + 1..=index * 100 + 100)
                .map(|n| pr(n, vec![]))
                .collect()
        };
        steps.push(step(
            REPOSITORY_QUERY,
            &variables,
            repository(nodes, (index < 11).then_some(next.as_str())),
        ));
    }
    steps.push(history_step("PR_1101", vec![requested(user("me"), T1)]));
    let fetched = run(&["o/r"], steps);
    assert!(fetched.errors.is_empty());
    assert_eq!(fetched.completed_repos, HashSet::from(["o/r".into()]));
    assert_eq!(fetched.pending.len(), 1);
    assert_eq!(fetched.pending[0].number, 1101);
}

#[test]
fn paginates_review_requests_and_history_without_losing_an_old_request() {
    let mut pull = pr(1, vec![]);
    pull["reviewRequests"] = page(
        (0..100)
            .map(|n| json!({"requestedReviewer": user(&format!("other{n}"))}))
            .collect(),
        Some("requests-2"),
    );
    let fetched = run(
        &["o/r"],
        vec![
            step(
                REPOSITORY_QUERY,
                &[("owner", "o"), ("name", "r")],
                repository(vec![pull], None),
            ),
            step(
                REQUESTS_QUERY,
                &[("id", "PR_1"), ("after", "requests-2")],
                json!({"node": {"reviewRequests": page(vec![json!({"requestedReviewer": user("me")})], None)}}),
            ),
            step(
                HISTORY_QUERY,
                &[("id", "PR_1")],
                json!({"node": {"timelineItems": page(vec![requested(user("me"), T1)], Some("history-2"))}}),
            ),
            step(
                HISTORY_QUERY,
                &[("id", "PR_1"), ("after", "history-2")],
                json!({"node": {"timelineItems": page((0..100).map(|n| requested(user(&format!("other{n}")), T3)).collect(), None)}}),
            ),
        ],
    );
    assert!(fetched.errors.is_empty());
    assert_eq!(fetched.pending[0].requested_at.as_deref(), Some(T1));
}

#[test]
fn team_only_requests_use_exact_membership_and_cache_it_per_poll() {
    let fetched = run(
        &["o/r"],
        vec![
            step(
                REPOSITORY_QUERY,
                &[("owner", "o"), ("name", "r")],
                repository(
                    vec![pr(1, vec![team("A"), team("B")]), pr(2, vec![team("A")])],
                    None,
                ),
            ),
            step(
                MEMBERS_QUERY,
                &[("id", "A"), ("me", "me")],
                json!({"node": {"members": page(vec![json!({"login": "someone"})], Some("members-2"))}}),
            ),
            step(
                MEMBERS_QUERY,
                &[("id", "A"), ("me", "me"), ("after", "members-2")],
                json!({"node": {"members": page(vec![json!({"login": "ME"})], None)}}),
            ),
            step(
                MEMBERS_QUERY,
                &[("id", "B"), ("me", "me")],
                json!({"node": {"members": page(vec![json!({"login": "me-too"})], None)}}),
            ),
            history_step(
                "PR_1",
                vec![
                    requested(team("A"), T1),
                    requested(team("B"), T3),
                    reviewed("me", Some(T2), "APPROVED"),
                ],
            ),
            history_step("PR_2", vec![requested(team("A"), T3)]),
        ],
    );
    assert!(fetched.errors.is_empty());
    assert_eq!(fetched.pending.len(), 1);
    assert_eq!(fetched.pending[0].number, 2);
    assert!(!fetched.pending[0].rereview);
}

#[test]
fn completed_reviews_clear_and_only_relevant_active_requests_can_reopen() {
    let cases = [
        (
            vec![
                requested(team("A"), T1),
                reviewed("me", Some(T2), "COMMENTED"),
            ],
            None,
        ),
        (
            vec![
                requested(team("A"), T1),
                reviewed("me", Some(T1), "APPROVED"),
            ],
            Some((T1, true)),
        ),
        (
            vec![
                requested(team("A"), T1),
                reviewed("me", Some(T2), "CHANGES_REQUESTED"),
                requested(team("A"), T3),
            ],
            Some((T3, true)),
        ),
        // B is no longer requested (or the viewer is not a member).
        (
            vec![
                requested(team("A"), T1),
                reviewed("me", Some(T2), "DISMISSED"),
                requested(team("B"), T3),
            ],
            None,
        ),
        // A withdrawn direct request must not revive a remaining team request.
        (
            vec![
                requested(team("A"), T1),
                reviewed("me", Some(T2), "APPROVED"),
                requested(user("me"), T3),
            ],
            None,
        ),
        // A draft review is not completion, and a teammate's review isn't ours.
        (
            vec![
                requested(team("A"), T1),
                reviewed("me", None, "PENDING"),
                reviewed("other", Some(T2), "APPROVED"),
            ],
            Some((T1, false)),
        ),
    ];
    for (history, expected) in cases {
        let pull: Pr = serde_json::from_value(pr(1, vec![team("A")])).unwrap();
        let history = serde_json::from_value(json!(history)).unwrap();
        let result = pull
            .into_pending("o/r", "me", &HashSet::from(["team:A".into()]), history)
            .unwrap();
        assert_eq!(
            result.map(|r| (r.requested_at.unwrap(), r.rereview)),
            expected.map(|(at, rereview)| (at.to_string(), rereview))
        );
    }
}

#[test]
fn direct_rerequest_survives_a_later_pending_review() {
    let fetched = run(
        &["o/r"],
        vec![
            step(
                REPOSITORY_QUERY,
                &[("owner", "o"), ("name", "r")],
                repository(vec![pr(1, vec![user("me")])], None),
            ),
            history_step(
                "PR_1",
                vec![
                    requested(user("me"), T1),
                    reviewed("me", Some(T2), "APPROVED"),
                    requested(user("ME"), T3),
                    reviewed("me", None, "PENDING"),
                ],
            ),
        ],
    );
    assert!(fetched.errors.is_empty());
    assert!(fetched.pending[0].rereview);
    assert_eq!(fetched.pending[0].requested_at.as_deref(), Some(T3));
}

#[test]
fn withdrawal_closed_own_and_archived_prs_do_not_enter_queue() {
    let mut closed = pr(2, vec![user("me")]);
    closed["state"] = json!("CLOSED");
    let mut own = pr(3, vec![user("me")]);
    own["author"]["login"] = json!("ME");
    let fetched = run(
        &["o/r"],
        vec![step(
            REPOSITORY_QUERY,
            &[("owner", "o"), ("name", "r")],
            repository(
                vec![pr(1, vec![]), closed, own, pr(4, vec![user("someone")])],
                None,
            ),
        )],
    );
    assert!(fetched.pending.is_empty());
    assert!(fetched.completed_repos.contains("o/r"));
    let mut archived = repository(vec![pr(1, vec![user("me")])], None);
    archived["repository"]["isArchived"] = json!(true);
    let fetched = run(
        &["o/r"],
        vec![step(
            REPOSITORY_QUERY,
            &[("owner", "o"), ("name", "r")],
            archived,
        )],
    );
    assert!(fetched.pending.is_empty());
    assert!(fetched.completed_repos.contains("o/r"));
}

#[test]
fn incomplete_history_or_unavailable_membership_fails_the_repository() {
    let fetched = run(
        &["o/r"],
        vec![
            step(
                REPOSITORY_QUERY,
                &[("owner", "o"), ("name", "r")],
                repository(vec![pr(1, vec![user("me")])], None),
            ),
            history_step("PR_1", vec![requested(user("other"), T3)]),
        ],
    );
    assert!(fetched.completed_repos.is_empty());
    assert!(fetched.errors[0].contains("history incomplete"));
    let fetched = run(
        &["o/r"],
        vec![
            step(
                REPOSITORY_QUERY,
                &[("owner", "o"), ("name", "r")],
                repository(vec![pr(1, vec![team("A")])], None),
            ),
            step(
                MEMBERS_QUERY,
                &[("id", "A"), ("me", "me")],
                json!({"node": null}),
            ),
        ],
    );
    assert!(fetched.completed_repos.is_empty());
    assert!(fetched.errors[0].contains("membership unavailable"));
}

#[test]
fn partial_graphql_null_and_malformed_responses_never_count_as_complete() {
    let responses = [
        json!({"data": repository(vec![], None), "errors": [{"message": "rate limited"}]})
            .to_string(),
        json!({"data": {"repository": null}}).to_string(),
        json!({"data": repository(vec![Value::Null], None)}).to_string(),
        json!({"data": {"repository": {"nameWithOwner": "o/r"}}}).to_string(),
        "not JSON".into(),
    ];
    for output in responses {
        let fetched = run(
            &["o/r"],
            vec![Step {
                query: REPOSITORY_QUERY,
                variables: vars(&[("owner", "o"), ("name", "r")]),
                output: Ok(output),
            }],
        );
        assert!(fetched.completed_repos.is_empty());
        assert!(fetched.pending.is_empty());
        assert_eq!(fetched.errors.len(), 1);
    }
}

#[test]
fn later_page_failure_discards_partial_repo_but_other_repos_still_update() {
    let fetched = run(
        &["o/r", "o/s"],
        vec![
            step(
                REPOSITORY_QUERY,
                &[("owner", "o"), ("name", "r")],
                repository(vec![pr(1, vec![user("me")])], Some("next")),
            ),
            history_step("PR_1", vec![requested(user("me"), T1)]),
            Step {
                query: REPOSITORY_QUERY,
                variables: vars(&[("owner", "o"), ("name", "r"), ("after", "next")]),
                output: Err(anyhow::anyhow!("network unavailable")),
            },
            step(
                REPOSITORY_QUERY,
                &[("owner", "o"), ("name", "s")],
                json!({"repository": {"nameWithOwner": "o/s", "isArchived": false, "pullRequests": page(vec![], None)}}),
            ),
        ],
    );
    assert!(fetched.pending.is_empty());
    assert_eq!(fetched.completed_repos, HashSet::from(["o/s".into()]));
    assert!(fetched.errors[0].contains("network unavailable"));
}

#[test]
fn missing_or_repeated_cursor_is_an_error() {
    let mut cursor = Cursor::default();
    assert!(
        cursor
            .advance(&PageInfo {
                has_next_page: true,
                end_cursor: None
            })
            .is_err()
    );
    assert!(
        cursor
            .advance(&PageInfo {
                has_next_page: true,
                end_cursor: Some("next".into())
            })
            .unwrap()
    );
    assert!(
        cursor
            .advance(&PageInfo {
                has_next_page: true,
                end_cursor: Some("next".into())
            })
            .is_err()
    );
}

#[test]
fn no_watched_repos_makes_no_api_calls() {
    let fetched = run(&[], vec![]);
    assert!(fetched.pending.is_empty());
    assert!(fetched.errors.is_empty());
}

#[test]
fn graphql_errors_explain_partial_data_failures() {
    let fetched = run(
        &["o/r"],
        vec![Step {
            query: REPOSITORY_QUERY,
            variables: vars(&[("owner", "o"), ("name", "r")]),
            output: Ok(json!({
                "data": {"repository": {"pullRequests": null}},
                "errors": [{"message": "insufficient scopes: read:org"}]
            })
            .to_string()),
        }],
    );
    assert!(fetched.completed_repos.is_empty());
    assert!(fetched.errors[0].contains("insufficient scopes: read:org"));
}

#[test]
fn unavailable_nested_nodes_and_missing_history_timestamps_fail_safely() {
    let mut null_requests = pr(1, vec![]);
    null_requests["reviewRequests"] = page(vec![Value::Null], None);
    let mut null_reviewer = pr(1, vec![]);
    null_reviewer["reviewRequests"] = page(vec![json!({"requestedReviewer": null})], None);
    for pull in [null_requests, null_reviewer] {
        let fetched = run(
            &["o/r"],
            vec![step(
                REPOSITORY_QUERY,
                &[("owner", "o"), ("name", "r")],
                repository(vec![pull], None),
            )],
        );
        assert!(fetched.completed_repos.is_empty());
        assert_eq!(fetched.errors.len(), 1);
    }
    for history in [
        vec![Value::Null],
        vec![requested(user("me"), "bad date")],
        vec![requested(user("me"), T1), reviewed("me", None, "APPROVED")],
    ] {
        let fetched = run(
            &["o/r"],
            vec![
                step(
                    REPOSITORY_QUERY,
                    &[("owner", "o"), ("name", "r")],
                    repository(vec![pr(1, vec![user("me")])], None),
                ),
                history_step("PR_1", history),
            ],
        );
        assert!(fetched.completed_repos.is_empty());
        assert_eq!(fetched.errors.len(), 1);
    }
}

#[test]
fn unreadable_team_does_not_hide_a_confirmed_direct_request() {
    let mut forbidden_lookups = 0;
    let fetched = fetch_with(&HashSet::from(["o/r".into()]), "me", &mut |query, variables| {
        let id = variables.iter().find(|(name, _)| *name == "id").map(|(_, value)| value.as_str());
        let data = match query {
            REPOSITORY_QUERY => repository(vec![
                pr(1, vec![team("unreadable"), user("me")]),
                pr(2, vec![team("unreadable")]),
                pr(3, vec![team("readable")]),
            ], None),
            MEMBERS_QUERY if id == Some("unreadable") => {
                forbidden_lookups += 1;
                return Ok(json!({"data": {"node": {"members": null}}, "errors": [{"message": "membership forbidden"}]}).to_string());
            }
            MEMBERS_QUERY => json!({"node": {"members": page(vec![json!({"login": "me"})], None)}}),
            HISTORY_QUERY => json!({"node": {"timelineItems": page(vec![
                if id == Some("PR_1") {requested(user("me"), T1)} else {requested(team("readable"), T2)}
            ], None)}}),
            _ => panic!("unexpected query"),
        };
        Ok(json!({"data": data}).to_string())
    }).unwrap();
    assert_eq!(
        fetched
            .pending
            .iter()
            .map(|pr| pr.number)
            .collect::<Vec<_>>(),
        vec![1, 3],
        "confirmed direct and readable-team requests must reach the queue"
    );
    assert_eq!(
        forbidden_lookups, 1,
        "failed membership lookups should be cached for this poll"
    );
    assert!(
        fetched.completed_repos.is_empty(),
        "unknown teams must not authorize clearing saved requests"
    );
    assert!(fetched.errors[0].contains("membership forbidden"));

    // Exercise the Store boundary too: positive results from this incomplete
    // repository must enter the queue without discarding its uncertain request.
    let mut saved = fetched.pending[0].clone();
    saved.number = 2;
    let mut store = crate::store::Store {
        pending: vec![saved.clone()],
        ..Default::default()
    };
    assert!(store.snooze(&saved.key(), 60, 1_000));
    let result = store.reconcile_repositories(
        fetched.pending,
        &fetched.completed_repos,
        &HashSet::from(["o/r".into()]),
    );
    assert_eq!(
        store
            .pending
            .iter()
            .map(|pr| pr.number)
            .collect::<HashSet<_>>(),
        HashSet::from([1, 2, 3])
    );
    assert_eq!(
        result
            .fresh
            .iter()
            .map(|pr| pr.number)
            .collect::<HashSet<_>>(),
        HashSet::from([1, 3])
    );
    assert_eq!(store.snoozed.len(), 1);
    assert_eq!(store.next_snooze_until(), Some(4_600));
}
