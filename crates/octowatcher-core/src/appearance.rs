use serde::{Deserialize, Serialize};

/// Saved intent, rather than the last appearance reported by the desktop.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    Light,
    Dark,
    #[default]
    System,
}

/// Read the entire JSON value before falling back, so an invalid preference
/// cannot invalidate the rest of the saved store. Syntax errors still propagate.
impl<'de> Deserialize<'de> for Appearance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value.as_str() {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        })
    }
}

impl Appearance {
    pub const CHOICES: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}
