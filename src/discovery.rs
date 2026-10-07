use serde::{Deserialize, Serialize};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use crate::repository::{PUBLIC_HOST, RepositoryId, normalize_host};

const MAX_DEPTH: usize = 5;
const MAX_CONFIG_LINE_BYTES: u64 = 64 * 1024;
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "build",
    "dist",
    "Library",
];

/// A GitHub repository with at least one clone on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRepo {
    /// Legacy discovery caches stored public GitHub slugs under `slug`.
    #[serde(alias = "slug")]
    pub id: RepositoryId,
    pub paths: Vec<PathBuf>,
}

/// A failed scan of a folder or checkout. `path` is the affected checkout
/// subtree, even when the unreadable metadata lives outside the watched roots.
#[derive(Debug, Clone)]
pub struct ScanIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub repos: Vec<LocalRepo>,
    pub issues: Vec<ScanIssue>,
    inspected: BTreeSet<PathBuf>,
}

impl ScanResult {
    /// Keep known checkouts only where this scan could not establish what exists.
    /// Deleted clones in readable folders and removed watched roots disappear.
    pub fn retain_unavailable(&mut self, previous: &[LocalRepo]) {
        let mut found = BTreeMap::new();
        for repo in &self.repos {
            for path in &repo.paths {
                record(&mut found, &repo.id, path);
            }
        }
        let unavailable: Vec<_> = self
            .issues
            .iter()
            .map(|issue| (&issue.path, normalized_path(&issue.path)))
            .collect();
        for repo in previous {
            for path in &repo.paths {
                let identity = normalized_path(path);
                if !self.inspected.contains(&identity)
                    && unavailable.iter().any(|(logical, physical)| {
                        path.starts_with(logical) || identity.starts_with(physical)
                    })
                {
                    record(&mut found, &repo.id, path);
                }
            }
        }
        self.repos = grouped_repos(found);
    }
}

/// Paths are keyed by their physical identity, while displayed/cache paths
/// preserve the watched root (including an explicitly watched symlink).
struct FoundRepo {
    id: RepositoryId,
    paths: BTreeMap<PathBuf, PathBuf>,
}

fn record(found: &mut BTreeMap<RepositoryId, FoundRepo>, id: &RepositoryId, path: &Path) {
    let repo = found.entry(id.clone()).or_insert_with(|| FoundRepo {
        id: id.clone(),
        paths: BTreeMap::new(),
    });
    repo.paths
        .entry(normalized_path(path))
        .or_insert_with(|| path.to_path_buf());
}

fn grouped_repos(found: BTreeMap<RepositoryId, FoundRepo>) -> Vec<LocalRepo> {
    found
        .into_values()
        .map(|repo| {
            let mut paths: Vec<_> = repo.paths.into_values().collect();
            paths.sort();
            LocalRepo { id: repo.id, paths }
        })
        .collect()
}

/// Walks `roots` for checkouts and groups them by GitHub repository. Resolves
/// gitfiles and commondir without traversing the external metadata directory.
/// No subprocesses or filesystem watches are needed.
pub fn discover(roots: &[PathBuf], hosts: &[String]) -> ScanResult {
    let config = dirs::home_dir()
        .and_then(|home| fs::read_to_string(home.join(".ssh/config")).ok())
        .unwrap_or_default();
    discover_with_aliases(roots, hosts, &ssh_aliases(&config))
}

fn discover_with_aliases(
    roots: &[PathBuf],
    hosts: &[String],
    aliases: &BTreeMap<String, String>,
) -> ScanResult {
    let mut scanner = Scanner {
        hosts,
        aliases,
        found: BTreeMap::new(),
        issues: Vec::new(),
        visited: BTreeMap::new(),
        configs: BTreeMap::new(),
        inspected: BTreeSet::new(),
    };
    for root in roots {
        match fs::canonicalize(root) {
            Ok(identity) => scanner.walk(&absolute_path(root), &identity, 0),
            Err(err) => scanner.issue(
                &absolute_path(root),
                format!("could not scan watched folder: {err}"),
            ),
        }
    }
    ScanResult {
        repos: grouped_repos(scanner.found),
        issues: scanner.issues,
        inspected: scanner.inspected,
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

/// Canonicalize the existing prefix too when the final folder is missing.
/// This keeps cache matching stable for paths such as macOS's /var -> /private/var.
fn normalized_path(path: &Path) -> PathBuf {
    let absolute = absolute_path(path);
    let mut ancestor = absolute.as_path();
    loop {
        if let Ok(prefix) = fs::canonicalize(ancestor) {
            return prefix.join(absolute.strip_prefix(ancestor).unwrap());
        }
        let Some(parent) = ancestor.parent() else {
            return absolute;
        };
        ancestor = parent;
    }
}

/// Drop paths whose watched root was removed before retaining unavailable ones.
pub fn within_roots(repos: &[LocalRepo], roots: &[PathBuf]) -> Vec<LocalRepo> {
    let roots: Vec<_> = roots
        .iter()
        .flat_map(|root| [absolute_path(root), normalized_path(root)])
        .collect();
    repos
        .iter()
        .filter_map(|repo| {
            let paths: Vec<_> = repo
                .paths
                .iter()
                .filter(|path| {
                    let identity = normalized_path(path);
                    roots
                        .iter()
                        .any(|root| path.starts_with(root) || identity.starts_with(root))
                })
                .cloned()
                .collect();
            (!paths.is_empty()).then(|| LocalRepo {
                id: repo.id.clone(),
                paths,
            })
        })
        .collect()
}

struct Scanner<'a> {
    hosts: &'a [String],
    aliases: &'a BTreeMap<String, String>,
    found: BTreeMap<RepositoryId, FoundRepo>,
    issues: Vec<ScanIssue>,
    /// Revisit an overlapping root only when it offers more depth allowance.
    visited: BTreeMap<PathBuf, usize>,
    /// Main clones and linked worktrees share config; read it once per scan.
    configs: BTreeMap<PathBuf, Result<Vec<RepositoryId>, String>>,
    inspected: BTreeSet<PathBuf>,
}

impl Scanner<'_> {
    fn issue(&mut self, path: &Path, message: String) {
        if !self
            .issues
            .iter()
            .any(|issue| issue.path == path && issue.message == message)
        {
            self.issues.push(ScanIssue {
                path: path.to_path_buf(),
                message,
            });
        }
    }

    fn walk(&mut self, dir: &Path, identity: &Path, depth: usize) {
        if self
            .visited
            .get(identity)
            .is_some_and(|&seen| seen <= depth)
        {
            return;
        }
        self.visited.insert(identity.to_path_buf(), depth);
        let git = dir.join(".git");
        match fs::symlink_metadata(&git) {
            Ok(_) => {
                match checkout_config(&git).and_then(|config| {
                    self.configs
                        .entry(config.clone())
                        .or_insert_with(|| {
                            github_repositories(&config, self.hosts, self.aliases)
                                .map_err(|err| err.to_string())
                        })
                        .clone()
                }) {
                    Ok(slugs) => {
                        self.inspected.insert(identity.to_path_buf());
                        for slug in slugs {
                            record(&mut self.found, &slug, dir);
                        }
                    }
                    Err(err) => self.issue(dir, format!("could not read checkout metadata: {err}")),
                }
                // Preserve the checkout boundary, including non-GitHub repos.
                return;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                self.issue(dir, format!("could not inspect checkout: {err}"));
                return;
            }
        }
        if depth >= MAX_DEPTH {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) => {
                self.issue(dir, format!("could not scan folder: {err}"));
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    self.issue(dir, format!("could not list folder: {err}"));
                    continue;
                }
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => {
                    self.walk(&entry.path(), &identity.join(entry.file_name()), depth + 1)
                }
                Ok(_) => {} // Do not follow directory symlinks.
                Err(err) => self.issue(&entry.path(), format!("could not inspect folder: {err}")),
            }
        }
    }
}

fn open_metadata(path: &Path) -> std::io::Result<fs::File> {
    if !fs::metadata(path)?.is_file() {
        return Err(std::io::Error::other("metadata is not a regular file"));
    }
    fs::File::open(path)
}

/// Bound small path-pointer files; do not follow recursive pointer chains.
fn read_metadata(path: &Path, limit: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut contents = String::new();
    open_metadata(path)?
        .take(limit + 1)
        .read_to_string(&mut contents)?;
    if contents.len() as u64 > limit {
        return Err(std::io::Error::other("metadata file is too large"));
    }
    Ok(contents)
}

fn metadata_path(base: &Path, value: &str) -> Result<PathBuf, String> {
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.contains(['\r', '\n', '\0']) {
        return Err("invalid Git metadata path".into());
    }
    let path = base.join(value); // An absolute value replaces base.
    let path = fs::canonicalize(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    if !path.is_dir() {
        return Err(format!("{} is not a Git directory", path.display()));
    }
    Ok(path)
}

fn checkout_config(git: &Path) -> Result<PathBuf, String> {
    let git_dir = if git.is_dir() {
        fs::canonicalize(git).map_err(|err| err.to_string())?
    } else {
        let contents = read_metadata(git, 64 * 1024).map_err(|err| err.to_string())?;
        let target = contents
            .strip_prefix("gitdir: ")
            .ok_or("invalid .git file")?;
        metadata_path(git.parent().ok_or("missing checkout directory")?, target)?
    };
    let common_file = git_dir.join("commondir");
    let common = match fs::symlink_metadata(&common_file) {
        Ok(_) => {
            let contents = read_metadata(&common_file, 64 * 1024)
                .map_err(|err| format!("{}: {err}", common_file.display()))?;
            metadata_path(&git_dir, &contents)?
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => git_dir,
        Err(err) => return Err(format!("{}: {err}", common_file.display())),
    };
    Ok(common.join("config"))
}

/// Literal Host aliases in ~/.ssh/config. Keep the first HostName, including
/// unsupported destinations, so a later block cannot rebind an alias to GitHub.
/// Complex OpenSSH rules (Include, Match, wildcard/negated Host blocks) are not
/// evaluated; only straightforward literal aliases are supported.
fn ssh_aliases(config: &str) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    let mut current: Vec<String> = Vec::new();
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((key, value)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            continue;
        };
        let value = value.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                let patterns: Vec<_> = value.split_whitespace().collect();
                current = if patterns.iter().any(|p| p.contains(['*', '?', '!'])) {
                    Vec::new()
                } else {
                    patterns.into_iter().filter_map(normalize_host).collect()
                };
            }
            "match" | "include" => current.clear(),
            "hostname" => {
                if let Some(host) = normalize_host(value) {
                    for alias in &current {
                        aliases.entry(alias.clone()).or_insert_with(|| host.clone());
                    }
                }
            }
            _ => {}
        }
    }
    aliases
}

fn github_repositories(
    config: &Path,
    hosts: &[String],
    aliases: &BTreeMap<String, String>,
) -> std::io::Result<Vec<RepositoryId>> {
    use std::io::{BufRead, BufReader, Error, ErrorKind, Read};
    // Stream arbitrary-size configs, bounding allocation before reading each
    // line. An oversized line warns and retains the checkout's cached state.
    let mut reader = BufReader::new(open_metadata(config)?);
    let mut bytes = Vec::new();
    let mut slugs = Vec::new();
    let mut in_remote = false;
    loop {
        bytes.clear();
        let count = reader
            .by_ref()
            .take(MAX_CONFIG_LINE_BYTES + 2)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        // LF and CRLF terminate the line but don't count toward its content
        // limit. The bounded read allows both bytes without unbounded growth.
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        if bytes.len() as u64 > MAX_CONFIG_LINE_BYTES {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Git config line exceeds 64 KiB",
            ));
        }
        let line =
            std::str::from_utf8(&bytes).map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
        let line = line.trim();
        if line.starts_with('[') {
            in_remote = line.to_ascii_lowercase().starts_with("[remote ");
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if in_remote
            && key.trim().eq_ignore_ascii_case("url")
            && let Some(slug) = parse_github_url(value.trim(), hosts, aliases)
        {
            slugs.push(slug);
        }
    }
    slugs.sort();
    slugs.dedup();
    Ok(slugs)
}

/// Only SSH remotes resolve aliases. HTTPS authorities must name an API host
/// directly; an SSH alias never authorizes HTTPS on a different destination.
fn parse_github_url(
    url: &str,
    hosts: &[String],
    aliases: &BTreeMap<String, String>,
) -> Option<RepositoryId> {
    let (authority, path, ssh) = match url.split_once("://") {
        Some(("https", rest)) => {
            let (authority, path) = rest.split_once('/')?;
            (authority, path, false)
        }
        Some(("ssh", rest)) => {
            let (authority, path) = rest.split_once('/')?;
            (authority, path, true)
        }
        Some(_) => return None,
        None => {
            let (authority, path) = url.split_once(':')?;
            (authority, path, true)
        }
    };
    let authority = authority.rsplit('@').next()?;
    let raw_host = if let Some((host, port)) = authority.split_once(':') {
        let port: u16 = port.parse().ok()?;
        if port == 0 || (!ssh && port != 443) {
            return None;
        }
        host
    } else {
        authority
    };
    let host = normalize_host(raw_host)?;
    let host = if ssh {
        aliases.get(&host).unwrap_or(&host)
    } else {
        &host
    };
    // GitHub's SSH-over-443 endpoint serves github.com repositories. It is a
    // transport destination, not a separate host for API calls or saved state.
    let host = if ssh && host == "ssh.github.com" {
        PUBLIC_HOST
    } else {
        host.as_str()
    };
    if !hosts.iter().any(|known| known.eq_ignore_ascii_case(host)) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let valid_component = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    };
    (valid_component(owner) && valid_component(name))
        .then(|| RepositoryId::new(host, &format!("{owner}/{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn discover(roots: &[PathBuf]) -> ScanResult {
        super::discover_with_aliases(roots, &hosts(), &BTreeMap::new())
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "octowatcher-discovery-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn checkout(&self, name: &str, urls: &[&str]) -> PathBuf {
            let dir = self.path(name);
            fs::create_dir_all(dir.join(".git")).unwrap();
            write_remotes(&dir.join(".git/config"), urls);
            dir
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_remotes(config: &Path, urls: &[&str]) {
        let text: String = urls
            .iter()
            .enumerate()
            .map(|(i, url)| format!("[remote \"remote{i}\"]\n\turl = {url}\n"))
            .collect();
        fs::write(config, text).unwrap();
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Discovery Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "--initial-branch=main"]);
        git(dir, &["commit", "--allow-empty", "-m", "fixture"]);
    }

    #[test]
    fn enterprise_worktree_survives_metadata_outage_and_remote_changes() {
        let f = Fixture::new();
        let main = f.path("external/main");
        init_repo(&main);
        git(
            &main,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        git(
            &main,
            &["remote", "add", "upstream", "git@work:owner/repo.git"],
        );
        let root = f.path("watched");
        fs::create_dir(&root).unwrap();
        let worktree = root.join("worktree");
        git(
            &main,
            &["worktree", "add", "--detach", worktree.to_str().unwrap()],
        );
        let roots = [root, worktree.clone()];
        let aliases = ssh_aliases("Host work\n HostName github.example.com\n");
        let mut previous = discover_with_aliases(&roots, &hosts(), &aliases).repos;
        assert_eq!(previous.len(), 2);
        assert!(
            previous
                .iter()
                .all(|repo| repo.paths == vec![worktree.clone()])
        );
        assert_ne!(previous[0].id, previous[1].id);

        // Both hosts share the external config but keep separate cache entries.
        fs::rename(main.join(".git"), f.path("offline")).unwrap();
        let mut unavailable = discover_with_aliases(&roots, &hosts(), &aliases);
        assert_eq!(unavailable.issues.len(), 1);
        unavailable.retain_unavailable(&previous);
        assert_eq!(unavailable.repos, previous);
        fs::rename(f.path("offline"), main.join(".git")).unwrap();

        git(
            &main,
            &[
                "remote",
                "set-url",
                "upstream",
                "git@work:owner/changed.git",
            ],
        );
        let mut recovered = discover_with_aliases(&roots, &hosts(), &aliases);
        assert!(recovered.issues.is_empty());
        recovered.retain_unavailable(&previous);
        assert!(
            recovered
                .repos
                .iter()
                .any(|repo| repo.id == RepositoryId::new("github.com", "owner/repo"))
        );
        assert!(
            recovered
                .repos
                .iter()
                .any(|repo| repo.id == RepositoryId::new("github.example.com", "owner/changed"))
        );
        assert_eq!(recovered.repos.len(), 2);
        previous = recovered.repos;
        git(&main, &["worktree", "remove", worktree.to_str().unwrap()]);
        let mut removed = discover_with_aliases(&roots, &hosts(), &aliases);
        // Removing an explicitly watched folder is unavailable, so scan the
        // readable parent alone to establish that the checkout disappeared.
        assert_eq!(removed.issues.len(), 1);
        removed = discover_with_aliases(&roots[..1], &hosts(), &aliases);
        removed.retain_unavailable(&previous);
        assert!(removed.repos.is_empty());
    }

    #[test]
    fn observes_create_move_remove_and_remote_changes() {
        let f = Fixture::new();
        let roots = [f.0.clone()];
        assert!(discover(&roots).repos.is_empty());
        let clone = f.checkout("clone", &["git@github.com:owner/old.git"]);
        assert_eq!(discover(&roots).repos[0].id.slug, "owner/old");
        write_remotes(
            &clone.join(".git/config"),
            &["https://github.com/owner/new.git"],
        );
        assert_eq!(discover(&roots).repos[0].id.slug, "owner/new");
        let moved = f.path("moved");
        fs::rename(&clone, &moved).unwrap();
        assert_eq!(discover(&roots).repos[0].paths, vec![moved.clone()]);
        fs::remove_dir_all(&moved).unwrap();
        assert!(discover(&roots).repos.is_empty());
    }

    #[test]
    fn resolves_real_worktree_with_main_outside_roots_and_shared_remotes() {
        let f = Fixture::new();
        let main = f.path("outside/main");
        init_repo(&main);
        git(
            &main,
            &["remote", "add", "origin", "git@github.com:Owner/Repo.git"],
        );
        git(
            &main,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/upstream/repo.git",
            ],
        );
        let watched = f.path("watched");
        fs::create_dir(&watched).unwrap();
        let worktree = watched.join("linked checkout");
        git(
            &main,
            &["worktree", "add", "--detach", worktree.to_str().unwrap()],
        );
        let scan = discover(std::slice::from_ref(&watched));
        assert!(scan.issues.is_empty(), "{:?}", scan.issues);
        assert_eq!(scan.repos.len(), 2);
        assert_eq!(scan.repos[0].id.slug, "owner/repo");
        assert_eq!(scan.repos[0].paths, vec![worktree.clone()]);
        git(
            &main,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:owner/changed.git",
            ],
        );
        let scan = discover(&[watched, main.clone()]);
        assert!(scan.issues.is_empty());
        assert_eq!(scan.repos[0].id.slug, "owner/changed");
        assert_eq!(scan.repos[0].paths.len(), 2);
    }

    #[test]
    fn resolves_relative_gitfile_and_absolute_common_dir_without_following_chains() {
        let f = Fixture::new();
        let common = f.path("external/common");
        let metadata = f.path("external/worktree");
        fs::create_dir_all(&common).unwrap();
        fs::create_dir_all(&metadata).unwrap();
        write_remotes(&common.join("config"), &["git@github.com:o/r.git"]);
        fs::write(
            metadata.join("commondir"),
            format!("{}\n", common.display()),
        )
        .unwrap();
        let checkout = f.path("watched/checkout");
        fs::create_dir_all(&checkout).unwrap();
        fs::write(checkout.join(".git"), "gitdir: ../../external/worktree\r\n").unwrap();
        let scan = discover(&[f.path("watched")]);
        assert!(scan.issues.is_empty());
        assert_eq!(scan.repos[0].paths, vec![checkout]);
    }

    #[test]
    fn nested_submodule_requires_explicit_watched_root() {
        let f = Fixture::new();
        let parent = f.path("parent");
        let source = f.path("outside/source");
        init_repo(&parent);
        init_repo(&source);
        git(
            &parent,
            &["remote", "add", "origin", "git@github.com:o/parent.git"],
        );
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                source.to_str().unwrap(),
                "deps/module",
            ],
        );
        let submodule = parent.join("deps/module");
        git(
            &submodule,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/o/module.git",
            ],
        );
        let scan = discover(std::slice::from_ref(&parent));
        assert_eq!(scan.repos.len(), 1);
        assert_eq!(scan.repos[0].id.slug, "o/parent");
        let scan = discover(&[parent, submodule.clone()]);
        assert!(scan.issues.is_empty());
        assert_eq!(scan.repos.len(), 2);
        assert_eq!(scan.repos[0].id.slug, "o/module");
        assert_eq!(scan.repos[0].paths, vec![submodule]);
    }

    #[test]
    fn overlaps_deduplicate_paths_but_allow_deeper_explicit_roots() {
        let f = Fixture::new();
        let root = f.path("root");
        let deep = f.checkout("root/a/b/c/d/e/too-deep", &["git@github.com:o/deep.git"]);
        let clone = f.checkout(
            "root/clone",
            &["git@github.com:O/R.git", "https://github.com/o/r.git"],
        );
        assert_eq!(discover(std::slice::from_ref(&root)).repos.len(), 1);
        let roots = [root.clone(), clone.clone(), root.join("a"), root];
        let scan = discover(&roots);
        assert!(scan.issues.is_empty());
        assert_eq!(scan.repos.len(), 2);
        assert_eq!(scan.repos[0].paths, vec![deep]);
        assert_eq!(scan.repos[1].paths, vec![clone]);
    }

    #[test]
    fn preserves_depth_skips_checkout_boundaries_and_ssh_aliases() {
        let f = Fixture::new();
        let at_limit = f.checkout("a/b/c/d/e", &["git@github-work:o/visible.git"]);
        f.checkout("x/a/b/c/d/e", &["git@github.com:o/too-deep.git"]);
        for name in super::SKIPPED_DIRS.iter().copied().chain([".hidden"]) {
            f.checkout(&format!("{name}/clone"), &["git@github.com:o/skipped.git"]);
        }
        f.checkout("local", &["git@gitlab.com:o/local.git"]);
        f.checkout("local/nested", &["git@github.com:o/nested.git"]);
        let scan = discover_with_aliases(
            std::slice::from_ref(&f.0),
            &["github.com".into()],
            &ssh_aliases("Host github-work\n HostName github.com\n"),
        );
        assert!(scan.issues.is_empty());
        assert_eq!(scan.repos.len(), 1);
        assert_eq!(scan.repos[0].paths, vec![at_limit]);
    }

    #[test]
    fn missing_roots_and_broken_metadata_keep_known_paths_until_recovery() {
        let f = Fixture::new();
        let root = f.path("root");
        let clone = f.checkout("root/clone", &["git@github.com:o/r.git"]);
        let previous = discover(std::slice::from_ref(&root)).repos;
        fs::rename(&root, f.path("disconnected")).unwrap();
        let mut scan = discover(std::slice::from_ref(&root));
        assert_eq!(scan.issues.len(), 1);
        scan.retain_unavailable(&within_roots(&previous, std::slice::from_ref(&root)));
        assert_eq!(scan.repos, previous);
        assert!(within_roots(&previous, &[]).is_empty());
        fs::rename(f.path("disconnected"), &root).unwrap();
        fs::remove_dir_all(clone.join(".git")).unwrap();
        fs::write(clone.join(".git"), "gitdir: ../../../does-not-exist\n").unwrap();
        let mut scan = discover(std::slice::from_ref(&root));
        assert_eq!(scan.issues[0].path, clone);
        scan.retain_unavailable(&previous);
        assert_eq!(scan.repos, previous);
        fs::remove_file(clone.join(".git")).unwrap();
        let mut scan = discover(std::slice::from_ref(&root));
        scan.retain_unavailable(&previous);
        assert!(scan.issues.is_empty());
        assert!(scan.repos.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_folder_is_reported_and_retried() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let clone = f.checkout("root/clone", &["git@github.com:o/r.git"]);
        let root = f.path("root");
        let previous = discover(std::slice::from_ref(&root)).repos;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
        let mut scan = discover(std::slice::from_ref(&root));
        // Root users can read mode 000; still restore permissions before asserting.
        let inaccessible = fs::read_dir(&root).is_err();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        if inaccessible {
            assert!(!scan.issues.is_empty());
            scan.retain_unavailable(&previous);
            assert_eq!(scan.repos[0].paths, vec![clone]);
        }
        assert!(discover(&[root]).issues.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_symlink_roots_deduplicate_and_retain_paths_when_unavailable() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let root = f.path("actual");
        f.checkout("actual/clone", &["git@github.com:o/r.git"]);
        let alias = f.path("alias");
        symlink(&root, &alias).unwrap();
        let scan = discover(&[alias.clone(), root.clone()]);
        assert_eq!(scan.repos[0].paths, vec![alias.join("clone")]);
        // A remaining physical root still owns a path cached via the alias.
        let previous = within_roots(&scan.repos, std::slice::from_ref(&root));
        let mut failed_physical = ScanResult {
            issues: vec![ScanIssue {
                path: root.clone(),
                message: "unreadable".into(),
            }],
            ..ScanResult::default()
        };
        failed_physical.retain_unavailable(&previous);
        assert_eq!(failed_physical.repos, scan.repos);
        fs::rename(&root, f.path("unmounted")).unwrap();
        let mut failed = discover(std::slice::from_ref(&alias));
        let previous = within_roots(&scan.repos, std::slice::from_ref(&alias));
        failed.retain_unavailable(&previous);
        assert_eq!(failed.repos, scan.repos);
        assert!(!failed.issues.is_empty());
        fs::rename(f.path("unmounted"), &root).unwrap();
        assert!(discover(&[alias]).issues.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn review_regression_unavailable_git_symlink_preserves_saved_snooze() {
        use crate::store::{PendingReview, Snooze, Store};
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let clone = f.checkout("watched/clone", &["git@github.com:o/r.git"]);
        let root = f.path("watched");
        let metadata = f.path("external-metadata");
        fs::rename(clone.join(".git"), &metadata).unwrap();
        symlink(&metadata, clone.join(".git")).unwrap();
        let previous = discover(std::slice::from_ref(&root)).repos;
        assert_eq!(previous.len(), 1);
        let mut store = Store {
            pending: vec![PendingReview {
                host: crate::repository::default_host(),
                account: "alice".into(),
                account_id: 1,
                repo: "o/r".into(),
                number: 1,
                title: "review".into(),
                url: "https://github.com/o/r/pull/1".into(),
                author: "reviewer".into(),
                is_draft: false,
                rereview: false,
                requested_at: None,
            }],
            snoozed: vec![Snooze {
                host: crate::repository::default_host(),
                account: "alice".into(),
                account_id: 1,
                repo: "o/r".into(),
                number: 1,
                until: 100,
                requested_at: None,
            }],
            ..Store::default()
        };
        store.queue_notifications(&store.pending.clone());
        let saved_notices = store.notification_queue.clone();
        fs::rename(&metadata, f.path("offline")).unwrap();
        let mut scan = discover(std::slice::from_ref(&root));
        scan.retain_unavailable(&previous);
        let watched = scan.repos.iter().map(|repo| repo.id.clone()).collect();
        store.retain_watched(&watched);
        assert_eq!(
            store.snoozed.len(),
            1,
            "unavailable metadata must not discard a saved snooze"
        );
        assert_eq!(store.pending.len(), 1);
        assert_eq!(store.notification_queue, saved_notices);
        assert_eq!(scan.repos, previous);
        assert_eq!(scan.issues[0].path, clone);
        fs::rename(f.path("offline"), &metadata).unwrap();
        let recovered = discover(&[root]);
        assert!(recovered.issues.is_empty());
        assert_eq!(recovered.repos, previous);
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_common_dir_symlink_does_not_fall_back_to_private_config() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let clone = f.checkout("watched/clone", &["git@github.com:o/private.git"]);
        let common = f.path("common");
        fs::create_dir(&common).unwrap();
        write_remotes(&common.join("config"), &["git@github.com:o/shared.git"]);
        let pointer = f.path("common-pointer");
        fs::write(&pointer, common.to_str().unwrap()).unwrap();
        symlink(&pointer, clone.join(".git/commondir")).unwrap();
        let root = f.path("watched");
        let previous = discover(std::slice::from_ref(&root)).repos;
        assert_eq!(previous[0].id.slug, "o/shared");
        fs::rename(&pointer, f.path("offline-pointer")).unwrap();
        let mut scan = discover(std::slice::from_ref(&root));
        scan.retain_unavailable(&previous);
        assert_eq!(scan.repos, previous);
        assert_eq!(scan.issues[0].path, clone);
        fs::rename(f.path("offline-pointer"), &pointer).unwrap();
        assert!(discover(&[root]).issues.is_empty());
    }

    #[test]
    fn review_regression_config_line_content_limit_excludes_terminators() {
        let f = Fixture::new();
        let clone = f.checkout("clone", &[]);
        let config = clone.join(".git/config");
        let limit = super::MAX_CONFIG_LINE_BYTES as usize;
        for content_length in [limit - 1, limit, limit + 1] {
            for ending in ["\n", "\r\n", ""] {
                let mut line = "url = https://github.com/o/boundary.git".to_string();
                line.extend(std::iter::repeat_n(' ', content_length - line.len()));
                fs::write(&config, format!("[remote \"origin\"]\n{line}{ending}")).unwrap();
                let scan = discover(std::slice::from_ref(&f.0));
                if content_length <= limit {
                    assert!(
                        scan.issues.is_empty(),
                        "length {content_length}, ending {ending:?}"
                    );
                    assert_eq!(scan.repos[0].id.slug, "o/boundary");
                } else {
                    assert!(scan.repos.is_empty());
                    assert_eq!(scan.issues.len(), 1, "ending {ending:?}");
                }
            }
        }
    }

    #[test]
    fn review_regression_oversized_config_line_warns_and_retains_known_checkout() {
        let f = Fixture::new();
        let clone = f.checkout("watched/clone", &["git@github.com:o/r.git"]);
        let root = f.path("watched");
        let previous = discover(std::slice::from_ref(&root)).repos;
        let config = clone.join(".git/config");
        let original = fs::read(&config).unwrap();
        let mut oversized = vec![b'#'; 2 * 1024 * 1024];
        oversized.push(b'\n');
        oversized.extend_from_slice(&original);
        fs::write(&config, &oversized).unwrap();
        let mut scan = discover(std::slice::from_ref(&root));
        assert_eq!(scan.issues.len(), 1);
        assert_eq!(scan.issues[0].path, clone);
        assert!(scan.issues[0].message.contains("config line exceeds"));
        scan.retain_unavailable(&previous);
        assert_eq!(scan.repos, previous);
        // An unterminated oversized line must hit the same bound.
        fs::write(&config, &oversized[..2 * 1024 * 1024]).unwrap();
        assert_eq!(discover(std::slice::from_ref(&root)).issues.len(), 1);
        fs::write(config, original).unwrap();
        let recovered = discover(&[root]);
        assert!(recovered.issues.is_empty());
        assert_eq!(recovered.repos, previous);
    }

    #[test]
    fn review_regression_valid_config_larger_than_one_mib_is_discovered() {
        let f = Fixture::new();
        let clone = f.checkout("clone", &[]);
        let mut contents = "# valid padding comment\n".repeat(50_000);
        contents.push_str("[remote \"origin\"]\nurl = https://github.com/o/large.git\n");
        assert!(contents.len() > 1024 * 1024);
        fs::write(clone.join(".git/config"), contents).unwrap();
        let scan = discover(std::slice::from_ref(&f.0));
        assert_eq!(
            scan.repos.len(),
            1,
            "valid configs must not be excluded by their total size"
        );
        assert_eq!(scan.repos[0].id.slug, "o/large");
        assert!(scan.issues.is_empty());
    }

    #[test]
    fn malformed_and_oversized_metadata_report_checkout_scoped_issues() {
        let f = Fixture::new();
        for (name, contents) in [
            ("bad", "not a gitfile".to_string()),
            ("large", format!("gitdir: {}", "a".repeat(64 * 1024))),
        ] {
            let checkout = f.path(name);
            fs::create_dir(&checkout).unwrap();
            fs::write(checkout.join(".git"), contents).unwrap();
        }
        let clone = f.checkout("bad-config", &["git@github.com:o/r.git"]);
        fs::remove_file(clone.join(".git/config")).unwrap();
        fs::create_dir(clone.join(".git/config")).unwrap();
        let scan = discover(std::slice::from_ref(&f.0));
        assert!(scan.repos.is_empty());
        assert_eq!(scan.issues.len(), 3);
    }

    #[test]
    fn a_successful_checkout_overrides_a_broader_scan_failure() {
        let path = PathBuf::from("/root/clone");
        let previous = [LocalRepo {
            id: RepositoryId::new("github.com", "o/old"),
            paths: vec![path.clone()],
        }];
        let mut scan = ScanResult {
            repos: vec![LocalRepo {
                id: RepositoryId::new("github.com", "o/new"),
                paths: vec![path.clone()],
            }],
            issues: vec![ScanIssue {
                path: "/root".into(),
                message: "partial listing".into(),
            }],
            inspected: [path].into(),
        };
        scan.retain_unavailable(&previous);
        assert_eq!(scan.repos.len(), 1);
        assert_eq!(scan.repos[0].id.slug, "o/new");
    }

    #[test]
    #[ignore = "manual filesystem performance check"]
    fn scan_performance() {
        let f = Fixture::new();
        for i in 0..2_000 {
            fs::create_dir_all(f.path(&format!("folders/group{}/dir{i}", i % 50))).unwrap();
        }
        for i in 0..200 {
            f.checkout(
                &format!("clones/repo{i}"),
                &[&format!("git@github.com:o/repo{i}.git")],
            );
        }
        let common = f.path("metadata/common");
        fs::create_dir_all(&common).unwrap();
        write_remotes(&common.join("config"), &["git@github.com:o/shared.git"]);
        for i in 0..200 {
            let metadata = f.path(&format!("metadata/worktrees/w{i}"));
            fs::create_dir_all(&metadata).unwrap();
            fs::write(metadata.join("commondir"), "../../common\n").unwrap();
            let checkout = f.path(&format!("worktrees/w{i}"));
            fs::create_dir_all(&checkout).unwrap();
            fs::write(
                checkout.join(".git"),
                format!("gitdir: {}\n", metadata.display()),
            )
            .unwrap();
        }
        let start = std::time::Instant::now();
        for _ in 0..5 {
            let scan = discover(&[f.0.clone(), f.path("folders"), f.path("clones")]);
            assert!(scan.issues.is_empty());
            assert_eq!(scan.repos.len(), 201);
            assert_eq!(
                scan.repos
                    .iter()
                    .find(|repo| repo.id.slug == "o/shared")
                    .unwrap()
                    .paths
                    .len(),
                200
            );
        }
        println!(
            "Five scans of 2,000 folders, 200 clones and 200 linked worktrees with overlapping roots: {:?}",
            start.elapsed()
        );
    }

    fn hosts() -> Vec<String> {
        ["github.com", "github.example.com", "acme.ghe.com"]
            .map(str::to_string)
            .to_vec()
    }

    #[test]
    fn parses_remote_forms_and_canonicalizes_aliases() {
        let aliases = ssh_aliases(
            "Host work\n HostName github.example.com\nHost public\n HostName github.com",
        );
        for (url, host) in [
            ("git@github.com:Owner/Repo.git", "github.com"),
            (
                "ssh://git@github.example.com/Owner/Repo.git",
                "github.example.com",
            ),
            ("ssh://git@work:2222/Owner/Repo.git", "github.example.com"),
            (
                "https://github.example.com:443/Owner/Repo.git",
                "github.example.com",
            ),
            ("https://ACME.GHE.COM/Owner/Repo/", "acme.ghe.com"),
            ("git@public:Owner/Repo", "github.com"),
        ] {
            assert_eq!(
                parse_github_url(url, &hosts(), &aliases),
                Some(RepositoryId::new(host, "owner/repo")),
                "{url}"
            );
        }
        for url in [
            "https://work/owner/repo.git",
            "http://github.com/owner/repo",
            "git://github.com/owner/repo",
            "git@gitlab.com:owner/repo.git",
            "https://github.com:8443/owner/repo",
            "ssh://github.com:bad/owner/repo",
            "https://github.com/owner",
            "https://github.com/owner/repo/pull/1",
            "https://github.com/owner/repo?x=1",
            "/some/local/path",
            "../owner/repo",
            "git@unknown.ghe.com:owner/repo.git",
            "https://github.com/../repo",
        ] {
            assert_eq!(parse_github_url(url, &hosts(), &aliases), None, "{url}");
        }
    }

    #[test]
    fn aliases_keep_destinations_separate_and_first_hostname_wins() {
        let config = "Host work gh\n HostName=github.example.com # comment\nHost public\n HostName github.com\nHost other\n HostName gitlab.com\nHost other\n HostName github.com\nHost neg !neg\n HostName github.com\nHost *\n HostName github.com\nMatch all\n HostName github.com\n";
        let aliases = ssh_aliases(config);
        assert_eq!(aliases.get("work").unwrap(), "github.example.com");
        assert_eq!(aliases.get("gh").unwrap(), "github.example.com");
        assert_eq!(aliases.get("public").unwrap(), "github.com");
        assert!(!aliases.contains_key("neg"));
        assert_eq!(
            parse_github_url("git@other:owner/repo", &hosts(), &aliases),
            None
        );
    }

    #[test]
    fn ssh_over_https_clones_keep_the_public_repository_identity() {
        // GitHub's documented Host github.com / HostName ssh.github.com override.
        let aliases = ssh_aliases(
            "Host github.com github-443\n HostName ssh.github.com\n Port 443\n User git\n",
        );
        let root = std::env::temp_dir().join(format!(
            "octowatcher-discovery-ssh443-{}",
            std::process::id()
        ));
        for (clone, remote) in [
            ("direct", "ssh://git@ssh.github.com:443/Owner/Repo.git"),
            ("override", "git@github.com:owner/repo.git"),
            ("alias", "git@github-443:Owner/Repo.git"),
            ("https", "https://github.com/owner/repo.git"),
        ] {
            let dir = root.join(clone).join(".git");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("config"),
                format!("[remote \"origin\"]\n url = {remote}\n"),
            )
            .unwrap();
        }
        let found = discover_with_aliases(std::slice::from_ref(&root), &hosts(), &aliases);
        fs::remove_dir_all(root).unwrap();
        assert!(found.issues.is_empty());
        assert_eq!(found.repos.len(), 1);
        assert_eq!(
            found.repos[0].id,
            RepositoryId::new("github.com", "owner/repo")
        );
        assert_eq!(found.repos[0].paths.len(), 4);
        // The SSH transport endpoint must not become an HTTPS or API-host alias.
        assert_eq!(
            parse_github_url("https://ssh.github.com/owner/repo.git", &hosts(), &aliases),
            None
        );
        assert_eq!(
            parse_github_url("https://github-443/owner/repo.git", &hosts(), &aliases),
            None
        );
    }

    #[test]
    fn discovery_groups_by_host_and_slug_without_collisions() {
        let root =
            std::env::temp_dir().join(format!("octowatcher-discovery-{}", std::process::id()));
        for clone in ["one", "two"] {
            let dir = root.join(clone).join(".git");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("config"), "[remote \"origin\"]\n url = git@github.com:Owner/Repo.git\n[remote \"upstream\"]\n url = https://github.example.com/owner/repo.git\n[remote \"duplicate\"]\n url = git@github.com:owner/repo.git\n[submodule \"ignored\"]\n url = https://github.com/other/repo.git\n").unwrap();
        }
        let found =
            discover_with_aliases(std::slice::from_ref(&root), &hosts(), &BTreeMap::new()).repos;
        fs::remove_dir_all(root).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|repo| repo.paths.len() == 2));
        assert_ne!(found[0].id, found[1].id);
    }
}
