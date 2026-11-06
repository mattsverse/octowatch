//! The Repositories tab: the folders scanned for clones, and the GitHub repositories found.

use octowatcher_core::{engine::Command, repository::PUBLIC_HOST};
use rune::{El, Tx, col, row, text};
use rune_kit::{IconButtonVariant, Menu, MenuItem, Placement};

use super::icons;
use super::widgets::{self, META, Row, banner, glyph_tile, monogram, section};
use crate::app::{Octowatcher, Popup, display_path};
use crate::theme::Theme;

pub fn view(s: &Octowatcher, t: &Theme, layer: &mut Vec<El>) -> El {
    let store = s.engine.store();
    let mut folders: Vec<El> = store
        .roots
        .iter()
        .enumerate()
        .map(|(ix, root)| {
            let path = display_path(root);
            let remove = root.clone();
            let found = s
                .engine
                .repos()
                .unwrap_or_default()
                .iter()
                .filter(|repo| repo.paths.iter().any(|p| p.starts_with(root)))
                .count();
            Row::new(format!("root-{ix}"), t, path.clone())
                .leading(glyph_tile(t, &icons::FOLDER, t.accent))
                .detail(
                    t,
                    match found {
                        0 => "No GitHub clones".to_string(),
                        1 => "1 repository".into(),
                        n => format!("{n} repositories"),
                    },
                )
                .trailing(
                    super::widgets::icon_button(&icons::CLOSE)
                        .variant(IconButtonVariant::GhostMuted)
                        .label(format!("Stop watching {path}"))
                        .help("Stop watching this folder")
                        .on_press_tx(move |s: &mut Octowatcher, tx: &mut Tx| {
                            s.command(Command::RemoveRoot(remove.clone()), tx)
                        })
                        .build(&t.k)
                        .key("remove")
                        .group_opacity(0.85, 1.0),
                )
                .build()
                .hover_group()
        })
        .collect();
    if folders.is_empty() {
        folders.push(
            Row::new("no-roots", t, "No watched folders")
                .leading(glyph_tile(t, &icons::FOLDER, t.muted))
                .detail(t, "Add a folder that contains your GitHub clones.")
                .build(),
        );
    }
    folders.push(
        row()
            .key("folder-actions")
            .w_full()
            .h(52.0)
            .pl(widgets::TEXT)
            .gap(8.0)
            .center_y()
            .child(
                super::widgets::button("Add Folder…")
                    .icon(&icons::PLUS)
                    .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| s.add_root(tx))
                    .build(&t.k)
                    .key("add-root"),
            )
            .child(
                super::widgets::button(if s.engine.scanning() {
                    "Scanning…"
                } else {
                    "Rescan Folders"
                })
                .help("Look for new or removed clones in these folders")
                .help("Look for new or removed clones in these folders")
                .icon(&icons::FOLDER_SEARCH)
                .disabled(s.engine.scanning())
                .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| s.command(Command::Rescan, tx))
                .build(&t.k)
                .key("rescan"),
            ),
    );

    let mut page = col().key("repositories").w_full().gap(22.0).child(section(
        t,
        "roots",
        Some("Watched folders"),
        folders,
    ));
    for (ix, issue) in s.engine.scan_issues().iter().enumerate() {
        page = page.child(banner(
            t,
            &format!("scan-issue-{ix}"),
            &icons::ALERT,
            t.warning,
            format!(
                "{}: {}. Repositories found there earlier stay listed; Octowatcher retries on the next check.",
                display_path(&issue.path),
                issue.message
            ),
            None,
        ));
    }

    let repos = s.engine.repos().unwrap_or_default();
    let accounts = &store.known_accounts;
    let mut rows: Vec<El> = repos
        .iter()
        .enumerate()
        .map(|(ix, repo)| {
            let slug = repo.id.store_key();
            let enabled = store.is_enabled(&slug);
            let (owner, name) = repo.id.slug.split_once('/').unwrap_or(("", &repo.id.slug));
            let paths = repo
                .paths
                .iter()
                .map(|p| display_path(p))
                .collect::<Vec<_>>()
                .join(", ");
            let host = if repo.id.host != PUBLIC_HOST {
                format!("{} · ", repo.id.host)
            } else {
                Default::default()
            };
            let title = row()
                .key("title")
                .min_w(0.0)
                .child(
                    text(format!("{host}{owner}/"))
                        .font(widgets::BODY)
                        .color(t.muted),
                )
                .child(
                    text(name.to_string())
                        .font(widgets::BODY_MEDIUM)
                        .color(if enabled { t.text } else { t.muted })
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                );
            let toggle = slug.clone();
            // The default scope (every account) reads in secondary text; a narrower scope in
            // primary text, so it stands out.
            let accounts_menu = (!enabled && repo.id.host == PUBLIC_HOST && accounts.len() > 1)
                .then(|| {
                    // Off repositories say so in the column instead of showing a dead control.
                    text("Not watched")
                        .key("accounts")
                        .font(META)
                        .color(t.muted)
                        .w(widgets::POPUP)
                        .pl(8.0)
                })
                .or(
                    (enabled && repo.id.host == PUBLIC_HOST && accounts.len() > 1).then(|| {
                        let default = store.all_repo_accounts(&repo.id.slug)
                            && s.popup != Some(Popup::RepoAccounts(repo.id.slug.clone()));
                        account_menu(s, t, &repo.id.slug, layer, !default)
                    }),
                );
            let switch = super::widgets::switch(
                t,
                "watch",
                format!("Watch {}", repo.id),
                enabled,
                true,
                move |s: &mut Octowatcher, tx: &mut Tx| {
                    s.command(Command::ToggleRepo(toggle.clone()), tx)
                },
            );
            let mut r = Row::new(format!("repo-{ix}"), t, repo.id.to_string())
                .leading(monogram(t, owner, s.owner_slot(owner), !enabled))
                .title_el(title)
                .detail_el(
                    text(if paths.is_empty() {
                        "No local clone".into()
                    } else {
                        paths
                    })
                    .key("detail")
                    .font(META)
                    .color(if enabled { t.muted } else { t.faint })
                    .truncate_middle(),
                )
                .trailing(
                    row()
                        .key("controls")
                        .gap(10.0)
                        .center_y()
                        .children(accounts_menu)
                        .child(switch),
                );
            let mut below = col().key("below").gap(6.0);
            let mut has_below = false;
            let none = enabled
                && repo.id.host == PUBLIC_HOST
                && !store.all_repo_accounts(&repo.id.slug)
                && !accounts
                    .iter()
                    .any(|account| store.repo_account_selected(&repo.id.slug, account));
            if none {
                has_below = true;
                below = below.child(
                    text("No accounts selected, so nothing is watched here.")
                        .font(META)
                        .color(t.warning),
                );
            }
            for ((_, failed), error) in s.engine.repo_errors() {
                if failed == &slug {
                    has_below = true;
                    below =
                        below.child(text(error.clone()).font(META).color(t.danger).max_lines(3));
                }
            }
            if has_below {
                r = r.below(below);
            }
            r.build().hover_group()
        })
        .collect();
    if rows.is_empty() {
        let message = if s.engine.scanning() || s.engine.repos().is_none() {
            "Looking for GitHub clones…"
        } else {
            "No GitHub clones in these folders."
        };
        rows.push(
            Row::new("no-repos", t, message)
                .leading(glyph_tile(t, &icons::PULL_REQUEST, t.muted))
                .build(),
        );
    }
    let watched = repos
        .iter()
        .filter(|repo| store.is_enabled(&repo.id.store_key()))
        .count();
    let heading = if repos.is_empty() {
        "Repositories".to_string()
    } else {
        format!("Repositories · {watched} of {} watched", repos.len())
    };
    let multi = accounts.len() > 1;
    page.child(
        col()
            .key("repos-section")
            .w_full()
            .gap(7.0)
            .child(
                row()
                    .key("repos-heading")
                    .w_full()
                    .justify_between()
                    .child(
                        text(heading)
                            .font(widgets::SECTION)
                            .color(t.muted)
                            .ml(widgets::INSET)
                            .role("heading"),
                    )
                    // Labels the account column, aligned to its pop-ups' left edge.
                    .children(multi.then(|| {
                        text("Monitor with")
                            .font(widgets::SECTION)
                            .color(t.muted)
                            .w(widgets::POPUP)
                            .mr(widgets::INSET + 34.0 + 10.0)
                    })),
            )
            .child(section(t, "repos", None, rows)),
    )
}

/// Which saved accounts watch a repository: all of them, or the ones picked, in a menu.
fn account_menu(
    s: &Octowatcher,
    t: &Theme,
    repo: &str,
    layer: &mut Vec<El>,
    emphasised: bool,
) -> El {
    let store = s.engine.store();
    let all = store.all_repo_accounts(repo);
    let picked: Vec<&String> = store
        .known_accounts
        .iter()
        .filter(|account| store.repo_account_selected(repo, account))
        .collect();
    let summary = if all {
        "All accounts".to_string()
    } else {
        match picked.as_slice() {
            [] => "No accounts".into(),
            [one] => format!("@{one}"),
            many => format!("{} accounts", many.len()),
        }
    };
    let popup = Popup::RepoAccounts(repo.to_string());
    let open = s.popup.as_ref() == Some(&popup);
    let trigger = widgets::popup_button(
        t,
        emphasised,
        "accounts",
        summary,
        "Accounts that monitor this repository",
        open,
        move |s: &mut Octowatcher| s.toggle_popup(popup.clone()),
    );
    let slug = repo.to_string();
    let mut menu = Menu::new(format!("accounts-menu-{repo}"))
        .trigger(trigger)
        .open(open)
        .placement(Placement::BottomEnd)
        .min_width(200.0)
        .group_label("Monitor with")
        .on_close(|s: &mut Octowatcher| {
            if matches!(s.popup, Some(Popup::RepoAccounts(_))) {
                s.popup = None;
            }
        })
        .item(
            MenuItem::new("All enabled accounts")
                .checked(all)
                .on_select_tx(move |s: &mut Octowatcher, tx: &mut Tx| {
                    s.command(Command::AllRepoAccounts(slug.clone()), tx)
                }),
        )
        .separator();
    for account in &store.known_accounts {
        let (repo, login) = (repo.to_string(), account.clone());
        menu = menu.item(
            MenuItem::new(format!("@{account}"))
                .checked(!all && store.repo_account_selected(&repo, account))
                .on_select_tx(move |s: &mut Octowatcher, tx: &mut Tx| {
                    s.command(
                        Command::ToggleRepoAccount {
                            repo: repo.clone(),
                            account: login.clone(),
                        },
                        tx,
                    )
                }),
        );
    }
    menu.build(&t.k, layer)
}
