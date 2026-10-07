# Octowatcher

Octowatcher sits in your menu bar and tells you when someone asks you to review a pull request in a repository you have cloned on your machine.

It finds the GitHub repositories in your project folders, checks GitHub every few minutes, and sends a desktop notification for each new review request. The tray icon shows how many reviews are waiting, and clicking one opens the pull request in your browser.

## Features

- **Uses the repositories you already have.** Octowatcher scans folders such as `~/Dev` for git clones with a GitHub remote, so you only hear about the repositories you work on. You can switch any of them off.
- **Notifies you about new requests only.** You get one notification when a review is requested, and another if you are asked to re-review after you've already left a review. A pull request leaves the list once you review it, the request is withdrawn, or the PR is closed.
- **Includes team requests.** It counts requests made to a team you belong to. The request clears once you review, even if the rest of the team hasn't.
- **Snoozes a review for later.** Snooze a pull request from its notification or from the window. It leaves the tray until the snooze runs out, then notifies you again. A new review request on it ends the snooze early.
- **Lives in the tray.** The icon shows how many reviews are waiting, and its menu lists them, marked `(draft)` or `(re-review)` where that applies.
- **Follows your desktop appearance.** Use System, Light or Dark in Settings. System is the default, and your choice survives restarts.
- **Stores no token.** Octowatcher talks to GitHub through the [GitHub CLI](https://cli.github.com/), so it uses the login you already have.
- **Shows setup and health.** See the active GitHub account, watched-folder readiness, notification permission, and last successful sync, with recovery actions when a check fails.
- **Updates itself.** It looks for a new release every six hours, or right away when you choose **Check for Updates…** from the tray menu. When one is out, a notification offers **Update**. On macOS, and on Linux when you run the AppImage, that installs it and then asks whether to restart now or later.

## Requirements

- macOS, or Linux with a desktop that shows tray icons (AppIndicator).
- The [GitHub CLI](https://cli.github.com/) (`gh`), logged in:

  ```sh
  gh auth login
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

This tab lists the pull requests waiting on you, most recent request first. Click one to open it on GitHub. The tray menu shows the same list.

While setup or a check needs attention, this tab shows **Setup & health** with guidance and recovery actions. The header keeps the sync status and **last successful sync** visible. An empty list says **Nothing waiting on your review** only after a successful check with enabled repositories and a complete folder scan. Before then it distinguishes loading, folder setup, and unverified results.

If a check fails, the last known reviews stay visible with a stale warning, including the tray links and count after a restart or during a rescan. An empty cache cannot confirm that no reviews are waiting. The success time does not advance on failure and survives a restart; saved results remain unverified until this launch completes a sync. Results also become stale after two check intervals plus one minute without a successful sync. The tray menu shows setup or stale status too.

Expired snoozes still send their reminder while results are stale or folders are being rescanned; if notification startup is still pending, the deadline is retained until startup finishes.

Choose **Refresh** to retry immediately. Automatic checks continue at the interval in Settings, without rapid retries. Each GitHub CLI API subprocess is limited to 30 seconds, so a hung account lookup or review query cannot permanently stop checks (a full check can use two such subprocesses). Update downloads have a separate ten-minute limit.

Octowatcher rechecks the effective `github.com` account each sync. After you change it with `gh auth switch --hostname github.com`, choose **Refresh**. A detected account change clears the previous account's cached reviews and snoozes; folder choices and settings stay intact. Cached reviews from an older state file without an account are reloaded from GitHub before being assigned to an account. This uses one account at a time, and GitHub CLI environment-token overrides still take precedence.

**Snooze…** opens a duration picker for that pull request: 5, 10, 15 or 30 minutes, or 1 or 2 hours. The default from Settings is marked; choose a duration to snooze, or **Cancel** to leave the review waiting. Each choice applies only to that snooze and doesn't change the default. Only one picker is open at a time.

A snoozed pull request leaves the tray. In the window it stays listed, dimmed, with the time it comes back. When the snooze runs out, you get its notification again. **Unsnooze** brings it back right away, without a notification. A new review request ends its snooze early. A new notification about a single pull request also has a **Snooze** button, which uses the Settings default. One that covers several pull requests doesn't. Each snooze's deadline survives a restart.

### Repositories

**Watched folders** are the folders Octowatcher scans for clones. The first time you launch it, it watches `~/Dev` if that folder exists, and your home folder otherwise. Use **Add folder…** and **Remove** to change the list, and **Rescan** after you clone something new.

The scan goes up to five levels deep. It skips hidden folders and `node_modules`, `target`, `vendor`, `build`, `dist` and `Library`.

**GitHub repositories found** lists every repository that has at least one clone in those folders, with the paths of its clones. Click a repository to switch between **watching** and **off**. Reviews from repositories that are off don't show up and don't notify you.

Octowatcher reads the remotes from each clone's `.git/config` and understands SSH, `ssh://` and HTTPS remotes. If you use host aliases in `~/.ssh/config`, such as `git@github-work:owner/repo.git`, it picks up any alias whose `HostName` is `github.com`. A clone with several GitHub remotes, like a fork and its upstream, counts for each of them.

### Settings

**Setup & health** is always available here, or through the header shortcut. It shows all state-loading, saving, tray, notification-delivery, folder-scan, and GitHub errors together. Missing or signed-out GitHub CLI setup offers an installation link and a **Copy login command** action; run that command in your terminal, then **Refresh**. **Manage folders**, **Add folder…**, and **Rescan** help recover folder setup.

On macOS, notification permission is read from the OS. **Notification settings** opens System Settings; after changing permission, **Refresh** to recheck it. Banners may be off even when permission is allowed. A signed `.app` bundle is required; a bare development binary reports permission as unknown. On Linux there is no portable permission query, so the panel reports **Unknown** and offers **Send test notification**. Permission and delivery errors are separate; a successful delivery does not establish OS permission or that you saw a banner. Folder scans and GitHub checks start independently of the permission prompt.

**Appearance** offers **System**, **Light** and **Dark**. System follows the desktop's appearance and updates an open window when it changes. Light and Dark override it immediately, and the choice is saved across restarts. Missing, unrecognized or invalid appearance values default to System while preserving the rest of your saved settings and reviews. The dark palette keeps Octowatcher's existing identity, with clearer muted text and hover states; the light palette uses matching shades. Snoozed reviews use dimmer text while their buttons and status labels stay readable.

On Linux, System uses the desktop's XDG settings portal. If the portal is unavailable or reports no preference, GPUI uses light appearance; choose Light or Dark explicitly if your desktop doesn't report changes. The macOS tray glyph follows the menu bar's appearance automatically. Linux uses a black glyph with a white outline for visibility on light and dark panels, independently of the window's appearance. Native title bars, tray menus and macOS dialogs follow the desktop's theme. GPUI's built-in Linux dialogs use their own styling.

This tab sets how often Octowatcher checks GitHub: every 1, 2, 5, 10, 15, 30 or 60 minutes. The default is 2 minutes.

It also sets the default snooze length: 5, 10, 15 or 30 minutes, or 1 or 2 hours. The default is 5 minutes. Changing it affects future snoozes, including notification actions, and leaves existing snooze deadlines unchanged.

### Running in the background

Closing the window doesn't quit Octowatcher. It keeps checking from the tray. To get the window back, choose **Open Octowatcher** from the tray menu, or on macOS click the Dock icon. To stop the app, choose **Quit Octowatcher**.

## Where your data lives

Octowatcher saves your settings (folders, switched-off repositories, check interval, snooze length and appearance), your snoozes, and the current review list for the repositories you watch to one JSON file:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/octowatcher/state.json` |
| Linux | `~/.config/octowatcher/state.json` |

Delete this file to reset Octowatcher. Everything it sends to GitHub goes through `gh`.

If this file is invalid, Octowatcher reports the recovery in **Setup & health** and preserves its original contents in a uniquely named `state-recovery-*.json` beside it before allowing defaults to be saved. Review the default folders and settings, or restore the backup to `state.json` and restart. If the file cannot be read or a backup cannot be made, saving is paused to protect it; fix the file or directory permissions and restart. Intentionally removing every watched folder is preserved across restarts.

## Troubleshooting

- **Notifications are disabled on macOS**: Octowatcher requests permission at startup. If you denied it, enable **Allow Notifications** for **Octowatcher** in **System Settings → Notifications**, then restart the app.
- **"could not run `gh`; is the GitHub CLI installed?"**: install the GitHub CLI, or put it in one of the folders listed under [Requirements](#requirements).
- **"gh api failed: …"**: run `gh auth status` in a terminal, and `gh auth login` if you're logged out.
- **A repository is missing from the list**: make sure its folder is inside a watched folder, no more than five levels down, and not inside one of the skipped folders. Then click **Rescan**.
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
