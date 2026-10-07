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
        &repos
            .iter()
            .map(|repo| RepositoryId::new(PUBLIC_HOST, repo))
            .collect(),
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
    assert_eq!(
        fetched.completed_repos,
        HashSet::from([RepositoryId::new(PUBLIC_HOST, "o/r")])
    );
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
            .into_pending(
                PUBLIC_HOST,
                "o/r",
                "me",
                &HashSet::from(["team:A".into()]),
                history,
            )
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
    assert!(
        fetched
            .completed_repos
            .contains(&RepositoryId::new(PUBLIC_HOST, "o/r"))
    );
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
    assert!(
        fetched
            .completed_repos
            .contains(&RepositoryId::new(PUBLIC_HOST, "o/r"))
    );
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
    assert_eq!(
        fetched.completed_repos,
        HashSet::from([RepositoryId::new(PUBLIC_HOST, "o/s")])
    );
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
fn paginated_queries_use_each_hosts_current_viewer_and_team_membership() {
    let repos: HashSet<_> = [PUBLIC_HOST, "github.example.com", "acme.ghe.com"]
        .into_iter()
        .map(|host| RepositoryId::new(host, "o/r"))
        .collect();
    for poll in [1, 2] {
        let mut calls = 0;
        let fetched = fetch_repositories_with(&repos, |args| {
            calls += 1;
            assert_eq!(&args[..2], ["api", "--hostname"]);
            let host = args[2];
            assert!(repos.contains(&RepositoryId::new(host, "o/r")));
            let viewer = format!("viewer-{host}-{poll}");
            if args[3] == "user" {
                assert_eq!(&args[4..], ["--jq", ".login"]);
                return Ok(viewer);
            }
            assert_eq!(args[3], "graphql");
            let field = |name: &str| {
                args.iter()
                    .find_map(|arg| arg.strip_prefix(&format!("{name}=")))
            };
            let data = match field("query").unwrap() {
                REPOSITORY_QUERY => {
                    assert_eq!(field("owner"), Some("o"));
                    assert_eq!(field("name"), Some("r"));
                    if field("after").is_some() {
                        assert_eq!(field("after"), Some("repo-next"));
                        repository(vec![], None)
                    } else {
                        let mut pull = pr(1, vec![]);
                        pull["url"] = json!(format!("https://{host}/o/r/pull/1"));
                        pull["reviewRequests"] = page(
                            vec![json!({"requestedReviewer": user(&viewer)})],
                            Some("requests-next"),
                        );
                        repository(vec![pull], Some("repo-next"))
                    }
                }
                REQUESTS_QUERY => {
                    assert_eq!(field("id"), Some("PR_1"));
                    assert_eq!(field("after"), Some("requests-next"));
                    json!({"node": {"reviewRequests": page(vec![json!({"requestedReviewer": team("shared-team-id")})], None)}})
                }
                MEMBERS_QUERY => {
                    assert_eq!(field("id"), Some("shared-team-id"));
                    assert_eq!(field("me"), Some(viewer.as_str()));
                    // The same team node ID can identify unrelated teams on different hosts.
                    let login = if host == PUBLIC_HOST { &viewer } else { "other" };
                    json!({"node": {"members": page(vec![json!({"login": login})], None)}})
                }
                HISTORY_QUERY => {
                    assert_eq!(field("id"), Some("PR_1"));
                    if field("after").is_some() {
                        assert_eq!(field("after"), Some("history-next"));
                        json!({"node": {"timelineItems": page(vec![requested(team("shared-team-id"), T2)], None)}})
                    } else {
                        json!({"node": {"timelineItems": page(vec![requested(user(&viewer), T1)], Some("history-next"))}})
                    }
                }
                query => panic!("unexpected query: {query}"),
            };
            let mut data = data;
            data["viewer"] = json!({"login": viewer});
            Ok(json!({"data": data}).to_string())
        })
        .unwrap();
        assert_eq!(calls, 21);
        assert!(fetched.errors.is_empty());
        assert_eq!(fetched.completed_repos, repos);
        assert_eq!(fetched.pending.len(), 3);
        let keys: HashSet<_> = fetched.pending.iter().map(PendingReview::key).collect();
        assert_eq!(keys.len(), 3);
        for review in fetched.pending {
            assert_eq!(review.url, format!("https://{}/o/r/pull/1", review.host));
            assert_eq!(
                review.requested_at.as_deref(),
                Some(if review.host == PUBLIC_HOST { T2 } else { T1 })
            );
        }
    }
}

#[test]
fn host_authentication_and_schema_failures_preserve_other_repositories() {
    for failure in ["auth", "empty-viewer", "schema"] {
        let healthy = RepositoryId::new(PUBLIC_HOST, "o/r");
        let failed = RepositoryId::new("github.example.com", "o/r");
        let watched = HashSet::from([healthy.clone(), failed.clone()]);
        let fetched = fetch_repositories_with(&watched, |args| {
            assert_eq!(&args[..2], ["api", "--hostname"]);
            if args[2] == failed.host {
                if failure == "auth" {
                    bail!("HTTP 401: bad credentials");
                }
                if args[3] == "user" {
                    return Ok(if failure == "empty-viewer" {
                        "\n"
                    } else {
                        "me"
                    }
                    .into());
                }
                return Ok(
                    r#"{"data":null,"errors":[{"message":"unsupported GraphQL field"}]}"#.into(),
                );
            }
            if args[3] == "user" {
                return Ok("me".into());
            }
            let mut data = repository(vec![], None);
            data["viewer"] = json!({"login": "me"});
            Ok(json!({"data": data}).to_string())
        })
        .unwrap();
        assert_eq!(fetched.completed_repos, HashSet::from([healthy]));
        assert_eq!(fetched.errors.len(), 1);
        assert!(fetched.errors[0].contains(&failed.host));
        let cached = PendingReview {
            host: failed.host.clone(),
            repo: "o/r".into(),
            number: 1,
            title: "Cached".into(),
            url: "https://github.example.com/o/r/pull/1".into(),
            author: "someone".into(),
            is_draft: false,
            rereview: false,
            requested_at: Some(T1.into()),
        };
        let mut store = crate::store::Store {
            pending: vec![cached.clone()],
            ..Default::default()
        };
        store.snooze(&cached.key(), 5, 0);
        store.reconcile_repositories(fetched.pending, &fetched.completed_repos, &watched);
        assert_eq!(store.pending, vec![cached.clone()]);
        assert!(store.snooze_for(&cached).is_some());
    }
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
    let fetched = fetch_with(&HashSet::from([RepositoryId::new(PUBLIC_HOST, "o/r")]), "me", &mut |query, variables| {
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
        &HashSet::from([RepositoryId::new(PUBLIC_HOST, "o/r")]),
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

#[test]
fn zero_repositories_probes_auth_without_claiming_a_successful_review_sync() {
    for (login, expected) in [
        (Ok("alice\n".into()), Readiness::Ready("alice".into())),
        (Err(anyhow::anyhow!("HTTP 401")), Readiness::SignedOut),
    ] {
        let mut login = Some(login);
        let fetched = fetch_repositories_with(&HashSet::new(), |args| {
            assert_eq!(
                args,
                ["api", "--hostname", PUBLIC_HOST, "user", "--jq", ".login"]
            );
            login.take().expect("only one readiness probe")
        })
        .unwrap();
        assert_eq!(fetched.readiness[PUBLIC_HOST], expected);
        assert!(fetched.pending.is_empty());
        assert!(fetched.completed_repos.is_empty());
        assert!(fetched.successful_hosts.is_empty());
        let mut store = crate::store::Store::default();
        let mut health = crate::health::SyncHealth::default();
        health.apply_hosts(&fetched, &mut store, 100);
        assert_eq!(store.last_successful_sync, None);
        assert!(!health.verified);
    }
}

#[test]
fn missing_signed_out_and_offline_cli_checks_preserve_success_time_and_recover() {
    let watched = HashSet::from([RepositoryId::new(PUBLIC_HOST, "o/r")]);
    for (script, expected) in [
        (None, Readiness::Missing),
        (
            Some("printf 'HTTP 401: gh auth login' >&2; exit 1"),
            Readiness::SignedOut,
        ),
        (
            Some("printf 'dial tcp: network is unreachable' >&2; exit 1"),
            Readiness::Offline,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-gh");
        let mut store = crate::store::Store::default();
        store.activate_host_account(PUBLIC_HOST, "me");
        store.last_successful_sync = Some(100);
        let failed = fetch_repositories_with(&watched, |_| {
            let mut command = if let Some(script) = script {
                let mut command = Command::new("/bin/sh");
                command.args(["-c", script]);
                command
            } else {
                Command::new(&path)
            };
            gh_output(
                &["api"],
                command_output(&mut command, Duration::from_secs(2))?,
            )
        })
        .unwrap();
        assert_eq!(failed.readiness[PUBLIC_HOST], expected);
        assert!(failed.completed_repos.is_empty());
        let mut health = crate::health::SyncHealth::default();
        health.apply_hosts(&failed, &mut store, 200);
        assert_eq!(store.last_successful_sync, Some(100));
        assert!(!health.verified);
        let recovered = fetch_repositories_with(&watched, |args| {
            if args[3] == "user" {
                return Ok("me".into());
            }
            let mut data = repository(vec![], None);
            data["viewer"] = json!({"login": "me"});
            Ok(json!({"data": data}).to_string())
        })
        .unwrap();
        health.apply_hosts(&recovered, &mut store, 300);
        assert!(health.verified);
        assert_eq!(store.last_successful_sync, Some(300));
    }
}

#[test]
fn account_change_on_a_history_page_discards_host_results_and_old_account_cache() {
    let watched = HashSet::from([RepositoryId::new(PUBLIC_HOST, "o/r")]);
    let fetched = fetch_repositories_with(&watched, |args| {
        if args[3] == "user" { return Ok("me".into()); }
        let query = args.iter().find_map(|arg| arg.strip_prefix("query=")).unwrap();
        let mut data = if query == REPOSITORY_QUERY {
            let mut data = repository(vec![pr(1, vec![user("me")])], None);
            data["viewer"] = json!({"login": "me"});
            data
        } else {
            json!({"viewer": {"login": "new-account"}, "node": {"timelineItems": page(vec![requested(user("me"), T2)], None)}})
        };
        // Keep the actual account identity present on each response.
        assert!(data.get_mut("viewer").is_some());
        Ok(json!({"data": data}).to_string())
    }).unwrap();
    assert_eq!(
        fetched.readiness[PUBLIC_HOST],
        Readiness::Ready("new-account".into())
    );
    assert!(fetched.pending.is_empty());
    assert!(fetched.completed_repos.is_empty());
    assert!(fetched.successful_hosts.is_empty());
    assert!(
        fetched
            .errors
            .iter()
            .any(|error| error.contains("account changed"))
    );
    let mut store = crate::store::Store::default();
    store.activate_host_account(PUBLIC_HOST, "me");
    store.pending = vec![PendingReview {
        host: PUBLIC_HOST.into(),
        repo: "o/r".into(),
        number: 1,
        title: "Old account".into(),
        author: "author".into(),
        url: "https://github.com/o/r/pull/1".into(),
        is_draft: false,
        rereview: false,
        requested_at: Some(T1.into()),
    }];
    store.queue_notifications(&store.pending.clone());
    store.snooze(&store.pending[0].key(), 30, 1_000);
    store.last_successful_sync = Some(100);
    let mut health = crate::health::SyncHealth::default();
    assert!(
        health
            .apply_hosts(&fetched, &mut store, 200)
            .contains(PUBLIC_HOST)
    );
    assert!(store.pending.is_empty());
    assert!(store.snoozed.is_empty());
    assert!(store.notification_queue.is_empty());
    assert_eq!(store.last_successful_sync, None);
    assert!(!health.verified);
}

#[test]
fn response_without_viewer_is_not_accepted_as_an_empty_success() {
    let watched = HashSet::from([RepositoryId::new(PUBLIC_HOST, "o/r")]);
    let fetched = fetch_repositories_with(&watched, |args| {
        if args[3] == "user" {
            return Ok("me".into());
        }
        Ok(json!({"data": repository(vec![], None)}).to_string())
    })
    .unwrap();
    assert!(fetched.completed_repos.is_empty());
    assert!(fetched.successful_hosts.is_empty());
    assert!(fetched.errors[0].contains("omitted the active account"));
}

#[test]
fn request_beyond_twenty_events_ends_old_snooze_and_queues_one_fresh_alert() {
    let watched = HashSet::from([RepositoryId::new(PUBLIC_HOST, "o/r")]);
    let mut store = crate::store::Store::default();
    store.activate_host_account(PUBLIC_HOST, "me");
    let old = PendingReview {
        host: PUBLIC_HOST.into(),
        repo: "o/r".into(),
        number: 1,
        title: "Review 1".into(),
        author: "author".into(),
        url: "https://github.com/o/r/pull/1".into(),
        is_draft: false,
        rereview: false,
        requested_at: Some(T1.into()),
    };
    store.reconcile(vec![old.clone()]);
    store.mark_delivered(&store.notification_queue.clone());
    store.snooze(&old.key(), 30, 1_000);
    let fetched = fetch_repositories_with(&watched, |args| {
        if args[3] == "user" {
            return Ok("me".into());
        }
        let field = |name: &str| {
            args.iter()
                .find_map(|arg| arg.strip_prefix(&format!("{name}=")))
        };
        let mut data = if field("query") == Some(REPOSITORY_QUERY) {
            repository(vec![pr(1, vec![user("me")])], None)
        } else if field("after").is_none() {
            let mut events = vec![requested(user("me"), T1)];
            events.extend((0..20).map(|_| requested(user("another-reviewer"), T3)));
            json!({"node": {"timelineItems": page(events, Some("older-request-page"))}})
        } else {
            assert_eq!(field("after"), Some("older-request-page"));
            json!({"node": {"timelineItems": page(vec![requested(user("me"), T2)], None)}})
        };
        data["viewer"] = json!({"login": "me"});
        Ok(json!({"data": data}).to_string())
    })
    .unwrap();
    assert!(fetched.errors.is_empty());
    let changed = store.reconcile_repositories(fetched.pending, &fetched.completed_repos, &watched);
    assert_eq!(changed.fresh.len(), 1);
    assert!(store.snoozed.is_empty());
    assert_eq!(store.notifications_due().len(), 1);
    assert_eq!(store.pending[0].requested_at.as_deref(), Some(T2));
}
