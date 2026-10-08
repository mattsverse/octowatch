# Homebrew distribution

Octowatcher uses the existing public [Mattsverse tap](https://github.com/mattsverse/homebrew-tap).
Its cask is `Casks/octowatcher.rb`; users install it with:

```sh
brew install --cask mattsverse/tap/octowatcher
gh auth login --hostname github.com
```

The cask installs GitHub CLI, uses the universal signed and notarized
`Octowatcher.dmg`, and keeps the version in its download URL. No separate
Intel/Apple silicon artifacts or Homebrew-hosted downloads are needed.
The initial cask uses the already published `v0.6.0` release, so launching on
Homebrew does not require a new app release.

## One-time credentials

Create a [fine-grained personal access token](https://github.com/settings/personal-access-tokens/new):

1. Choose **mattsverse** as the resource owner.
2. Choose **Only select repositories**, and select **homebrew-tap**.
3. Under repository permissions, grant **Contents: Read and write**.
   Metadata read access is included automatically. No Workflows or Pull requests
   permission is needed: the job pushes a single cask commit to the tap's `main`.
4. Choose an expiration date and a reminder to rotate the token. Complete any
   organization approval required for the token before testing it.
5. In **octowatch → Settings → Secrets and variables → Actions → New repository
   secret**, save it as **HOMEBREW_TAP_TOKEN**.

Alternatively, create an organization Actions secret with that name and grant
`octowatch` access. Accordo already uses a secret named `HOMEBREW_TAP_TOKEN`,
but a secret scoped to Accordo is not automatically available to Octowatcher.
If an organization secret already exists, add Octowatcher to its allowed
repositories and check that the token can still write to this tap.

GitHub's default `GITHUB_TOKEN` is scoped to the app repository, so it cannot
push to the separate tap. The workflow uses it only to read the app release;
the tap token is used for the tap checkout and push.
See [GitHub token authentication](https://docs.github.com/en/actions/tutorials/authenticate-with-github_token)
and [fine-grained token setup](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens).

Keep the existing `RELEASE_PLEASE_TOKEN` and Apple signing/notarization secrets.
They are already configured and the `v0.6.0` release succeeded; Homebrew needs
no additional Apple credentials.

The token owner's repository role and any tap branch rules must allow pushing
the cask commit to `main`. If the tap requires every update to go through a PR,
adapt the publisher to open PRs rather than weakening that rule.

## Initial rollout

1. Review and merge the tap branch that adds `Casks/octowatcher.rb` and its
   installation instructions. Use a rebase merge. This makes `v0.6.0` installable
   immediately; it does not depend on the automation workflow being merged.
2. Configure `HOMEBREW_TAP_TOKEN` in Octowatcher as described above.
3. Review and rebase-merge the app branch containing `homebrew.yml`, the
   post-release job, and `update_cask.py`.
4. Run the publisher on `main` against the current published release:

   ```sh
   gh workflow run homebrew.yml --repo mattsverse/octowatch --ref main -f tag=v0.6.0
   gh run list --repo mattsverse/octowatch --workflow homebrew.yml --limit 5
   ```

   Inspect the run in Actions. An identical version/checksum produces
   **No cask changes needed** and no commit. The workflow first performs a
   dry-run push to check tap authentication without changing the remote.
5. On a Mac without an existing manually installed Octowatcher app, run:

   ```sh
   brew update
   brew install --cask mattsverse/tap/octowatcher
   brew info --cask mattsverse/tap/octowatcher
   gh auth login --hostname github.com
   open -a Octowatcher
   ```

   If `/Applications/Octowatcher.app` already exists from a DMG install, quit
   the app and move that copy aside before the Homebrew install. Check that
   the app opens normally and Settings shows GitHub CLI readiness. There is
   no quarantine-removal step for this notarized release.

## Normal release flow

Merge conventional commits to `main`, then review and rebase-merge the
Release Please PR. The existing automation creates a tag and draft release,
builds both platforms, signs and notarizes the Mac artifacts, uploads the
downloads, and publishes the release.

Only after the `release` job succeeds does the new `homebrew` job call the
publisher. It checks that the tag identifies a published stable release,
downloads `Octowatcher.dmg`, computes SHA-256 and checks GitHub's asset digest
when present. It changes only the tap cask's `version` and `sha256`, preserving
other cask metadata, then commits `chore(octowatcher): release vX.Y.Z`, rebases
onto the tap's current `main`, and pushes.

Prerelease tags are excluded. Re-running the same release is a no-op; an older
release cannot roll back the tap. A changed DMG checksum for an existing version
fails rather than silently pointing Homebrew at a replaced release artifact.
Publish a new app version for changed binaries.

The publisher is called directly by the Release workflow because events created
with `GITHUB_TOKEN` generally do not start another workflow. It also has its own
manual trigger for bootstrap and recovery, without rebuilding or republishing
the app. Manually dispatching the existing **Release** workflow on `main` still
only builds artifacts and does not publish the cask.

## Recovery and token rotation

If Homebrew publication fails, the app release stays published and the tap
stays at its previous version. Fix the tap token, initial cask, or push conflict,
then run **Publish Homebrew cask** on `main` with the failed release's tag:

```sh
gh workflow run homebrew.yml --repo mattsverse/octowatch --ref main -f tag=vX.Y.Z
```

If Accordo updates the tap during the job, the rebase preserves its formula
commit. A conflicting cask edit or a push race fails the job; retry the publisher
to fetch the current tap. Do not rerun the Mac/Linux builds to repair the tap.

For **403** or a rejected push, check token expiration, organization approval,
the selected repository, Contents write permission, the token owner's access,
and tap branch rules. Rotate the credential by replacing `HOMEBREW_TAP_TOKEN`
with a new token using the same permissions, test a manual run, then revoke the
old token if no other publisher still uses it.

Octowatcher's in-app updater remains available. Since the cask declares
`auto_updates true`, Homebrew users who prefer explicit upgrades should quit
Octowatcher and run:

```sh
brew update
brew upgrade --cask --greedy mattsverse/tap/octowatcher
```

See the [Homebrew Cask Cookbook](https://docs.brew.sh/Cask-Cookbook) for cask
fields and [tap conventions](https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap)
for the `mattsverse/tap` name.

## Local validation

```sh
python3 -m unittest discover -s packaging/homebrew -p 'test_*.py'
python3 packaging/homebrew/update_cask.py v0.6.0 ../homebrew-tap/Casks/octowatcher.rb
brew style ../homebrew-tap/Casks/octowatcher.rb
```

The update script changes the local cask only; it never commits or pushes.
The workflow owns those operations.

After the cask is merged, audit it through the installed tap (current Homebrew
requires a cask name rather than a file path for `audit`):

```sh
brew tap mattsverse/tap
brew update
brew audit --cask --online mattsverse/tap/octowatcher
```
