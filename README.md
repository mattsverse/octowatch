# Octowatcher

Octowatcher sits in your menu bar and tells you when someone asks you to review a pull request in a repository you have cloned on your machine.

It finds the GitHub repositories in your project folders, checks GitHub every few minutes, and sends a desktop notification for each new review request. The tray icon shows how many reviews are waiting, and clicking one opens the pull request in your browser.

## Features

- **Uses the repositories you already have.** Octowatcher scans folders such as `~/Dev` for git clones with a GitHub remote, so you only hear about the repositories you work on. You can switch any of them off.
- **Notifies you about new requests only.** You get one notification when a review is requested, and another if you are asked to re-review after you've already left a review. A pull request leaves the list once you review it, the request is withdrawn, or the PR is closed.
- **Includes team requests.** It checks membership for each requested team, including child-team membership. The request clears once you submit a review, even if the rest of the team hasn't. A later request to you or a team you belong to brings it back; requests to unrelated teams don't.
- **Checks the whole queue.** It checks enabled local repositories directly and follows every page of open pull requests, requested reviewers and review history. Reviews from other repositories can't crowd yours out of a global search, and requests older than the latest 20 history events still count.
- **Snoozes a review for later.** Snooze a pull request from its notification or from the window. It leaves the tray until the snooze runs out, then notifies you again. A new review request on it ends the snooze early.
- **Lets you mute review alerts.** Polling and the queue keep updating while muted. Resume when you're ready to get one catch-up alert for undelivered requests still waiting.
- **Lets you choose draft alerts.** Drafts always stay in the queue; switch their notifications off to wait until they're ready for review.
- **Opens reviews from notifications.** Click a single-review notification to open its PR, or a summary to open the Reviews tab.
- **Lives in the tray.** The icon shows how many reviews are waiting, and its menu lists them, marked `(draft)` or `(re-review)` where that applies.
- **Supports Enterprise hosts.** Watch clones from github.com, GitHub Enterprise Server, and Enterprise Cloud with data residency (`*.ghe.com`) together. Repositories and reviews with the same name on different hosts stay separate.
- **Follows your desktop appearance.** Use System, Light or Dark in Settings. System is the default, and your choice survives restarts.
- **Stores no token.** Octowatcher talks to GitHub through the [GitHub CLI](https://cli.github.com/), so it uses the login you already have.
- **Updates itself.** It looks for a new release every six hours, or right away when you choose **Check for Updates…** from the tray menu. When one is out, a notification offers **Update**. On macOS, and on Linux when you run the AppImage, that installs it and then asks whether to restart now or later.

## Requirements

- macOS, or Linux with a desktop that shows tray icons (AppIndicator).
- The [GitHub CLI](https://cli.github.com/) (`gh`) **2.81 or newer**, logged in:

  ```sh
  gh auth login --hostname github.com
  ```

Octowatcher looks for `gh` in `/opt/homebrew/bin`, `/usr/local/bin` and `/usr/bin`, and then on your `PATH`.

## Install

Download the latest build from the [releases page](https://github.com/mattsverse/octowatch/releases/latest).

### macOS

1. Open the `.dmg` and drag **Octowatcher** into **Applications**.
2. Open **Octowatcher** from Applications.

Older, unnotarized releases may be blocked on first launch. For those builds, run:

```sh
xattr -dr com.apple.quarantine /Applications/Octowatcher.app
```

The build is universal and runs on both Apple silicon and Intel Macs.

### Linux

Pick one of these:

- **AppImage** (recommended, because it can update itself):

  ```sh
  chmod +x octowatcher_*.AppImage
  ./octowatcher_*.AppImage
  ```

- **Debian / Ubuntu:** `sudo apt install ./octowatcher_*.deb`
- **Fedora / RHEL / openSUSE:** `sudo dnf install ./octowatcher-*.rpm`

If you install the `.deb` or `.rpm`, Octowatcher still checks for new versions. It can't replace a package it didn't install, so it shows a **Download** button that opens the release page.

## Using Octowatcher

The window has three tabs.

### Reviews

This tab lists the pull requests waiting on you, most recent request first. Click one to open it on GitHub. The tray menu lists the awake reviews.

If GitHub can't completely check a repository, Octowatcher shows the error and keeps unchecked saved reviews and snoozes until a complete check succeeds. Confirmed requests still enter the queue when some team memberships can't be checked, so an unreadable team doesn't hide a direct request. Other repositories continue updating. The saved list may be out of date while an error is shown. Large repositories or long review histories take more API requests to check; GitHub rate-limit errors also preserve the saved list.

If a team request and your review have exactly the same timestamp, the request stays visible: Octowatcher can't prove which happened first.

**Search and filters** help you find reviews in a busy queue. Search matches title, owner/repository, author, and PR number without regard to case. Every space-separated term must match, and terms can match different fields: `acme alice login` finds login PRs by Alice in an Acme repository. A plain number is a substring search; `#123` matches exactly PR number 123.

Repository choices include the host, so matching owner/repository names on different hosts remain separate. You can also search by host as part of the repository label. Choose a repository, **Ready** (non-draft) or **Draft**, **First review** or **Re-review**, and **Awake** or **Snoozed**. These filters combine with search and each other. All four default to **All**, including snoozed reviews. Repository choices come from the full review list; a selected repository stays selected even if its last PR disappears during a refresh.

The list shows **X of Y reviews**. The Reviews tab keeps the total count, including snoozed PRs. Search and filters only change this window's list: they do not change watched repositories, the tray's awake count/list, or notifications. Your view choices survive refreshes, tab changes, and closing/reopening the window, and reset when you restart the app.

Press **⌘F** on macOS or **Ctrl+F** on Linux to open Reviews and focus search. Use **Tab** / **Shift+Tab** to move between search and filter controls, and **Enter** or **Space** to choose a filter. **Escape** in Reviews or **Reset** clears search and all filters. In short windows, the filter area scrolls while leaving space for reviews; keyboard focus scrolls each control into view. When no PRs match, the window shows a no-results message and a reset button.

Clicking a notification about one review opens that PR. Clicking a notification about several reviews brings the Reviews tab to the front, reopening the window if you closed it; its **Open Reviews** button does the same. Your current search and filters stay active. At each launch, Octowatcher summarizes eligible reviews as GitHub confirms them. An incomplete check can announce confirmed requests, while unchecked saved reviews and their undelivered alerts wait for confirmation. Reviews confirmed later can notify then; they do not need a new request.

**Snooze…** opens a duration picker for that pull request: 5, 10, 15 or 30 minutes, or 1 or 2 hours. The default from Settings is marked; choose a duration to snooze, or **Cancel** to leave the review waiting. Each choice applies only to that snooze and doesn't change the default. Only one picker is open at a time.

The picker and snooze apply to the pull request on its displayed host. Matching repository names and pull request numbers on another host keep their own snooze durations and deadlines.

A snoozed pull request leaves the tray. With the default All filters, it stays listed in the window, dimmed, with the time it comes back. When the snooze runs out, you get its notification again. **Unsnooze** brings it back right away, without a notification. A new review request ends its snooze early. A new notification about a single pull request also has a **Snooze** button, which uses the Settings default. One that covers several pull requests doesn't. Each snooze's deadline survives a restart.

### Repositories

**Watched folders** are the folders Octowatcher scans for clones. The first time you launch it, it watches `~/Dev` if that folder exists, and your home folder otherwise. Use **Add folder…** and **Remove** to change the list, and **Rescan** after you clone something new.

The scan goes up to five levels deep. It skips hidden folders and `node_modules`, `target`, `vendor`, `build`, `dist` and `Library`.

**GitHub repositories found** lists every repository that has at least one clone in those folders, with the paths of its clones. Click a repository to switch between **watching** and **off**. Reviews from repositories that are off don't show up and don't notify you.

Enabling a repository checks it right away. If a GitHub check is already running, Octowatcher checks the updated repository list as soon as that check finishes.

Octowatcher reads the remotes from each clone's `.git/config` and understands scp-like SSH (`git@HOST:owner/repo.git`), `ssh://`, and HTTPS remotes. A clone with several GitHub remotes, like a fork and its upstream, counts for each of them. Repository labels include the host, for example `github.com/owner/repo` and `github.example.com/owner/repo`.

It also reads straightforward literal `Host` / `HostName` aliases from `~/.ssh/config`. For example, `git@github-work:owner/repo.git` maps to the host named by `HostName` in the `Host github-work` block. Aliases work for SSH remotes only, and each alias stays scoped to its destination host. Octowatcher does not evaluate `Include`, `Match`, wildcard or negated SSH host rules; use a direct host remote or a literal alias for those configurations.

### Enterprise hosts

Log in to each host through GitHub CLI, then choose **Rescan** in Repositories (or restart Octowatcher):

```sh
gh auth login --hostname github.example.com
gh auth login --hostname acme.ghe.com
```

Octowatcher recognizes the hosts configured in `gh`, plus github.com. You can watch several hosts at once; only hosts with enabled local repositories are polled for reviews. An expired login remains discoverable, so re-authenticating can recover its reviews. There is no separate host or token list in Octowatcher. It uses gh's active account on each host, including gh's normal environment-token precedence. SSH keys and aliases select a remote destination; they do not select the account used by the API. Multiple accounts on one host are not independently monitored.

API requests explicitly target the repository's web host. GitHub CLI chooses the endpoint: `api.github.com` for github.com, `HOST/api/v3` and `HOST/api/graphql` for Enterprise Server, and `api.TENANT.ghe.com` for Enterprise Cloud with data residency. Hosts must provide standard HTTPS APIs. Custom API ports, HTTP-only APIs, reverse-proxy path prefixes, and IPv6 host literals are not supported. SSH remote URLs may use a custom SSH port; HTTPS remotes may use the standard port 443.

Each host must support the GraphQL fields used for review requests, review history, and re-review detection. Octowatcher reports incompatible schemas as a host-specific error; it does not provide fallback APIs for older Enterprise Server versions. Authentication, permission, network, and API failures on one host leave its last known reviews and snoozes in place while healthy hosts keep refreshing. Discovery and review CLI calls time out after 60 seconds, including command completion and output collection, so a stalled request can report an error and allow other hosts to refresh. If discovery itself fails, Octowatcher keeps the previous scan in memory; retry **Rescan** after resolving the error. Cached reviews may be out of date until that host recovers. Repositories switched off or removed from the watched folders still leave the list.

Undelivered notifications are also scoped to their host. After a restart, cached alerts wait until their individual requests are confirmed on that host; healthy hosts can deliver while another host’s alerts remain queued. Snoozing a review or switching a repository off clears only its own host’s alerts.

Self-updates always use `github.com/mattsverse/octowatch`, independently of monitored hosts and `GH_HOST`. Keep your github.com login available to check and download app updates.

Host routing and failure handling are covered by local fixtures and command-routing tests. Enterprise Server and data-residency service behavior has not been exercised against a live Enterprise instance.

### Settings

**Appearance** offers **System**, **Light** and **Dark**. System follows the desktop's appearance and updates an open window when it changes. Light and Dark override it immediately, and the choice is saved across restarts. Missing, unrecognized or invalid appearance values default to System while preserving the rest of your saved settings and reviews. The dark palette keeps Octowatcher's existing identity, with clearer muted text and hover states; the light palette uses matching shades. Snoozed reviews use dimmer text while their buttons and status labels stay readable.

On Linux, System uses the desktop's XDG settings portal. If the portal is unavailable or reports no preference, GPUI uses light appearance; choose Light or Dark explicitly if your desktop doesn't report changes. The macOS tray glyph follows the menu bar's appearance automatically. Linux uses a black glyph with a white outline for visibility on light and dark panels, independently of the window's appearance. Native title bars, tray menus and macOS dialogs follow the desktop's theme. GPUI's built-in Linux dialogs use their own styling.

This tab sets how often Octowatcher checks GitHub: every 1, 2, 5, 10, 15, 30 or 60 minutes. The default is 2 minutes.

It also sets the default snooze length: 5, 10, 15 or 30 minutes, or 1 or 2 hours. The default is 5 minutes. Changing it affects future snoozes, including notification actions, and leaves existing snooze deadlines unchanged.

**Mute review notifications** silences review alerts until you choose **Resume review notifications**. You can also mute or resume from the tray menu. This setting survives restarts. GitHub checks, the review list, tray counts, and snooze timers continue updating. Resume sends one catch-up notification for undelivered requests still pending, excluding snoozed reviews and suppressed drafts. A snooze that expires while muted becomes visible immediately and joins that catch-up alert. Already delivered alerts can still be clicked; a send already in progress may finish. Update alerts and **Send test notification** remain available while review alerts are muted.

**Notify about drafts** defaults to **On** to preserve existing behavior. When **Off**, drafts remain visible in Reviews and the tray, but their alerts wait until the PR becomes ready for review. The same request is announced only once: making an already-announced draft ready doesn't send another alert. Switching this preference back **On** releases undelivered draft alerts, subject to mute and snooze.

Failed review deliveries remain queued, including across restarts, and retry after a successful GitHub check. Reviews found during a send are delivered as soon as that send succeeds, without waiting for another check or a click. If GitHub omits a still-pending review's request timestamp, Octowatcher keeps its last known request identity, alert, and snooze. Resolved, withdrawn, or disabled-repository requests leave the delivery queue. Acceptance by the desktop notification service counts as delivery; no click or dismissal is needed. Focus mode and desktop notification settings can still hide an accepted alert. A crash between acceptance and saving state, or a service that accepts a request after the 15-second send timeout, can cause a retry of an already delivered alert.

Octowatcher observes at most 32 active notifications, with a one-hour action lifetime, to avoid accumulating tasks and connections. Extra review alerts stay queued until an observer frees capacity or a later successful check. Linux desktops vary in support for notification buttons and body clicks; the Reviews tab and tray remain available.

### Running in the background

Closing the window doesn't quit Octowatcher. It keeps checking from the tray. To get the window back, choose **Open Octowatcher** from the tray menu, or on macOS click the Dock icon. To stop the app, choose **Quit Octowatcher**.

## Where your data lives

Octowatcher saves your settings (folders, switched-off repositories, check interval, snooze length, appearance, review mute and draft preference), your snoozes, the current review list, and undelivered review alerts to one JSON file. Existing state files keep their settings and snoozes; new notification preferences default to unmuted with draft alerts on.

The file lives at:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/octowatcher/state.json` |
| Linux | `~/.config/octowatcher/state.json` |

State from v0.4.3 and earlier loads automatically: repositories, reviews, snoozes, and queued alerts without a host belong to github.com. Settings and existing snooze deadlines are retained. New state records host-qualified identities; repository switches and snoozes affect only that host.

Delete this file to reset Octowatcher. Everything it sends to GitHub goes through `gh`.

## Troubleshooting

- **Notifications are disabled on macOS**: Octowatcher requests permission at startup. If you denied it, enable **Allow Notifications** for **Octowatcher** in **System Settings → Notifications**, then use **Send test notification** or wait for the next successful GitHub check. Octowatcher checks permission before each send.
- **"could not run `gh`; is the GitHub CLI installed?"**: install the GitHub CLI, or put it in one of the folders listed under [Requirements](#requirements).
- **A host reports an API or authentication error**: run `gh auth status --hostname HOST` in a terminal, and `gh auth login --hostname HOST` if you're logged out. Permission or schema errors may require your Enterprise administrator. Other hosts continue updating while this host's cached reviews remain visible.
- **A team membership check fails**: make sure your GitHub CLI login can read the organization’s teams. For an OAuth CLI login, `gh auth refresh -h HOST -s read:org` can grant the required scope; organizations using SAML SSO may also require authorizing the login for that organization.
- **"install gh 2.81 or newer"**: update GitHub CLI, then choose **Rescan**. Host discovery uses [JSON authentication status added in gh 2.81](https://github.com/cli/cli/releases/tag/v2.81.0).
- **A repository is missing from the list**: make sure its folder is inside a watched folder, no more than five levels down, and not inside one of the skipped folders. For Enterprise clones, authenticate to their destination host using `gh auth login --hostname HOST`. Then click **Rescan**.
- **No tray icon on Linux**: GNOME needs an AppIndicator extension, such as *AppIndicator and KStatusNotifierItem Support*, before it shows tray icons.

## Building from source

You need a recent stable Rust toolchain. The repository includes a `mise.toml` if you use [mise](https://mise.jdx.dev/).

```sh
cargo run --release
```

On Linux, install the development headers first. On Debian or Ubuntu:

```sh
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev \
  libx11-xcb-dev libxcb1-dev libfontconfig-dev libssl-dev \
  libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libdbus-1-dev
```

On macOS, notification permission and delivery require a signed `.app` bundle; they don't work when running the bare binary with `cargo run`. An ad-hoc signature is sufficient. To build the app bundle, use [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle):

```sh
cargo install cargo-bundle
cargo bundle --release
```

## Testing notifications

Run the behavioral tests with `cargo test --locked`. They cover persisted delivery retries, repeated polls, startup summaries, mute/resume, draft transitions, action routing, and snooze expiration without contacting GitHub or the desktop notification service.

On Linux, an additional opt-in test exercises actual D-Bus sends, injected permission/send failures and recovery, advertised body clicks, Snooze, summary actions, dismissal, and observer timeout against a fake notification service. Install `python3-dbus`, `python3-gi`, and `dbus-daemon`, then run:

```sh
tests/test-linux-notifications.sh
```

The script creates an isolated session bus and does not send notifications to your desktop. This checks the service contract; it doesn't prove how a particular desktop renders or routes notification actions.

Native desktop checks still need a signed macOS `.app` or a Linux desktop with a notification service:

- Check a single-review body click, **Snooze**, and a summary body click/**Open Reviews** from a different tab with the window closed.
- Deny notification permission on macOS (or stop the Linux notification service), fetch a new request, restore permission/service, and refresh. The pending request should notify once; another refresh before any click should not duplicate it.
- Mute, receive requests, resolve one, and restart. Polling/counts should stay current, mute should persist, and Resume should announce only eligible requests still waiting.
- Turn draft alerts off, request review on a draft, and mark it ready without requesting again. It should stay listed throughout and notify once when ready. With draft alerts on, marking an announced draft ready should not notify again.
- Snooze, let it expire both while active and while muted, and confirm one reminder or inclusion in the resume catch-up alert. Unsnooze should remain silent.
- Dismiss alerts, use macOS **Clear All**, and leave alerts untouched past their action lifetime. Verify observer/connection counts remain bounded and new queued alerts can be delivered.

## Releasing

Merging the release-please pull request creates the `v*` tag and a draft
GitHub release with its notes. The tag starts the Release workflow. It builds
both platforms, checks that the self-update downloads (`octowatcher-macos.tar.gz`
and the x86_64 AppImage) are present, attaches every download to the draft, and
only then publishes it. Until then the release stays a draft, so the updater
never sees a release without downloads. If a build fails, the draft and tag
remain: re-run the failed jobs, or delete both before releasing again.

The workflow can also be run manually to build and verify artifacts without
publishing a release.

The macOS job requires these repository secrets under **Settings → Secrets and
variables → Actions**:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE_P12` | Base64-encoded Developer ID Application certificate and private key, exported as a `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | The `.p12` export password |
| `APPLE_ID` | Developer Apple Account email |
| `APPLE_TEAM_ID` | The certificate's 10-character developer Team ID |
| `APPLE_APP_SPECIFIC_PASSWORD` | An app-specific password for notarization |

`packaging/macos/release.sh` signs the universal app with Hardened Runtime,
submits it to Apple, and staples the notarization ticket before creating the
self-update archive and DMG. It also signs, notarizes, and staples the DMG.
The release is published only after notarization and verification of both downloads succeed.
The temporary signing keychain and credentials are removed when the script exits.

If Apple rejects a submission, the job prints its notarization log. Each submission
waits up to 40 minutes; a timeout fails the job while Apple may continue processing.
The submission ID in the job output can be used to check its status with
`xcrun notarytool info`. Changing your Apple Account password revokes app-specific
passwords, so update `APPLE_APP_SPECIFIC_PASSWORD` afterward.

## License

[MIT](https://opensource.org/license/mit)
