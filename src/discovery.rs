use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

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
    /// `owner/name` as written in the remote URL.
    pub slug: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub repos: Vec<LocalRepo>,
    pub issues: Vec<String>,
}

/// Walks `roots` for git clones and groups them by GitHub repository. A clone
/// with several GitHub remotes (a fork and its upstream) counts for each.
pub fn discover(roots: &[PathBuf]) -> Scan {
    let hosts = github_hosts();
    let mut found: BTreeMap<String, LocalRepo> = BTreeMap::new();
    let mut issues = Vec::new();
    for root in roots {
        walk(root, 0, &hosts, &mut issues, &mut |repo_dir, slugs| {
            for slug in slugs {
                found
                    .entry(slug.to_lowercase())
                    .or_insert_with(|| LocalRepo {
                        slug: slug.clone(),
                        paths: Vec::new(),
                    })
                    .paths
                    .push(repo_dir.to_path_buf());
            }
        });
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
    issues: &mut Vec<String>,
    on_repo: &mut impl FnMut(&Path, Vec<String>),
) {
    let git_dir = dir.join(".git");
    // A `.git` file means a worktree or submodule: its main clone is found elsewhere.
    if git_dir.is_dir() {
        let slugs = match github_slugs(&git_dir.join("config"), hosts) {
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
        walk(&entry.path(), depth + 1, hosts, issues, on_repo);
    }
}

/// `github.com` plus every SSH alias pointing at it, so remotes such as
/// `git@github-work:owner/repo.git` are recognised.
fn github_hosts() -> Vec<String> {
    let mut hosts = vec!["github.com".to_string()];
    let config = dirs::home_dir()
        .and_then(|home| fs::read_to_string(home.join(".ssh").join("config")).ok())
        .unwrap_or_default();
    hosts.extend(ssh_aliases_for_github(&config));
    hosts
}

fn ssh_aliases_for_github(config: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            continue;
        };
        let value = value.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
        match key.to_lowercase().as_str() {
            "host" => {
                current = value
                    .split_whitespace()
                    .filter(|pattern| !pattern.contains(['*', '?', '!']))
                    .map(str::to_lowercase)
                    .collect();
            }
            "match" => current.clear(),
            "hostname" if value.eq_ignore_ascii_case("github.com") => {
                aliases.append(&mut current);
            }
            _ => {}
        }
    }
    aliases
}

fn github_slugs(config: &Path, hosts: &[String]) -> std::io::Result<Vec<String>> {
    let contents = fs::read_to_string(config)?;
    let mut slugs: Vec<String> = contents
        .lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once('=')?;
            (key.trim() == "url").then(|| parse_github_url(value.trim(), hosts))?
        })
        .collect();
    slugs.sort_by_key(|slug| slug.to_lowercase());
    slugs.dedup_by_key(|slug| slug.to_lowercase());
    Ok(slugs)
}

/// Extracts `owner/name` from the SSH, scp-like and HTTPS forms of a remote
/// URL whose host is one of `hosts`.
fn parse_github_url(url: &str, hosts: &[String]) -> Option<String> {
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => url.split_once(':')?,
    };
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?.to_lowercase();
    if !hosts.contains(&host) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::{parse_github_url, ssh_aliases_for_github};

    #[test]
    fn distinguishes_empty_missing_folders_and_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let empty = super::discover(&[dir.path().to_path_buf()]);
        assert!(empty.repos.is_empty());
        assert!(empty.issues.is_empty());
        let root = dir.path().join("projects");
        let missing = super::discover(std::slice::from_ref(&root));
        assert!(missing.repos.is_empty());
        assert_eq!(missing.issues.len(), 1);
        std::fs::create_dir_all(root.join("clone/.git")).unwrap();
        let unreadable_config = super::discover(std::slice::from_ref(&root));
        assert_eq!(unreadable_config.issues.len(), 1);
        std::fs::write(
            root.join("clone/.git/config"),
            "[remote \"origin\"]\nurl = git@github.com:o/r.git\n",
        )
        .unwrap();
        let recovered = super::discover(&[root]);
        assert!(recovered.issues.is_empty());
        assert_eq!(recovered.repos.len(), 1);
        assert_eq!(recovered.repos[0].slug, "o/r");
    }

    #[test]
    fn parses_remote_forms() {
        let hosts = vec!["github.com".to_string(), "github-work".to_string()];
        for url in [
            "git@github.com:owner/repo.git",
            "git@github.com:owner/repo",
            "ssh://git@github.com/owner/repo.git",
            "ssh://git@github.com:22/owner/repo.git",
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo/",
            "git@github-work:owner/repo.git",
        ] {
            assert_eq!(
                parse_github_url(url, &hosts).as_deref(),
                Some("owner/repo"),
                "{url}"
            );
        }
        assert_eq!(
            parse_github_url("git@gitlab.com:owner/repo.git", &hosts),
            None
        );
        assert_eq!(parse_github_url("https://github.com/owner", &hosts), None);
        assert_eq!(parse_github_url("/some/local/path", &hosts), None);
    }

    #[test]
    fn finds_ssh_aliases() {
        let config = "Host github.com\n  HostName github.com\nHost github-work gh\n  HostName github.com\nHost other\n  HostName example.com\nHost *\n  HostName github.com\n";
        assert_eq!(
            ssh_aliases_for_github(config),
            vec!["github.com", "github-work", "gh"]
        );
    }
}
