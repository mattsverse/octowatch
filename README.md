# Octowatcher

Octowatcher sits in your menu bar and tells you when someone asks you to review a pull request in a repository you have cloned on your machine.

It finds the GitHub repositories in your project folders, checks GitHub every few minutes, and sends a desktop notification for each new review request. The tray icon shows how many reviews are waiting, and clicking one opens the pull request in your browser.

## Features

- **Uses the repositories you already have.** Octowatcher scans folders such as `~/Dev` for git clones with a GitHub remote, so you only hear about the repositories you work on. You can switch any of them off.
- **Notifies you about new requests only.** You get one notification when a review is requested, and another if you are asked to re-review after you've already left a review. A pull request leaves the list once you review it, the request is withdrawn, or the PR is closed.
- **Includes team requests.** It counts requests made to a team you belong to. The request clears once you review, even if the rest of the team hasn't.
- **Snoozes a review for later.** Snooze a pull request from its notification or from the window. It leaves the tray until the snooze runs out, then notifies you again. A new review request on it ends the snooze early.
- **Lives in the tray.** The icon shows how many reviews are waiting, and its menu lists them, marked `(draft)` or `(re-review)` where that applies.
- **Monitors multiple accounts together.** Enable your work and personal accounts independently. Each review shows its receiving account, and requests for the same PR under different accounts have separate snoozes.
- **Stores no token.** Octowatcher talks to GitHub through the [GitHub CLI](https://cli.github.com/), using its saved github.com accounts without switching your active CLI account.
- **Updates itself.** It looks for a new release every six hours, or right away when you choose **Check for Updates…** from the tray menu. When one is out, a notification offers **Update**. On macOS, and on Linux when you run the AppImage, that installs it and then asks whether to restart now or later.

## Requirements

- macOS, or Linux with a desktop that shows tray icons (AppIndicator).
- A recent [GitHub CLI](https://cli.github.com/) (`gh`) supporting `gh auth status --json hosts` and `gh auth token --user`, with at least one saved github.com login:

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

This tab lists the pull requests waiting on you, most recent request first. Each card, tray entry, and review notification includes the receiving `@login`. Click one to open it on GitHub. The tray menu shows the same list. If the same PR needs a review from two accounts, it appears twice and counts twice toward the tray total; snoozing one account's request does not snooze the other.

Opening a PR uses your browser's current GitHub session. Octowatcher does not switch your browser login; choose the matching account in GitHub before reviewing.

**Snooze** hides a pull request from the tray for the snooze length set in Settings. In the window it stays listed, dimmed, with the time it comes back. When the snooze runs out, you get its notification again. **Unsnooze** brings it back right away, without a notification. A new notification about a single pull request also has a **Snooze** button. One that covers several pull requests doesn't. Snoozes survive a restart.

### Repositories

**Watched folders** are the folders Octowatcher scans for clones. The first time you launch it, it watches `~/Dev` if that folder exists, and your home folder otherwise. Use **Add folder…** and **Remove** to change the list, and **Rescan** after you clone something new.

The scan goes up to five levels deep. It skips hidden folders and `node_modules`, `target`, `vendor`, `build`, `dist` and `Library`.

**GitHub repositories found** lists every repository that has at least one clone in those folders, with the paths of its clones. Click a repository to switch between **watching** and **off**. Reviews from repositories that are off don't show up and don't notify you.

Each repository defaults to **All enabled accounts**. Under **Monitor with**, click account names to restrict it to selected accounts, or choose **All enabled accounts** to restore the default, including accounts added later. Selecting no accounts pauses that repository. Account selections still respect the account's global enable/disable setting. Repository-access errors name the account and repository; check permissions and organization SSO authorization if a private repository is inaccessible. Other accessible repositories continue normally.

Octowatcher reads the remotes from each clone's `.git/config` and understands SSH, `ssh://` and HTTPS remotes. If you use host aliases in `~/.ssh/config`, such as `git@github-work:owner/repo.git`, it picks up any alias whose `HostName` is `github.com`. SSH aliases identify repositories; they do not assign a GitHub API account. A clone with several GitHub remotes, like a fork and its upstream, counts for each of them. GitHub Enterprise hosts are not supported by this account feature.

### Settings

**GitHub accounts** lists saved github.com accounts discovered through `gh`. New accounts are enabled automatically. Use **Disable** or **Enable** to control monitoring without signing an account out of `gh`. To add an account, run `gh auth login --hostname github.com` in a terminal, sign in as that account, then click **Refresh**. Account additions, removals, and external `gh auth switch` changes are picked up on the next check; changing the CLI's active account does not change which enabled accounts Octowatcher monitors.

If an account is signed out, its authentication expires, or its check fails, its reviews are hidden from the window and tray and it sends no new review notifications. Settings shows an account-specific error. Its cache and snoozes are retained separately; other accounts continue checking. After a successful check, its current reviews return and snoozes that expired while unavailable can notify again. Cached reviews also stay hidden after app restart until the first successful check.

This tab sets how often Octowatcher checks GitHub: every 1, 2, 5, 10, 15, 30 or 60 minutes. The default is 2 minutes.

It also sets how long a snooze lasts: 5, 10, 15 or 30 minutes, or 1 or 2 hours. The default is 5 minutes.

### Running in the background

Closing the window doesn't quit Octowatcher. It keeps checking from the tray. To get the window back, choose **Open Octowatcher** from the tray menu, or on macOS click the Dock icon. To stop the app, choose **Quit Octowatcher**.

## Where your data lives

Octowatcher saves your settings (folders, switched-off repositories and accounts, per-repository account selections, check interval and snooze length), your account-specific snoozes, and cached review lists to one JSON file:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/octowatcher/state.json` |
| Linux | `~/.config/octowatcher/state.json` |

Delete this file to reset Octowatcher. Everything it sends to GitHub goes through `gh`.

Review and snooze identity uses GitHub's stable user ID, repository, and PR number. Credentials are retrieved by account from `gh`, used temporarily in memory and the child process environment, and never saved to the state file or passed in command arguments. Octowatcher uses saved CLI credentials and ignores inherited `GH_TOKEN`/`GITHUB_TOKEN` overrides for both monitoring and updates. Update checks and downloads can use a healthy saved github.com account even if it is disabled for monitoring.

When upgrading from a state file without account identity, folders, disabled repositories, check interval, and snooze duration are preserved. Old cached reviews and snoozes are discarded because their receiving account is unknown; current requests are fetched again and may notify again.

## Troubleshooting

- **Notifications are disabled on macOS**: Octowatcher requests permission at startup. If you denied it, enable **Allow Notifications** for **Octowatcher** in **System Settings → Notifications**, then restart the app.
- **"could not run `gh`; is the GitHub CLI installed?"**: install the GitHub CLI, or put it in one of the folders listed under [Requirements](#requirements).
- **An account is unavailable**: run `gh auth status --hostname github.com` in a terminal, sign back into the affected account with `gh auth login --hostname github.com`, then click **Refresh**. Upgrade `gh` if account discovery reports unsupported JSON flags. An environment token alone is not a saved account.
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
