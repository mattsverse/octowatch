//! Repository identity is scoped to the GitHub web host, never an SSH alias.

use std::fmt;

use serde::{Deserialize, Serialize};

pub const PUBLIC_HOST: &str = "github.com";

pub fn default_host() -> String {
    PUBLIC_HOST.into()
}

/// Case-insensitive identity shared by discovery, settings, reviews and snoozes.
/// Stored as `host/owner/name`; old `owner/name` settings mean github.com.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepositoryId {
    pub host: String,
    pub slug: String,
}

impl RepositoryId {
    pub fn new(host: &str, slug: &str) -> Self {
        Self {
            host: host.to_ascii_lowercase(),
            slug: slug.to_ascii_lowercase(),
        }
    }
}

impl fmt::Display for RepositoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.host, self.slug)
    }
}

impl From<RepositoryId> for String {
    fn from(id: RepositoryId) -> Self {
        id.to_string()
    }
}

impl TryFrom<String> for RepositoryId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let parts: Vec<_> = value.split('/').collect();
        match parts.as_slice() {
            [owner, name] if !owner.is_empty() && !name.is_empty() => {
                Ok(Self::new(PUBLIC_HOST, &value))
            }
            [host, owner, name]
                if normalize_host(host).is_some() && !owner.is_empty() && !name.is_empty() =>
            {
                Ok(Self::new(host, &format!("{owner}/{name}")))
            }
            _ => Err("expected owner/name or host/owner/name"),
        }
    }
}

/// Only bare DNS names / IPv4 addresses are supported as API hosts. Custom
/// ports, schemes and path prefixes cannot silently become a different host.
pub fn normalize_host(host: &str) -> Option<String> {
    if host.is_empty() || host.len() > 253 {
        return None;
    }
    host.split('.')
        .all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
        .then(|| host.to_ascii_lowercase())
}
