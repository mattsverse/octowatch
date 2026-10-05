mod discovery;
mod github;
mod store;
mod tray;
mod updater;

use std::{collections::{HashMap, HashSet}, path::PathBuf, time::Duration};

use chrono::{DateTime, Local};
use gpui::{
    App, Application, Bounds, ClickEvent, Context, Entity, FontWeight, Global, KeyBinding,
    PathPromptOptions, SharedString, Task, Window, WindowBounds, WindowOptions, actions, div,
    prelude::*, px, rgb, size,
};

use discovery::LocalRepo;
use store::{PendingReview, Store};
use tray::Tray;
use updater::Release;

const POLL_INTERVAL: Duration = Duration::from_secs(120);
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

actions!(octowatcher, [Quit]);

mod theme {
    pub const BASE: u32 = 0x1e1e2e;
    pub const SURFACE: u32 = 0x313244;
    pub const SURFACE_HOVER: u32 = 0x45475a;
    pub const TEXT: u32 = 0xcdd6f4;
    pub const SUBTEXT: u32 = 0xa6adc8;
    pub const MUTED: u32 = 0x6c7086;
    pub const ACCENT: u32 = 0x89b4fa;
    pub const GREEN: u32 = 0xa6e3a1;
    pub const PEACH: u32 = 0xfab387;
    pub const RED: u32 = 0xf38ba8;
}

enum Update {
    /// Installed over the running copy; a restart switches to it.
    Ready(semver::Version),
    /// Couldn't be installed in place; the release page has it.
    Manual(Release),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Reviews,
    Repositories,
}

struct Octowatcher {
    store: Store,
    /// `None` until the first scan of the roots finishes.
    repos: Option<Vec<LocalRepo>>,
    tab: Tab,
    last_checked: Option<DateTime<Local>>,
    error: Option<String>,
    tray: Option<Tray>,
    update: Option<Update>,
    scan_task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
    _poll_task: Task<()>,
    _update_task: Task<()>,
}

impl Octowatcher {
    fn new(cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        });
        let update_task = cx.spawn(async move |this, cx| {
            // A dev build would overwrite its own target dir with a release.
            if cfg!(debug_assertions) {
                return;
            }
            loop {
                let checked = cx.background_executor().spawn(async { updater::check() }).await;
                match checked {
                    Ok(Some(release)) => {
                        let Ok(installed) = this.read_with(cx, |this, _| {
                            matches!(&this.update, Some(Update::Ready(v)) if *v >= release.version)
                        }) else {
                            break;
                        };
                        if !installed {
                            let result = cx
                                .background_executor()
                                .spawn({
                                    let release = release.clone();
                                    async move { updater::install(&release) }
                                })
                                .await;
                            let updated = this.update(cx, |this, cx| {
                                this.update = Some(match result {
                                    Ok(path) => {
                                        // Linux relaunches the executable path, which
                                        // reads as deleted once it was replaced.
                                        cx.set_restart_path(path);
                                        Update::Ready(release.version)
                                    }
                                    Err(err) => {
                                        eprintln!("could not install update: {err:#}");
                                        Update::Manual(release)
                                    }
                                });
                                this.sync_tray();
                                cx.notify();
                            });
                            if updated.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => eprintln!("could not check for updates: {err:#}"),
                }
                cx.background_executor().timer(UPDATE_INTERVAL).await;
            }
        });
        let store = Store::load();
        let (tray, error) = match Tray::new(&store.pending) {
            Ok(tray) => (Some(tray), None),
            Err(err) => (None, Some(format!("could not create tray icon: {err:#}"))),
        };
        let mut this = Self {
            store,
            repos: None,
            tab: Tab::Reviews,
            last_checked: None,
            error,
            tray,
            update: None,
            scan_task: None,
            fetch_task: None,
            _poll_task: poll_task,
            _update_task: update_task,
        };
        this.rescan(cx);
        this
    }

    /// Rediscovers local clones, then checks GitHub again.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        let roots = self.store.roots.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let repos = cx
                .background_executor()
                .spawn(async move { discovery::discover(&roots) })
                .await;
            this.update(cx, |this, cx| {
                this.repos = Some(repos);
                this.scan_task = None;
                // Results fetched against the old repo list are stale.
                this.fetch_task = None;
                this.refresh(cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Filtering needs the repo list, and the scan refreshes once it's done.
        if self.repos.is_none() || self.fetch_task.is_some() {
            return;
        }
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { github::fetch_awaiting_reviews() })
                .await;
            this.update(cx, |this, cx| {
                this.fetch_task = None;
                this.last_checked = Some(Local::now());
                match result {
                    Ok(fetched) => {
                        this.error = None;
                        this.reconcile(fetched, cx);
                    }
                    Err(err) => this.error = Some(format!("{err:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Replaces the pending list with what GitHub reports now, keeping only
    /// enabled local repos. A PR that drops out (reviewed, request removed,
    /// closed) is gone; one seen for the first time raises a notification.
    fn reconcile(&mut self, fetched: Vec<PendingReview>, cx: &mut Context<Self>) {
        let watched = self.watched_slugs();
        let mut fetched: Vec<PendingReview> = fetched
            .into_iter()
            .filter(|pr| watched.contains(&pr.repo.to_lowercase()))
            .collect();
        fetched.sort_by(|a, b| b.requested_at.cmp(&a.requested_at));

        // A request is new when the PR wasn't listed, or when it was asked
        // again after the request already on file.
        let known: HashMap<_, _> = self
            .store
            .pending
            .iter()
            .map(|pr| (pr.key(), pr.requested_at.clone()))
            .collect();
        let fresh: Vec<PendingReview> = fetched
            .iter()
            .filter(|pr| match known.get(&pr.key()) {
                None => true,
                Some(previous) => pr.requested_at > *previous,
            })
            .cloned()
            .collect();

        if fetched != self.store.pending {
            self.store.pending = fetched;
            self.save();
            self.sync_tray();
        }
        if !fresh.is_empty() {
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { notify(&fresh) })
                    .await;
                if let Err(err) = result {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("could not send notification: {err:#}"));
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    fn watched_slugs(&self) -> HashSet<String> {
        self.repos
            .iter()
            .flatten()
            .filter(|repo| self.store.is_enabled(&repo.slug))
            .map(|repo| repo.slug.to_lowercase())
            .collect()
    }

    fn toggle_repo(&mut self, slug: &str, cx: &mut Context<Self>) {
        let key = slug.to_lowercase();
        if self.store.disabled.remove(&key) {
            self.save();
            self.refresh(cx);
        } else {
            self.store.disabled.insert(key.clone());
            self.store
                .pending
                .retain(|pr| pr.repo.to_lowercase() != key);
            self.save();
            self.sync_tray();
        }
        cx.notify();
    }

    fn add_root(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Watch folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| {
                for path in paths {
                    if !this.store.roots.contains(&path) {
                        this.store.roots.push(path);
                    }
                }
                this.save();
                this.rescan(cx);
            })
            .ok();
        })
        .detach();
    }

    fn remove_root(&mut self, root: &PathBuf, cx: &mut Context<Self>) {
        self.store.roots.retain(|r| r != root);
        self.save();
        self.rescan(cx);
    }

    fn sync_tray(&mut self) {
        let Some(tray) = &self.tray else { return };
        let ready = match &self.update {
            Some(Update::Ready(version)) => Some(version),
            _ => None,
        };
        if let Err(err) = tray.update(&self.store.pending, ready) {
            self.error = Some(format!("could not update tray menu: {err:#}"));
        }
    }

    fn save(&mut self) {
        if let Err(err) = self.store.save() {
            self.error = Some(format!("could not save state: {err:#}"));
        }
    }
}

fn notify(fresh: &[PendingReview]) -> notify_rust::error::Result<()> {
    let (summary, body) = match fresh {
        [pr] => (
            format!(
                "{} requested your {}",
                pr.author,
                if pr.rereview { "re-review" } else { "review" }
            ),
            format!("{}#{}: {}", pr.repo, pr.number, pr.title),
        ),
        many => (
            format!("{} pull requests need your review", many.len()),
            many.iter()
                .map(|pr| format!("{}#{}", pr.repo, pr.number))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    };
    notify_rust::Notification::new()
        .summary(&summary)
        .body(&body)
        .show()
        .map(drop)
}

impl Render for Octowatcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.tab {
            Tab::Reviews => self.render_reviews(cx).into_any_element(),
            Tab::Repositories => self.render_repositories(cx).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(theme::BASE))
            .text_color(rgb(theme::TEXT))
            .text_sm()
            .child(self.render_header(cx))
            .child(
                div()
                    .id("content")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_4()
                    .child(content),
            )
    }
}

impl Octowatcher {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status: SharedString = if self.scan_task.is_some() {
            "Scanning folders…".into()
        } else if self.fetch_task.is_some() {
            "Checking GitHub…".into()
        } else if let Some(at) = self.last_checked {
            format!("Checked at {}", at.format("%H:%M")).into()
        } else {
            "".into()
        };
        let repo_count = self.repos.as_ref().map_or(0, Vec::len);

        div()
            .flex()
            .flex_col()
            .gap_3()
            .px_4()
            .pt_4()
            .pb_3()
            .border_b_1()
            .border_color(rgb(theme::SURFACE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Octowatcher"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(status))
                            .child(button("refresh", "Refresh").on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| this.refresh(cx),
                            ))),
                    ),
            )
            .children(self.error.clone().map(|err| {
                div()
                    .text_xs()
                    .text_color(rgb(theme::RED))
                    .child(err)
            }))
            .children(self.render_update(cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.render_tab(
                        Tab::Reviews,
                        format!("Reviews ({})", self.store.pending.len()),
                        cx,
                    ))
                    .child(self.render_tab(
                        Tab::Repositories,
                        format!("Repositories ({repo_count})"),
                        cx,
                    )),
            )
    }

    fn render_update(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (message, action) = match self.update.as_ref()? {
            Update::Ready(version) => (
                format!("Octowatcher {version} is installed."),
                button("restart", "Restart")
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.restart())),
            ),
            Update::Manual(release) => {
                let url = release.url.clone();
                (
                    format!("Octowatcher {} is available.", release.version),
                    button("download-update", "Download")
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.open_url(&url))),
                )
            }
        };
        Some(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(theme::SURFACE))
                .text_xs()
                .child(message)
                .child(action),
        )
    }

    fn render_tab(&self, tab: Tab, label: String, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.tab == tab;
        let id = match tab {
            Tab::Reviews => "tab-reviews",
            Tab::Repositories => "tab-repositories",
        };
        div()
            .id(id)
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(active, |s| s.bg(rgb(theme::SURFACE)).text_color(rgb(theme::TEXT)))
            .when(!active, |s| {
                s.text_color(rgb(theme::SUBTEXT))
                    .hover(|s| s.bg(rgb(theme::SURFACE)))
            })
            .child(label)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.tab = tab;
                cx.notify();
            }))
    }

    fn render_reviews(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.store.pending.is_empty() {
            return div()
                .flex()
                .justify_center()
                .pt_16()
                .text_color(rgb(theme::MUTED))
                .child("Nothing waiting on your review.");
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(self.store.pending.iter().enumerate().map(|(ix, pr)| {
                let url = pr.url.clone();
                let badge = if pr.rereview {
                    Some(("re-review", theme::PEACH))
                } else {
                    None
                };
                div()
                    .id(("review", ix))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(theme::SURFACE))
                    .hover(|s| s.bg(rgb(theme::SURFACE_HOVER)))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(rgb(theme::SUBTEXT))
                            .child(format!("{}#{}", pr.repo, pr.number))
                            .children(badge.map(|(label, color)| pill(label, color)))
                            .when(pr.is_draft, |s| s.child(pill("draft", theme::MUTED))),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(pr.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(format!("by {}", pr.author)),
                    )
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.open_url(&url)))
            }))
    }

    fn render_repositories(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let roots = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("Watched folders"))
            .children(self.store.roots.iter().enumerate().map(|(ix, root)| {
                let root_for_click = root.clone();
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE))
                    .child(display_path(root))
                    .child(button(("remove-root", ix), "Remove").on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| this.remove_root(&root_for_click, cx),
                    )))
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button("add-root", "Add folder…").on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| this.add_root(cx),
                    )))
                    .child(button("rescan", "Rescan").on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| this.rescan(cx),
                    ))),
            );

        let repos = self.repos.as_deref().unwrap_or_default();
        let list = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("GitHub repositories found"))
            .when(repos.is_empty() && self.scan_task.is_none(), |s| {
                s.child(
                    div()
                        .text_color(rgb(theme::MUTED))
                        .child("No GitHub clones in these folders."),
                )
            })
            .children(repos.iter().enumerate().map(|(ix, repo)| {
                let enabled = self.store.is_enabled(&repo.slug);
                let slug = repo.slug.clone();
                let paths = repo
                    .paths
                    .iter()
                    .map(|p| display_path(p))
                    .collect::<Vec<_>>()
                    .join(", ");
                div()
                    .id(("repo", ix))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE))
                    .hover(|s| s.bg(rgb(theme::SURFACE_HOVER)))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .when(!enabled, |s| s.text_color(rgb(theme::MUTED)))
                            .child(repo.slug.clone())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::MUTED))
                                    .truncate()
                                    .child(paths),
                            ),
                    )
                    .child(if enabled {
                        pill("watching", theme::GREEN)
                    } else {
                        pill("off", theme::MUTED)
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_repo(&slug, cx)
                    }))
            }));

        div().flex().flex_col().gap_6().child(roots).child(list)
    }
}

fn button(id: impl Into<gpui::ElementId>, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(rgb(theme::SURFACE))
        .text_color(rgb(theme::ACCENT))
        .hover(|s| s.bg(rgb(theme::SURFACE_HOVER)))
        .cursor_pointer()
        .child(label)
}

fn pill(label: &'static str, color: u32) -> gpui::Div {
    div()
        .px_2()
        .rounded_full()
        .text_xs()
        .border_1()
        .border_color(rgb(color))
        .text_color(rgb(color))
        .child(label)
}

fn section_title(label: &'static str) -> gpui::Div {
    div()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(theme::SUBTEXT))
        .child(label)
}

fn display_path(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from)) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

fn main() {
    // An unbundled binary has no identity of its own, so notifications borrow
    // Terminal's. Left unset, the library looks up an app named "use_default"
    // via AppleScript and macOS asks the user where that app is.
    #[cfg(target_os = "macos")]
    let _ = notify_rust::set_application("com.apple.Terminal");

    let app = Application::new();
    // Clicking the dock icon with the window closed brings it back.
    app.on_reopen(show_window);
    app.run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus(vec![gpui::Menu {
            name: "Octowatcher".into(),
            items: vec![gpui::MenuItem::action("Quit", Quit)],
        }]);
        tray::listen(cx);

        // The app owns the state rather than the window, so closing the
        // window keeps polling and the tray icon alive until Quit.
        let octowatcher = cx.new(Octowatcher::new);
        cx.set_global(MainView(octowatcher));
        show_window(cx);
    });
}

struct MainView(Entity<Octowatcher>);

impl Global for MainView {}

/// Brings the window to the front, opening it again if it was closed.
pub fn show_window(cx: &mut App) {
    cx.activate(true);
    if let Some(window) = cx.windows().first() {
        window
            .update(cx, |_, window, _| window.activate_window())
            .ok();
        return;
    }
    let view = cx.global::<MainView>().0.clone();
    let bounds = Bounds::centered(None, size(px(560.), px(680.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |_, _| view,
    )
    .unwrap();
}
