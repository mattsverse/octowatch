//! Session-only view state. Filtering never mutates the monitored Store.

use std::collections::{BTreeMap, HashSet};

use crate::store::{PendingReview, Store};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DraftFilter {
    #[default]
    All,
    Ready,
    Draft,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReviewFilter {
    #[default]
    All,
    First,
    Rereview,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SnoozeFilter {
    #[default]
    All,
    Awake,
    Snoozed,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReviewFilters {
    pub query: String,
    /// An exact owner/repository selection, independent of watched repositories.
    pub repository: Option<String>,
    pub draft: DraftFilter,
    pub review: ReviewFilter,
    pub snooze: SnoozeFilter,
}

impl ReviewFilters {
    /// Indices into pending, preserving its newest-request-first ordering.
    pub fn visible_indices(&self, store: &Store) -> Vec<usize> {
        let terms: Vec<_> = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let snoozed: HashSet<_> = store.snoozed.iter().map(|s| s.key()).collect();
        store
            .pending
            .iter()
            .enumerate()
            .filter_map(|(ix, pr)| {
                let asleep = snoozed.contains(&pr.key());
                let matches = self
                    .repository
                    .as_ref()
                    .is_none_or(|repo| repo.eq_ignore_ascii_case(&pr.repo))
                    && match self.draft {
                        DraftFilter::All => true,
                        DraftFilter::Ready => !pr.is_draft,
                        DraftFilter::Draft => pr.is_draft,
                    }
                    && match self.review {
                        ReviewFilter::All => true,
                        ReviewFilter::First => !pr.rereview,
                        ReviewFilter::Rereview => pr.rereview,
                    }
                    && match self.snooze {
                        SnoozeFilter::All => true,
                        SnoozeFilter::Awake => !asleep,
                        SnoozeFilter::Snoozed => asleep,
                    }
                    && matches_terms(pr, &terms);
                matches.then_some(ix)
            })
            .collect()
    }

    /// Choices come from the whole queue, unaffected by other filters. Keep a
    /// selected repository even if its last PR leaves during a refresh.
    pub fn repositories(&self, store: &Store) -> Vec<String> {
        let mut repos = BTreeMap::new();
        for repo in store
            .pending
            .iter()
            .map(|pr| &pr.repo)
            .chain(self.repository.iter())
        {
            repos
                .entry(repo.to_lowercase())
                .or_insert_with(|| repo.clone());
        }
        repos.into_values().collect()
    }
}

fn matches_terms(pr: &PendingReview, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let fields = [
        pr.title.to_lowercase(),
        pr.repo.to_lowercase(),
        pr.author.to_lowercase(),
        pr.number.to_string(),
    ];
    terms.iter().all(|term| {
        if let Some(number) = term.strip_prefix('#') {
            !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit())
                && number.parse::<u64>().ok() == Some(pr.number)
        } else {
            fields.iter().any(|field| field.contains(term))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Snooze;

    fn pr(
        repo: &str,
        number: u64,
        title: &str,
        author: &str,
        draft: bool,
        rereview: bool,
    ) -> PendingReview {
        PendingReview {
            repo: repo.into(),
            number,
            title: title.into(),
            author: author.into(),
            url: format!("https://github.com/{repo}/pull/{number}"),
            is_draft: draft,
            rereview,
            requested_at: Some(format!("2026-01-{:02}T00:00:00Z", number % 28 + 1)),
        }
    }

    fn queue() -> Store {
        Store {
            pending: vec![
                pr("Acme/API", 123, "Fix café login", "Alice", false, false),
                pr("acme/api", 1234, "Login experiment", "Bob", true, true),
                pr("Other/Web", 7, "Fix issue 123", "Alice", false, true),
                pr("Other/Web", 8, "Login polish", "Chloé", true, false),
            ],
            snoozed: vec![Snooze {
                repo: "acme/api".into(),
                number: 1234,
                until: 200,
                requested_at: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn search_is_case_insensitive_and_terms_can_span_fields() {
        let cases = [
            ("", vec![0, 1, 2, 3]),
            ("  \t ", vec![0, 1, 2, 3]),
            ("LOGIN", vec![0, 1, 3]),
            ("ACME/API", vec![0, 1]),
            ("alice", vec![0, 2]),
            ("CAFÉ", vec![0]),
            ("CHLOÉ", vec![3]),
            ("  acme   ALICE login  ", vec![0]),
            ("alice experiment", vec![]),
            ("123", vec![0, 1, 2]),
            ("#123", vec![0]),
            ("#00123", vec![0]),
            ("#1234 bob", vec![1]),
            ("#123 bob", vec![]),
            ("#", vec![]),
            ("#nope", vec![]),
            ("#18446744073709551616", vec![]),
        ];
        for (query, expected) in cases {
            assert_eq!(
                ReviewFilters {
                    query: query.into(),
                    ..Default::default()
                }
                .visible_indices(&queue()),
                expected,
                "{query:?}"
            );
        }
    }

    #[test]
    fn all_filters_default_to_all_including_snoozed() {
        assert_eq!(
            ReviewFilters::default().visible_indices(&queue()),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            ReviewFilters {
                draft: DraftFilter::Ready,
                ..Default::default()
            }
            .visible_indices(&queue()),
            vec![0, 2]
        );
        assert_eq!(
            ReviewFilters {
                draft: DraftFilter::Draft,
                ..Default::default()
            }
            .visible_indices(&queue()),
            vec![1, 3]
        );
        assert_eq!(
            ReviewFilters {
                review: ReviewFilter::First,
                ..Default::default()
            }
            .visible_indices(&queue()),
            vec![0, 3]
        );
        assert_eq!(
            ReviewFilters {
                review: ReviewFilter::Rereview,
                ..Default::default()
            }
            .visible_indices(&queue()),
            vec![1, 2]
        );
        assert_eq!(
            ReviewFilters {
                snooze: SnoozeFilter::Awake,
                ..Default::default()
            }
            .visible_indices(&queue()),
            vec![0, 2, 3]
        );
        assert_eq!(
            ReviewFilters {
                snooze: SnoozeFilter::Snoozed,
                ..Default::default()
            }
            .visible_indices(&queue()),
            vec![1]
        );
    }

    #[test]
    fn search_and_independent_filters_are_intersections() {
        let mut filters = ReviewFilters {
            query: "LOGIN bob".into(),
            repository: Some("ACME/api".into()),
            draft: DraftFilter::Draft,
            review: ReviewFilter::Rereview,
            snooze: SnoozeFilter::Snoozed,
        };
        assert_eq!(filters.visible_indices(&queue()), vec![1]);
        filters.snooze = SnoozeFilter::Awake;
        assert!(filters.visible_indices(&queue()).is_empty());
        filters.snooze = SnoozeFilter::All;
        filters.repository = Some("other/web".into());
        assert!(filters.visible_indices(&queue()).is_empty());
        filters = ReviewFilters::default();
        assert_eq!(filters.visible_indices(&queue()), vec![0, 1, 2, 3]);
    }

    #[test]
    fn view_filtering_does_not_change_store_awake_counts_or_fresh_requests() {
        let mut store = queue();
        let before = serde_json::to_string(&store).unwrap();
        let filters = ReviewFilters {
            query: "not present".into(),
            snooze: SnoozeFilter::Snoozed,
            ..Default::default()
        };
        assert!(filters.visible_indices(&store).is_empty());
        assert_eq!(store.awake().len(), 3);
        assert_eq!(store.pending.len(), 4);
        assert_eq!(serde_json::to_string(&store).unwrap(), before);
        let new_pr = pr(
            "Other/Web",
            99,
            "Unmatched but notify",
            "Someone",
            false,
            false,
        );
        let mut fetched = store.pending.clone();
        fetched.push(new_pr.clone());
        assert_eq!(store.reconcile(fetched).fresh, vec![new_pr]);
        assert!(filters.visible_indices(&store).is_empty());
    }

    #[test]
    fn snooze_expiry_and_unsnooze_change_visibility_without_changing_filters() {
        let mut store = queue();
        let filters = ReviewFilters {
            snooze: SnoozeFilter::Snoozed,
            ..Default::default()
        };
        assert_eq!(filters.visible_indices(&store), vec![1]);
        assert_eq!(store.take_expired(200).len(), 1);
        assert!(filters.visible_indices(&store).is_empty());
        assert_eq!(filters.snooze, SnoozeFilter::Snoozed);
        assert_eq!(store.awake().len(), 4);
        store = queue();
        store.snoozed.clear();
        assert!(filters.visible_indices(&store).is_empty());
    }

    #[test]
    fn refresh_preserves_selection_even_if_repository_disappears() {
        let mut store = queue();
        let filters = ReviewFilters {
            query: "login".into(),
            repository: Some("Acme/API".into()),
            ..Default::default()
        };
        assert_eq!(filters.repositories(&store), vec!["Acme/API", "Other/Web"]);
        store.reconcile(vec![pr(
            "Other/Web",
            99,
            "Login change",
            "Alice",
            false,
            false,
        )]);
        assert!(filters.visible_indices(&store).is_empty());
        assert_eq!(filters.repositories(&store), vec!["Acme/API", "Other/Web"]);
        store.reconcile(vec![pr("acme/api", 100, "LOGIN fix", "Bob", false, false)]);
        assert_eq!(filters.visible_indices(&store), vec![0]);
        assert_eq!(filters.query, "login");
        assert_eq!(filters.repository.as_deref(), Some("Acme/API"));
    }

    #[test]
    fn large_queue_keeps_order_and_search_reaches_the_end() {
        let store = Store {
            pending: (1..=10_000)
                .rev()
                .map(|number| {
                    pr(
                        "acme/api",
                        number,
                        "Change",
                        "Alice",
                        number % 2 == 0,
                        false,
                    )
                })
                .collect(),
            ..Default::default()
        };
        let all = ReviewFilters::default().visible_indices(&store);
        assert_eq!(all.len(), 10_000);
        assert_eq!(all[0], 0);
        assert_eq!(all[9_999], 9_999);
        let filters = ReviewFilters {
            query: "acme ALICE #1".into(),
            draft: DraftFilter::Ready,
            ..Default::default()
        };
        assert_eq!(filters.visible_indices(&store), vec![9_999]);
    }
}
