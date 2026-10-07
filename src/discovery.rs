use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::repository::{RepositoryId, normalize_host};

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

#[derive(Debug, Default)]
pub struct Scan {
    pub repos: Vec<LocalRepo>,
    pub issues: Vec<String>,
}

/// Walks `roots` for git clones and groups them by GitHub repository. A clone
/// with several GitHub remotes (a fork and its upstream) counts for each.
pub fn discover(roots: &[PathBuf], hosts: &[String]) -> Scan {
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
) -> Scan {
    let mut found: BTreeMap<RepositoryId, LocalRepo> = BTreeMap::new();
    let mut issues = Vec::new();
    for root in roots {
        walk(
            root,
            0,
            hosts,
            aliases,
            &mut issues,
            &mut |repo_dir, repositories| {
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
            },
        );
    }
    Scan {
        repos: found.into_values().collect(),
        issues,
    }
}

fn walk(
    dir: &Path,
    depth: usize,
    hosts: &[String],
    aliases: &BTreeMap<String, String>,
    issues: &mut Vec<String>,
    on_repo: &mut impl FnMut(&Path, Vec<RepositoryId>),
) {
    let git_dir = dir.join(".git");
    // A `.git` file means a worktree or submodule: its main clone is found elsewhere.
    if git_dir.is_dir() {
        let slugs = match github_repositories(&git_dir.join("config"), hosts, aliases) {
            Ok(slugs) => slugs,
            Err(err) => {
                issues.push(format!(
                    "Cannot read {}: {err}",
                    git_dir.join("config").display()
                ));
                return;
            }
        };
        if !slugs.is_empty() {
            on_repo(dir, slugs);
        }
        return;
    }
    if depth >= MAX_DEPTH {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            issues.push(format!("Cannot scan {}: {err}", dir.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                issues.push(format!("Cannot scan {}: {err}", dir.display()));
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                issues.push(format!("Cannot inspect {}: {err}", entry.path().display()));
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_ref()) {
            continue;
        }
        walk(&entry.path(), depth + 1, hosts, aliases, issues, on_repo);
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
) -> std::io::Result<Vec<RepositoryId>> {
    let contents = fs::read_to_string(config)?;
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
    Ok(repositories)
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

    #[test]
    fn distinguishes_empty_missing_folders_and_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let empty = super::discover(&[dir.path().to_path_buf()], &hosts());
        assert!(empty.repos.is_empty());
        assert!(empty.issues.is_empty());
        let root = dir.path().join("projects");
        let missing = super::discover(std::slice::from_ref(&root), &hosts());
        assert!(missing.repos.is_empty());
        assert_eq!(missing.issues.len(), 1);
        std::fs::create_dir_all(root.join("clone/.git")).unwrap();
        let unreadable_config = super::discover(std::slice::from_ref(&root), &hosts());
        assert_eq!(unreadable_config.issues.len(), 1);
        std::fs::write(
            root.join("clone/.git/config"),
            "[remote \"origin\"]\nurl = git@github.com:o/r.git\n",
        )
        .unwrap();
        let recovered = super::discover(&[root], &hosts());
        assert!(recovered.issues.is_empty());
        assert_eq!(recovered.repos.len(), 1);
        assert_eq!(recovered.repos[0].id.slug, "o/r");
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
