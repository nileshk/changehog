//! User configuration, read from a TOML file.
//!
//! Precedence is built-in defaults, then the config file, then command-line
//! flags (applied by the frontend). A missing file just means defaults; a
//! malformed one, an unknown key or an out-of-range value is an error that
//! names the file and line.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Deserializer};

use crate::baseline::BaseMode;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarMode {
    /// Open when the terminal is wide enough.
    #[default]
    Auto,
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    // Shared by all frontends.
    /// Seconds to show each file (or scroll step) when cycling.
    #[serde(deserialize_with = "cycle_seconds")]
    pub cycle_seconds: f32,
    /// What to diff against.
    pub base: BaseMode,
    /// Seconds of inactivity before auto-follow resumes.
    #[serde(deserialize_with = "resume_after_seconds")]
    pub resume_after_seconds: f32,

    // Terminal frontend.
    /// Wrap long lines instead of cutting them off.
    pub wrap: bool,
    pub sidebar: SidebarMode,
    /// Show the git log panel.
    pub log: bool,
    /// Commits visible in the git log panel.
    #[serde(deserialize_with = "log_rows")]
    pub log_rows: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cycle_seconds: 4.0,
            base: BaseMode::SessionStart,
            resume_after_seconds: 20.0,
            wrap: false,
            sidebar: SidebarMode::Auto,
            log: true,
            log_rows: 5,
        }
    }
}

pub const CYCLE_RANGE: std::ops::RangeInclusive<f32> = 0.5..=600.0;
pub const RESUME_RANGE: std::ops::RangeInclusive<f32> = 1.0..=3600.0;
pub const LOG_ROWS_RANGE: std::ops::RangeInclusive<u16> = 1..=50;

/// A commented config file with every setting at its default.
pub const TEMPLATE: &str = r#"# changehog configuration
# Command-line flags override these settings.

# Seconds to show each file (or scroll step) when cycling. 0.5 to 600.
cycle_seconds = 4.0

# What to diff against: "session" (the working tree when changehog started)
# or "head" (the HEAD commit when changehog started).
base = "session"

# Seconds of inactivity after manual navigation before auto-follow resumes.
# 1 to 3600.
resume_after_seconds = 20.0

# Wrap long lines instead of cutting them off at the edge.
wrap = false

# File sidebar: "auto" (open when the terminal is wide enough), "open" or
# "closed".
sidebar = "auto"

# Show the git log panel at the bottom.
log = true

# Commits visible in the git log panel. 1 to 50.
log_rows = 5
"#;

impl Config {
    pub fn cycle(&self) -> Duration {
        Duration::from_secs_f32(self.cycle_seconds)
    }

    pub fn resume_after(&self) -> Duration {
        Duration::from_secs_f32(self.resume_after_seconds)
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Loads `explicit` if given (it must exist), else the default location
    /// if it exists, else the defaults.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => match default_path() {
                Some(path) if path.exists() => path,
                _ => return Ok(Self::default()),
            },
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("couldn't read config file {}", path.display()))?;
        Self::parse(&text).map_err(|e| anyhow!("in config file {}:\n{e}", path.display()))
    }
}

/// `$XDG_CONFIG_HOME/changehog/config.toml`, else
/// `~/.config/changehog/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("changehog").join("config.toml"))
}

fn in_range<'de, D: Deserializer<'de>>(
    d: D,
    range: std::ops::RangeInclusive<f32>,
) -> Result<f32, D::Error> {
    let value = f32::deserialize(d)?;
    if range.contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(format!(
            "must be between {} and {}",
            range.start(),
            range.end()
        )))
    }
}

fn cycle_seconds<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    in_range(d, CYCLE_RANGE)
}

fn resume_after_seconds<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    in_range(d, RESUME_RANGE)
}

fn log_rows<'de, D: Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    let value = u16::deserialize(d)?;
    if LOG_ROWS_RANGE.contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(format!(
            "must be between {} and {}",
            LOG_ROWS_RANGE.start(),
            LOG_ROWS_RANGE.end()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_matches_defaults() {
        assert_eq!(Config::parse(TEMPLATE).unwrap(), Config::default());
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c = Config::parse("wrap = true\nbase = \"head\"\ncycle_seconds = 2").unwrap();
        assert!(c.wrap);
        assert_eq!(c.base, BaseMode::Head);
        assert_eq!(c.cycle_seconds, 2.0);
        assert_eq!(c.sidebar, SidebarMode::Auto);
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        let err = Config::parse("wrapp = true").unwrap_err().to_string();
        assert!(err.contains("wrapp"), "{err}");

        let err = Config::parse("\ncycle_seconds = 0.1").unwrap_err().to_string();
        assert!(err.contains("between 0.5 and 600"), "{err}");
        assert!(err.contains("line 2"), "{err}");

        assert!(Config::parse("sidebar = \"sometimes\"").is_err());
        assert!(Config::parse("base = \"HEAD\"").is_err());
        assert!(Config::parse("log_rows = 0").is_err());
        assert_eq!(Config::parse("log_rows = 12").unwrap().log_rows, 12);
    }

    #[test]
    fn load_missing_explicit_file_is_an_error() {
        assert!(Config::load(Some(Path::new("/nonexistent/changehog.toml"))).is_err());
    }
}
