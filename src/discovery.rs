use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::repository::{PUBLIC_HOST, RepositoryId, normalize_host};

const MAX_DEPTH: usize = 5;
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "build",
    "dist",
    "Library",
];

/// A GitHub repository with at least one clone on this machine.
#[derive(Debug, Clone)]
pub struct LocalRepo {
    pub id: RepositoryId,
    pub paths: Vec<PathBuf>,
}

/// Walks `roots` for git clones and groups them by GitHub repository. A clone
/// with several GitHub remotes (a fork and its upstream) counts for each.
pub fn discover(roots: &[PathBuf], hosts: &[String]) -> Vec<LocalRepo> {
    let config = dirs::home_dir()
        .and_then(|home| fs::read_to_string(home.join(".ssh/config")).ok())
        .unwrap_or_default();
    let aliases = ssh_aliases(&config);
    discover_with_aliases(roots, hosts, &aliases)
}

fn discover_with_aliases(
    roots: &[PathBuf],
    hosts: &[String],
    aliases: &BTreeMap<String, String>,
) -> Vec<LocalRepo> {
    let mut found: BTreeMap<RepositoryId, LocalRepo> = BTreeMap::new();
    for root in roots {
        walk(root, 0, hosts, aliases, &mut |repo_dir, repositories| {
            for id in repositories {
                found
                    .entry(id.clone())
                    .or_insert_with(|| LocalRepo {
                        id,
                        paths: Vec::new(),
                    })
                    .paths
                    .push(repo_dir.to_path_buf());
            }
        });
    }
    found.into_values().collect()
}

fn walk(
    dir: &Path,
    depth: usize,
    hosts: &[String],
    aliases: &BTreeMap<String, String>,
    on_repo: &mut impl FnMut(&Path, Vec<RepositoryId>),
) {
    let git_dir = dir.join(".git");
    // A `.git` file means a worktree or submodule: its main clone is found elsewhere.
    if git_dir.is_dir() {
        let slugs = github_repositories(&git_dir.join("config"), hosts, aliases);
        if !slugs.is_empty() {
            on_repo(dir, slugs);
        }
        return;
    }
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_ref()) {
            continue;
        }
        walk(&entry.path(), depth + 1, hosts, aliases, on_repo);
    }
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
) -> Vec<RepositoryId> {
    let Ok(contents) = fs::read_to_string(config) else {
        return Vec::new();
    };
    let mut in_remote = false;
    let mut repositories = Vec::new();
    for line in contents.lines().map(str::trim) {
        if line.starts_with('[') {
            in_remote = line.to_ascii_lowercase().starts_with("[remote ");
        } else if in_remote
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
            && let Some(id) = parse_github_url(value.trim(), hosts, aliases)
        {
            repositories.push(id);
        }
    }
    repositories.sort();
    repositories.dedup();
    repositories
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
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, RepositoryId::new("github.com", "owner/repo"));
        assert_eq!(found[0].paths.len(), 4);
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
        let found = discover_with_aliases(std::slice::from_ref(&root), &hosts(), &BTreeMap::new());
        fs::remove_dir_all(root).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|repo| repo.paths.len() == 2));
        assert_ne!(found[0].id, found[1].id);
    }
}
