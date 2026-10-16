//! Optional user config, read from `~/.config/plans/config.toml`.
//!
//! Precedence is always **CLI flag > config file > built-in default**, so
//! anything set here can be overridden for one invocation. A broken config in
//! the default location warns to stderr and falls back to defaults — a typo
//! should not make the dashboard unusable. An explicitly passed `--config` is
//! fatal instead, because that file was asked for by name.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::app::{DateFormat, GroupBy, RepoColumn, Sort};
use crate::plan::Status;

/// Where the config lives, relative to `$HOME`. `~/.config` is often kept in
/// a versioned dotfiles repo, so this file travels with the rest of a setup.
pub const DEFAULT_RELATIVE: &str = ".config/plans/config.toml";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `iso` (2026-09-14) or `short` (9/14) dates in the list.
    pub date_format: Option<DateFormat>,
    /// `status`, `repo`, `week` or `none`.
    pub group_by: Option<GroupBy>,
    /// `updated`, `created`, `title` or `repo`.
    pub sort: Option<Sort>,
    pub reverse: Option<bool>,
    /// Default status filter, e.g. `["todo", "wip"]`. Beware: unlike the other
    /// settings this narrows *every* mode, including `--json`.
    pub status: Option<Vec<String>>,
    /// Force the detail pane on/off; absent means "on when the terminal is
    /// wide enough".
    pub detail: Option<bool>,
    pub mouse: Option<bool>,
    /// `auto` hides the repo column when the list is grouped by repo.
    pub repo_column: Option<RepoColumn>,
    /// Drop a leading `YYYY-MM-DD-` from displayed plan filenames. The date is
    /// already its own column, so this buys back width. Display only: the slug
    /// used by selectors (`--handoff`, `--copy`) and `--json` never changes.
    pub strip_slug_date: Option<bool>,
    /// Days after which a TODO/WIP/UAT plan counts as stale: `--stale`'s
    /// default cutoff and the TUI's red marker. `0` disables the marker.
    pub stale_days: Option<i64>,
    /// Start the TUI with byte-identical duplicate copies hidden (`x` toggles).
    pub hide_duplicates: Option<bool>,
    /// Consult git for per-plan clean/modified/untracked status (`--git`).
    pub git: Option<bool>,
    /// Scan roots, `~` allowed. Defaults to `~/src`.
    pub roots: Option<Vec<PathBuf>>,
    /// Path substrings to ignore during the walk, tested against the path
    /// relative to the scan root. Not a glob: each entry is a plain "path
    /// contains this string" test, so `"docs/adr"` drops every path containing
    /// it and `"node_modules"` drops every dependency tree. Glob-shaped
    /// entries like `"**/node_modules/**"` match nothing — write the substring.
    pub exclude: Option<Vec<String>>,
}

/// A loaded config plus anything worth telling the user about it.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub config: Config,
    /// The file actually read, if any.
    pub path: Option<PathBuf>,
    pub warnings: Vec<String>,
}

impl Config {
    /// The configured default status filter, if valid. Unknown names are
    /// reported rather than silently dropped.
    pub fn statuses(&self) -> (Option<Vec<Status>>, Option<String>) {
        let Some(names) = &self.status else {
            return (None, None);
        };
        let mut statuses = Vec::new();
        let mut unknown = Vec::new();
        for name in names {
            match Status::named(name) {
                Some(status) => statuses.push(status),
                None => unknown.push(name.clone()),
            }
        }
        let warning = (!unknown.is_empty()).then(|| {
            format!(
                "config: ignoring unknown status name(s): {}",
                unknown.join(", ")
            )
        });
        (Some(statuses), warning)
    }

    /// Scan roots with `~` expanded. Empty means "not configured".
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|path| expand_tilde(path))
            .collect()
    }
}

/// Read the config. `explicit` is `--config`; anything wrong with a file the
/// user named is an error, anything wrong with the default file is a warning.
pub fn load(explicit: Option<&Path>) -> anyhow::Result<Loaded> {
    let Some(path) = resolve_path(explicit) else {
        return Ok(Loaded::default());
    };

    if !path.is_file() {
        if explicit.is_some() {
            anyhow::bail!("config file {} does not exist", path.display());
        }
        return Ok(Loaded::default());
    }

    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    match toml::from_str::<Config>(&text) {
        Ok(config) => Ok(Loaded {
            config,
            path: Some(path),
            warnings: Vec::new(),
        }),
        Err(err) if explicit.is_some() => {
            Err(anyhow::Error::from(err).context(format!("parsing {}", path.display())))
        }
        Err(err) => Ok(Loaded {
            warnings: vec![format!("ignoring {}: {err}", path.display())],
            ..Loaded::default()
        }),
    }
}

fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PLANS_CONFIG") {
        return Some(PathBuf::from(path));
    }
    Some(dirs::home_dir()?.join(DEFAULT_RELATIVE))
}

fn expand_tilde(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    if let Some(rest) = text.strip_prefix("~/") {
        return Some(dirs::home_dir()?.join(rest));
    }
    if text == "~" {
        return dirs::home_dir();
    }
    Some(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_setting() {
        let config: Config = toml::from_str(
            r#"
            date_format = "short"
            group_by = "week"
            sort = "title"
            reverse = true
            detail = false
            mouse = false
            repo_column = "never"
            status = ["todo", "wip", "unknown"]
            stale_days = 21
            hide_duplicates = true
            git = true
            roots = ["~/src", "/tmp/extra"]
            exclude = ["node_modules", "docs/adr", "some-repo/docs/notes"]
            "#,
        )
        .expect("valid config");
        assert_eq!(config.date_format, Some(DateFormat::Short));
        assert_eq!(config.group_by, Some(GroupBy::Date));
        assert_eq!(config.sort, Some(Sort::Title));
        assert_eq!(config.reverse, Some(true));
        assert_eq!(config.detail, Some(false));
        assert_eq!(config.repo_column, Some(RepoColumn::Never));
        assert_eq!(config.stale_days, Some(21));
        assert_eq!(config.hide_duplicates, Some(true));
        assert_eq!(config.git, Some(true));
        let (statuses, warning) = config.statuses();
        assert!(warning.is_none());
        assert_eq!(statuses.unwrap().len(), 3);
        let roots = config.roots();
        assert_eq!(roots.len(), 2);
        assert!(roots[0].is_absolute() && roots[0].ends_with("src"));
        assert_eq!(roots[1], PathBuf::from("/tmp/extra"));
        assert_eq!(
            config.exclude,
            Some(vec![
                "node_modules".to_string(),
                "docs/adr".to_string(),
                "some-repo/docs/notes".to_string()
            ])
        );
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let config: Config = toml::from_str("").expect("empty is fine");
        assert_eq!(config.date_format, None);
        assert_eq!(config.statuses().0, None);
        assert!(config.roots().is_empty());
        assert!(config.exclude.is_none());
    }

    #[test]
    fn a_typo_is_an_error_rather_than_a_silent_ignore() {
        assert!(toml::from_str::<Config>("date_formats = \"short\"").is_err());
        assert!(toml::from_str::<Config>("date_format = \"nope\"").is_err());
    }

    #[test]
    fn unknown_status_names_are_reported_not_dropped() {
        let config: Config = toml::from_str("status = [\"todo\", \"nonsense\"]").unwrap();
        let (statuses, warning) = config.statuses();
        assert_eq!(statuses.unwrap().len(), 1);
        assert!(warning.unwrap().contains("nonsense"));
    }
}
