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
- **Monitors multiple accounts together.** Enable your work and personal accounts independently. Each review shows its receiving account, and requests for the same PR under different accounts have separate snoozes.
- **Stores no token.** Octowatcher talks to GitHub through the [GitHub CLI](https://cli.github.com/), using saved github.com accounts and existing Enterprise host credentials without switching your active CLI account.
- **Supports Enterprise hosts.** Watch github.com, Enterprise Server and Enterprise Cloud with data residency together; matching repositories on different hosts stay separate.
- **Follows your desktop appearance.** Use System, Light or Dark in Settings. System is the default, and your choice survives restarts.
- **Starts quietly at login when you choose.** Enable **Launch at login** in Settings to keep checking from the tray after you sign in. Opening Octowatcher yourself shows its window, and repeated launches reopen the running app.
- **Shows setup and health.** See saved-account readiness, the CLI active account, watched-folder readiness, notification permission, and last successful sync, with recovery actions when a check fails.
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

This tab lists the pull requests waiting on you, most recent request first. Each card, tray entry, and review notification includes the receiving `@login`. Click one to open it on GitHub. The tray menu lists the awake reviews. If the same PR needs a review from two accounts, it appears twice and counts twice toward the tray total; snoozing one account's request does not snooze the other.

When the cache is empty and setup or a check needs attention, this tab shows **Setup & health** with guidance and recovery actions. A populated list keeps a compact warning and the header shortcut to full details, including when filters match no reviews. The header keeps the sync status and **last successful sync** visible. An empty list says **Nothing waiting on your review** only after a successful check with enabled repositories and a complete folder scan. Before then it distinguishes loading, folder setup, and unverified results.

During loading or a network failure, last known reviews for known accounts stay visible with a stale warning, including the tray links and count after a restart with saved checkout paths or during a rescan. Signed-out, disabled, or identity-mismatched github.com accounts stay hidden; repository permission failures also hide unconfirmed requests for that account and repository. An empty cache cannot confirm that no reviews are waiting. The success time does not advance on failure and survives a restart; saved results remain unverified until this launch completes a sync. Results also become stale after two check intervals plus one minute without a successful sync. The tray menu shows setup or stale status too.

Once a saved request is confirmed for its account during this launch, expired snoozes still queue their reminder while results are stale or folders are being rescanned. Mute and draft preferences apply to delivery. If notification startup is still pending, the deadline is retained until startup finishes; before that saved request is confirmed, reminders wait in the delivery queue.

Choose **Refresh** to retry immediately. Automatic checks continue at the interval in Settings, without rapid retries. Each GitHub CLI API subprocess is limited to 30 seconds, with a 60-second budget for each saved github.com account or Enterprise host’s sync. Every pagination response verifies the receiving account; github.com also verifies its stable user ID. Timeout, incomplete history or an account change reports a failed check and keeps applicable saved results stale; only complete repository snapshots may remove them. Update downloads have a separate ten-minute limit.

Octowatcher rechecks enabled saved github.com accounts and the effective account on each enabled Enterprise host every sync. Changing the active github.com account with `gh auth switch` does not change which saved accounts are monitored. On Enterprise, a detected active-account change clears only that host's previous cached reviews, snoozes, and queued notifications. Setup & health lists account and host readiness, plus the active github.com CLI account. The last-success time advances only when all enabled account/repository and host checks complete; healthy partitions still update when another fails. Legacy reviews without a receiving account are fetched again before being assigned to an account.

Opening a PR uses your browser's current GitHub session. Octowatcher does not switch your browser login; choose the matching account in GitHub before reviewing.

If GitHub can't completely check a repository, Octowatcher shows the error and keeps unchecked saved reviews and snoozes until a complete check succeeds. Confirmed requests still enter the queue when some team memberships can't be checked, so an unreadable team doesn't hide a direct request. Other repositories continue updating. The saved list may be out of date while an error is shown. Large repositories or long review histories take more API requests to check; GitHub rate-limit errors also preserve the saved list. Unconfirmed github.com requests are hidden for an authentication or repository-access failure, while individually confirmed requests can remain visible during an incomplete repository check.

If a team request and your review have exactly the same timestamp, the request stays visible: Octowatcher can't prove which happened first.

**Search and filters** help you find reviews in a busy queue. Search matches title, owner/repository, author, and PR number without regard to case. Every space-separated term must match, and terms can match different fields: `acme alice login` finds login PRs by Alice in an Acme repository. A plain number is a substring search; `#123` matches exactly PR number 123.

Repository choices include the host, so matching owner/repository names on different hosts remain separate. You can also search by host as part of the repository label. Choose a repository, **Ready** (non-draft) or **Draft**, **First review** or **Re-review**, and **Awake** or **Snoozed**. These filters combine with search and each other. All four default to **All**, including snoozed reviews. Repository choices come from the full available review list, excluding hidden account and repository caches; a selected repository stays selected even if its last PR disappears during a refresh.

The list shows **X of Y reviews**. The Reviews tab keeps the total count, including snoozed PRs. Search and filters only change this window's list: they do not change watched repositories, the tray's awake count/list, or notifications. Your view choices survive refreshes, tab changes, and closing/reopening the window, and reset when you restart the app.

Press **⌘F** on macOS or **Ctrl+F** on Linux to open Reviews and focus search. Use **Tab** / **Shift+Tab** to move between search and filter controls, and **Enter** or **Space** to choose a filter. **Escape** in Reviews or **Reset** clears search and all filters. If a Snooze picker is open, Escape cancels it first and keeps your filters. In short windows, the filter area scrolls while leaving space for reviews; keyboard focus scrolls each control into view. When no PRs match, the window shows a no-results message and a reset button.

Clicking a notification about one review opens that PR. Clicking a notification about several reviews brings the Reviews tab to the front, reopening the window if you closed it; its **Open Reviews** button does the same. Your current search and filters stay active. At each launch, Octowatcher summarizes eligible reviews as GitHub confirms them. An incomplete check can announce confirmed requests, while unchecked saved reviews and their undelivered alerts wait for confirmation. Reviews confirmed later can notify then; they do not need a new request.

**Snooze…** opens a duration picker for that pull request: 5, 10, 15 or 30 minutes, or 1 or 2 hours. The default from Settings is marked; choose a duration to snooze, or **Cancel** to leave the review waiting. Each choice applies only to that snooze and doesn't change the default. Only one picker is open at a time.

The picker and snooze apply to the pull request on its displayed host. Matching repository names and pull request numbers on another host keep their own snooze durations and deadlines.

A snoozed pull request leaves the tray. With the default All filters, it stays listed in the window, dimmed, with the time it comes back. When the snooze runs out, you get its notification again. **Unsnooze** brings it back right away, without a notification. A new review request ends its snooze early. A new notification about a single pull request also has a **Snooze** button, which uses the Settings default. One that covers several pull requests doesn't. Each snooze's deadline survives a restart.

### Repositories

**Watched folders** are the folders Octowatcher scans for clones. The first time you launch it, it watches `~/Dev` if that folder exists, and your home folder otherwise. Use **Add folder…** and **Remove** to change the list. Octowatcher rescans in the background before every GitHub check (every 2 minutes by default). **Refresh** and **Rescan** both scan immediately and then check GitHub. New, moved or removed checkouts and changes to their remotes appear on the next scan. Requests received during an active scan or GitHub check trigger one follow-up, so a manual refresh is not lost behind work already running.

The scan goes up to five levels deep. It skips hidden folders and `node_modules`, `target`, `vendor`, `build`, `dist` and `Library`. It stops at each checkout rather than scanning inside it, and does not follow directory symlinks. A folder added explicitly is scanned even if its name is normally skipped.

**GitHub repositories found** lists every repository that has at least one clone in those folders, with the paths of its clones. Click a repository to switch between **watching** and **off**. Reviews from repositories that are off don't show up and don't notify you.

Each github.com repository defaults to **All enabled accounts**. Under **Monitor with**, click account names to restrict it to selected accounts, or choose **All enabled accounts** to restore the default, including accounts added later. Selecting no accounts pauses that repository. Account selections still respect the account's global enable/disable setting. Repository-access errors name the account and repository; check permissions and organization SSO authorization if a private repository is inaccessible. Other accessible repositories continue normally. A failed access check hides that repository’s requests for the affected account and retains its cached reviews and snoozes until access recovers. Successful repository metadata probes are cached in memory for 30 minutes. **Refresh** bypasses this cache. Every poll still verifies each account and fetches repository review pages; incomplete responses retain unconfirmed state.

Enabling a repository checks it right away. Repeated refreshes during a running check coalesce into a follow-up check. Changing account or repository monitoring choices requests a check under the new scope; a running check finishes before its replacement starts.

Octowatcher reads each checkout's Git config, resolving `.git` files and the shared metadata of linked worktrees even when the main clone is outside watched folders. A submodule is included when its folder is watched explicitly; scans do not descend into its parent checkout to find it. It understands scp-like SSH (`git@HOST:owner/repo.git`), `ssh://`, and HTTPS remotes. A clone with several GitHub remotes, like a fork and its upstream, counts for each of them. Repository labels include the host, for example `github.com/owner/repo` and `github.example.com/owner/repo`.

It also reads straightforward literal `Host` / `HostName` aliases from `~/.ssh/config`. For example, `git@github-work:owner/repo.git` maps to the host named by `HostName` in the `Host github-work` block. Aliases work for SSH remotes only, and each alias stays scoped to its destination host. Octowatcher does not evaluate `Include`, `Match`, wildcard or negated SSH host rules; use a direct host remote or a literal alias for those configurations.

SSH aliases identify destinations and do not assign a github.com API account.

If a watched folder is missing or unreadable, or a checkout’s Git metadata cannot be read, the window shows a warning with details in **Repositories**. Octowatcher retains previously discovered repositories in the affected folders, including across restarts, and retries on the next scan. Git configs are streamed without a total file-size limit; individual lines exceeding 64 KiB of content (excluding LF or CRLF terminators) produce the same warning and retention behavior. A successful scan updates the list; removing a watched folder removes its checkouts from the list.

GitHub’s [SSH-over-443 configuration](https://docs.github.com/en/authentication/troubleshooting-ssh/using-ssh-over-the-https-port) is supported: `ssh://git@ssh.github.com:443/owner/repo.git`, a `Host github.com` override to `HostName ssh.github.com`, and literal aliases to that endpoint all keep the `github.com/owner/repo` identity. API calls continue to target github.com.

### Enterprise hosts

Log in to each host through GitHub CLI, then wait for the next check or choose **Rescan** in Repositories:

```sh
gh auth login --hostname github.example.com
gh auth login --hostname acme.ghe.com
```

Octowatcher recognizes the hosts configured in `gh`, plus github.com. You can watch several hosts at once; only hosts with enabled local repositories are polled for reviews. An expired login remains discoverable, so re-authenticating can recover its reviews. There is no separate host or token list in Octowatcher. On Enterprise hosts it uses gh's active account, including gh's normal environment-token precedence. SSH keys and aliases select a remote destination; they do not select the account used by the API. Enterprise accounts on one host are not independently monitored; the simultaneous-account controls apply to github.com.

API requests explicitly target the repository's web host. GitHub CLI chooses the endpoint: `api.github.com` for github.com, `HOST/api/v3` and `HOST/api/graphql` for Enterprise Server, and `api.TENANT.ghe.com` for Enterprise Cloud with data residency. Hosts must provide standard HTTPS APIs. Custom API ports, HTTP-only APIs, reverse-proxy path prefixes, and IPv6 host literals are not supported. SSH remote URLs may use a custom SSH port; HTTPS remotes may use the standard port 443.

Each host must support the GraphQL fields used for review requests, review history, and re-review detection. Octowatcher reports incompatible schemas as a host-specific error; it does not provide fallback APIs for older Enterprise Server versions. Authentication, permission, network, and API failures on one host leave its last known reviews and snoozes in place while healthy hosts keep refreshing. Discovery and review CLI subprocesses time out after 30 seconds, including command completion and output collection; a review sync has a 60-second budget per host, so a stalled request can report an error and allow other hosts to refresh. If host discovery itself fails, Octowatcher keeps its last checkout snapshot, including a saved snapshot from before a restart, while still honoring explicitly removed watched folders. GitHub checks continue for the cached watched hosts while discovery retries at each check; **Rescan** retries immediately. On a first launch without a saved checkout snapshot, checks wait for successful discovery. Cached reviews may be out of date until that host’s API or authentication recovers. Repositories switched off or removed from the watched folders still leave the list.

Undelivered notifications are also scoped to their host. After a restart, cached alerts wait until their individual requests are confirmed on that host; healthy hosts can deliver while another host’s alerts remain queued. Snoozing a review or switching a repository off clears only its own host’s alerts.

Self-updates always use `github.com/mattsverse/octowatch`, independently of monitored hosts and `GH_HOST`. Keep your github.com login available to check and download app updates.

Host routing and failure handling are covered by local fixtures and command-routing tests. Enterprise Server and data-residency service behavior has not been exercised against a live Enterprise instance.

### Settings

**Setup & health** is always available here, or through the header shortcut. It shows all state-loading, saving, tray, notification-delivery, folder-scan, and GitHub errors together. Missing or signed-out GitHub CLI setup offers an installation link and a **Copy login command** action; run that command in your terminal, then **Refresh**. **Manage folders**, **Add folder…**, and **Rescan** help recover folder setup. The shortcut and recovery buttons join the window’s **Tab / Shift+Tab** order and activate with **Enter / Space**; keyboard focus scrolls recovery actions into view.

On macOS, notification permission is read from the OS. **Notification settings** opens System Settings; after changing permission, **Refresh** to recheck it. Banners may be off even when permission is allowed. A signed `.app` bundle is required; a bare development binary reports permission as unknown. On Linux there is no portable permission query, so the panel reports **Unknown** and offers **Send test notification**. Permission and delivery errors are separate; a successful delivery does not establish OS permission or that you saw a banner. Folder scans and GitHub checks start independently of the permission prompt.

**GitHub accounts** lists saved github.com accounts discovered through `gh`. New accounts are enabled automatically. Enable/disable choices and per-repository restrictions follow the verified GitHub user ID when its login changes. Use **Disable** or **Enable** to control monitoring without signing an account out of `gh`. To add an account, run `gh auth login --hostname github.com` in a terminal, sign in as that account, then click **Refresh**. Account additions, removals, and external `gh auth switch` changes are picked up on the next check; changing the CLI's active account does not change which enabled accounts Octowatcher monitors.

If an account is signed out, its authentication expires, or its verified identity changes, its reviews are hidden from the window and tray and it sends no new review notifications. Settings shows an account-specific error. Its cache and snoozes are retained separately; other accounts continue checking. During loading or a network failure, known caches remain visible as stale, subject to account/repository choices. Saved alerts and expired-snooze reminders wait until their requests are confirmed for an available account during this launch; cached visibility does not authorize notification delivery.

**Appearance** offers **System**, **Light** and **Dark**. System follows the desktop's appearance and updates an open window when it changes. Light and Dark override it immediately, and the choice is saved across restarts. Missing, unrecognized or invalid appearance values default to System while preserving the rest of your saved settings and reviews. The dark palette keeps Octowatcher's existing identity, with clearer muted text and hover states; the light palette uses matching shades. Snoozed reviews use dimmer text while their buttons and status labels stay readable.

On Linux, System uses the desktop's XDG settings portal. If the portal is unavailable or reports no preference, GPUI uses light appearance; choose Light or Dark explicitly if your desktop doesn't report changes. The macOS tray glyph follows the menu bar's appearance automatically. Linux uses a black glyph with a white outline for visibility on light and dark panels, independently of the window's appearance. Native title bars, tray menus and macOS dialogs follow the desktop's theme. GPUI's built-in Linux dialogs use their own styling.

This tab sets how often Octowatcher checks GitHub: every 1, 2, 5, 10, 15, 30 or 60 minutes. The default is 2 minutes.

It also sets the default snooze length: 5, 10, 15 or 30 minutes, or 1 or 2 hours. The default is 5 minutes. Changing it affects future snoozes, including notification actions, and leaves existing snooze deadlines unchanged.

**Mute review notifications** silences review alerts until you choose **Resume review notifications**. You can also mute or resume from the tray menu. This setting survives restarts. GitHub checks, the review list, tray counts, and snooze timers continue updating. Resume sends one catch-up notification for undelivered requests still pending, excluding snoozed reviews and suppressed drafts. A snooze that expires while muted becomes visible immediately and joins that catch-up alert. Already delivered alerts can still be clicked; a send already in progress may finish. Update alerts and **Send test notification** remain available while review alerts are muted.

**Notify about drafts** defaults to **On** to preserve existing behavior. When **Off**, drafts remain visible in Reviews and the tray, but their alerts wait until the PR becomes ready for review. The same request is announced only once: making an already-announced draft ready doesn't send another alert. Switching this preference back **On** releases undelivered draft alerts, subject to mute and snooze.

Failed review deliveries remain queued, including across restarts, and retry after a successful GitHub check. Reviews found during a send are delivered as soon as that send succeeds, without waiting for another check or a click. If GitHub omits a still-pending review's request timestamp, Octowatcher keeps its last known request identity, alert, and snooze. Resolved, withdrawn, or disabled-repository requests leave the delivery queue. Acceptance by the desktop notification service counts as delivery; no click or dismissal is needed. Focus mode and desktop notification settings can still hide an accepted alert. A crash between acceptance and saving state, or a service that accepts a request after the 15-second send timeout, can cause a retry of an already delivered alert.

Octowatcher observes at most 32 active notifications, with a one-hour action lifetime, to avoid accumulating tasks and connections. Extra review alerts stay queued until an observer frees capacity or a later successful check. Linux desktops vary in support for notification buttons and body clicks; the Reviews tab and tray remain available.

**Launch at login** is off by default. Switch it on to start Octowatcher quietly in the tray when you sign in. It is available for signed macOS release apps installed in `/Applications` or `~/Applications` on macOS 13 or later, and for Linux `.deb`, `.rpm`, and AppImage installs. Development binaries show this setting as unavailable; older macOS versions can still launch and run Octowatcher normally.

On macOS, this uses the system login item service. If approval is needed, allow Octowatcher in **System Settings → General → Login Items**. On Linux, it creates `~/.config/autostart/com.matteogassend.octowatcher.desktop` (or under `$XDG_CONFIG_HOME` when set). Keep an AppImage in a permanent location before enabling this setting; after moving or renaming it, Settings shows **Repair** instead of **On**. Click **Repair** to register its new path. Updates installed in place keep that path.

Disabling startup in your desktop's login settings is respected; Octowatcher does not turn it back on when it starts. Deleting `state.json` resets app preferences but does not remove a system login registration; switch **Launch at login** off to remove it.

### Keyboard access

- **Tab / Shift+Tab** move forward or backward through the window's controls. A contrasting border shows focus in either theme, and content scrolls into view when you navigate to it. Settings starts with setup recovery and account controls, followed by the System, Light and Dark appearance choices and the available Launch at login control; changing appearance keeps focus on your choice.
- **Enter / Space** activate the focused control: open a review, snooze or unsnooze it, switch a tab, toggle a repository, or pick a setting. Each review's Snooze button is a separate focus stop; activating it keeps you in Octowatcher.
- To snooze with the keyboard, activate **Snooze…**, use **Tab / Shift+Tab** to choose a duration or **Cancel**, then press **Enter / Space**. **Escape** cancels the open picker without resetting your filters. With no picker open, it resets search and filters. Choosing or canceling returns focus to that review’s Snooze button, or a nearby remaining Snooze button if a filter hides that review. Snoozing a different review from a notification preserves your open picker and focus.
- **Left / Right** switch tabs when a tab has focus, wrapping at either end.
- **Up / Down** move between the reviews matching your search and filters. **Home / End** move to the first or last matching review. If a Snooze button has focus, these keys move between the Snooze buttons instead.
- **Command+R / Command+Q** on macOS, or **Control+R / Control+Q** on Linux, refresh reviews or quit.

Focus follows the same review or repository when a background check reorders the list. Matching repository names and PR numbers on different GitHub hosts or receiving accounts have separate focus stops and actions. Account toggles and per-repository account selections also support keyboard navigation. If a focused control disappears, focus moves to a nearby remaining control of the same type when possible, so review-card focus stays on a card and Snooze focus stays on Snooze. Empty and loading lists still allow navigation through the header and tabs.

The current GPUI dependency (0.2.2) does not expose an accessibility tree or APIs for control roles, accessible names, selected states, or screen-reader announcements. Keyboard access is supported, but the custom window controls cannot currently be exposed to VoiceOver or Linux screen readers. Native menus and dialogs depend on platform support. Full assistive-technology support requires a framework change; this feature does not upgrade GPUI or change saved settings.

### Running in the background

Closing the window doesn't quit Octowatcher. It keeps checking from the tray. To get the window back, choose **Open Octowatcher** from the tray menu, or on macOS click the Dock icon. To stop the app, choose **Quit Octowatcher**.

On Linux, **Close** minimizes the window and leaves its taskbar entry available. GPUI's Linux backend otherwise exits when the last window is destroyed. Opening Octowatcher again creates a visible window with the same app state and window bounds. The desktop controls whether it receives keyboard focus. On macOS, Close removes the window as before.

There is one running instance per user, even if you launch another copy or start it from a terminal. An ordinary launch brings back that instance's window; a login launch leaves it quietly in the background. This prevents duplicate checks, notifications, and simultaneous state writes. The instance lock uses a protected home directory, or the platform’s private per-user directory when your home is group-writable (Linux: `$XDG_RUNTIME_DIR`, falling back to `/run/user/<uid>`; macOS: the Darwin user cache). **Quit Octowatcher** stops it until you open it again or next sign in, without disabling launch at login. Restarting after an update opens the new version's window.

For a quiet manual launch, pass `--background` to the binary or AppImage (on macOS, use `open /Applications/Octowatcher.app --args --background`). If the app cannot create its tray icon, it opens the window so you can still access it. On Linux, the desktop must still provide a visible AppIndicator tray; a successfully created icon cannot tell Octowatcher whether the desktop displays it.

## Where your data lives

Octowatcher saves your settings (folders, switched-off repositories and accounts, per-repository account selections, check interval, snooze length, last observed launch-at-login status, appearance, review mute and draft preference), the last discovered checkout paths, account-specific snoozes, cached review lists, and undelivered review alerts to one JSON file:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/octowatcher/state.json` |
| Linux | `~/.config/octowatcher/state.json` |

Delete this file to reset Octowatcher’s saved preferences and review state. Login registration is managed separately by the operating system. Everything it sends to GitHub goes through `gh`.

If this file is invalid, Octowatcher reports the recovery in **Setup & health** and preserves its original contents in a uniquely named `state-recovery-*.json` beside it before allowing defaults to be saved. Review the default folders and settings, or restore the backup to `state.json` and restart. If the file cannot be read or a backup cannot be made, saving is paused to protect it; fix the file or directory permissions and restart. Intentionally removing every watched folder is preserved across restarts.

Review and snooze identity includes the GitHub host, repository, and PR number, plus the stable user ID on github.com. Enterprise keeps its existing per-host active-account state. Monitoring preferences also use verified user IDs; older login-based preferences migrate when the corresponding identity is known or verified. Credentials are retrieved by account from `gh`, used temporarily in memory and the child process environment, and never saved to the state file or passed in command arguments. For github.com monitoring and updates, Octowatcher uses saved CLI credentials and ignores inherited `GH_TOKEN`/`GITHUB_TOKEN` overrides. Enterprise host calls retain gh’s normal environment-token precedence. Update checks and downloads can use a healthy saved github.com account even if it is disabled for monitoring.

When upgrading github.com state without account identity, folders, disabled repositories, check interval, and snooze duration are preserved. Old cached reviews, snoozes and undelivered alerts are discarded because their receiving account is unknown; current requests are fetched again and may notify again. Host-qualified Enterprise caches, snoozes and queued alerts are retained, and old repository switches remain compatible.

## Troubleshooting

- **Notifications are disabled on macOS**: Octowatcher requests permission at startup. If you denied it, enable **Allow Notifications** for **Octowatcher** in **System Settings → Notifications**, then use **Send test notification** or wait for the next successful GitHub check. Octowatcher checks permission before each send.
- **"could not run `gh`; is the GitHub CLI installed?"**: install the GitHub CLI, or put it in one of the folders listed under [Requirements](#requirements).
- **An account is unavailable**: run `gh auth status --hostname github.com` in a terminal, sign back into the affected account with `gh auth login --hostname github.com`, then click **Refresh**. Upgrade `gh` if account discovery reports unsupported JSON flags. An environment token alone is not a saved account.
- **A host reports an API or authentication error**: run `gh auth status --hostname HOST` in a terminal, and `gh auth login --hostname HOST` if you're logged out. Permission or schema errors may require your Enterprise administrator. Other hosts continue updating while this host's cached reviews remain visible.
- **A team membership check fails**: make sure your GitHub CLI login can read the organization’s teams. For an OAuth CLI login, `gh auth refresh -h HOST -s read:org` can grant the required scope; organizations using SAML SSO may also require authorizing the login for that organization.
- **"install gh 2.81 or newer"**: update GitHub CLI, then choose **Rescan**. Host discovery uses [JSON authentication status added in gh 2.81](https://github.com/cli/cli/releases/tag/v2.81.0).
- **A repository is missing from the list**: make sure its folder is inside a watched folder, no more than five levels down, and not inside one of the skipped folders. For Enterprise clones, authenticate to their destination host using `gh auth login --hostname HOST`. Wait for the next check, or click **Rescan** to scan immediately. Check **Repositories** for folder or Git metadata warnings. For a nested submodule, add its folder explicitly.
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

On Linux, an isolated X11 window lifecycle check verifies Close, minimize, and reopening the running process:

```sh
sudo apt install xvfb openbox xdotool wmctrl x11-utils dbus-x11
cargo build --locked
tests/linux-window-reopen.sh target/debug/octowatcher
```

It starts its own virtual display and uses temporary settings. Wayland compositor behavior still needs a desktop check.

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
