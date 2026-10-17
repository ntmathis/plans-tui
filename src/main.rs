//! `plans` — a global plan dashboard across every repo in `~/src`.
//!
//! Read-only by default: the only write path is the optional status edit (`S`
//! or `D` in the TUI), which lives behind a confirm prompt.
//!
//! Everything the TUI can see is reachable non-interactively. The output modes
//! (`--list`, `--json`, `--ndjson`, `--paths`, `--count`, `--handoff`) exist so
//! dashboards and agents can consume the same corpus, and they all take the
//! same filters and shaping flags.
//!
//! Display defaults come from `~/.config/plans/config.toml` when it exists;
//! see [`config`]. Precedence is always CLI flag > config file > built-in.

mod app;
mod clipboard;
mod config;
mod git;
mod journal;
mod plan;
mod repo;
mod scan;
mod ui;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Local;
use clap::{ArgGroup, Parser, ValueEnum};
use serde::Serialize;

use crate::app::{AppInit, DateFormat, Filters, GitFilters, GroupBy, RepoColumn, Sort};
use crate::config::Config;
use crate::plan::{Plan, Status, set_status_in_file};

const EXAMPLES: &str = "\
EXAMPLES:
  plans                                    TUI over ~/src (live work first)
  plans --here                             TUI over just the current repo
  plans --list                             live work, grouped by status
  plansl                                   alias for `plans --list`
  plans --status all --repo notes          one repo, every status, in the TUI
  plans --json | jq '.[].slug'             every plan, machine-readable
  plans --json --status wip --since 2026-09-01
  plans --ndjson --limit 20 | jq -c .      stream, one object per line
  plans --paths --status uat | xargs wc -l
  plans --count --status uat               just the number, for a badge
  plans --handoff 2026-09-14-widgetbox-hud-menu > prompt.txt
  plans --copy 2026-09-14-widgetbox-hud-menu    same, plus the clipboard
  plans --set-status 2026-09-14-widgetbox-hud-menu done   in-place status edit
  plans --history 10                       the last 10 status writes
  plans --undo                             roll back the most recent batch
  plans --undo 3                           roll back three batches
  plans --stale --status uat --paths       the acceptance-by-silence list
  plans --stale 30 --json | jq '.[].slug'  widen the cutoff for one run
  plans --dupes                            byte-identical copies, canonical first
  plans --canonical-only --count           how many remain after dedupe
  plans --worktrees --status all           include copies from git worktrees
  plans --git --dirty --paths | xargs git add    stage the dirty plan files
  plans --git --untracked --count          how many plan files git does not know
  plans --date-format short                override the config for one run
  plans --scan-report                      what discovery found, and why

CONFIG:
  ~/.config/plans/config.toml (or $PLANS_CONFIG, or --config PATH) sets display
  defaults; any CLI flag wins over it. Keys:
    date_format = \"iso\" | \"short\"          iso 2026-09-14, short 9/14
    group_by    = \"status\"|\"repo\"|\"week\"|\"none\"
    sort        = \"updated\"|\"created\"|\"title\"|\"repo\"
    reverse     = true | false
    status      = [\"todo\", \"wip\", ...]      narrows EVERY mode, --json included
    detail      = true | false             absent = on when the terminal is wide
    mouse       = true | false
    repo_column = \"auto\"|\"always\"|\"never\"  auto hides it when grouped by repo
    strip_slug_date = true | false          drop the YYYY-MM-DD- from displayed filenames
    stale_days  = 14                        days before a TODO/WIP/UAT is stale; 0 hides the marker
    hide_duplicates = true | false          start the TUI with duplicate copies hidden (x toggles)
    hide_worktree_dupes = true | false      hide worktree copies of a main-checkout plan (default true)
    git         = true | false              consult git for per-plan clean/modified/untracked
    roots       = [\"~/src\"]                 scan roots, ~ allowed
    exclude     = [\"docs/adr\"]              skip paths containing this substring

JSON FIELDS:
  path, repo_root, repo_name, rel_path, slug, title,
  status (TODO/WIP/UAT/DONE/DROPPED/UNKNOWN/other label), status_key
  (todo/wip/uat/done/dropped/unknown/other, stable for matching), status_raw
  (exactly as written, e.g. \"#UAT\" or \"ready\"; for an inferred status, the
  body **Status:** text),
  status_inferred (true when the status came from a body **Status:** line or a
  TODO-/WIP-/UAT-/DONE-/DROPPED- filename prefix, not frontmatter),
  date_created, date_updated (YYYY-MM-DD), age_days (days since date_updated,
  null when there is no date), content_hash (BLAKE3 hex), duplicate_of (the
  canonical copy's path, null for the canonical copy; a worktree copy points
  at its main-checkout twin even when the bytes differ), worktree (true when
  the file lives in a linked git worktree checkout), git_status
  (clean/modified/untracked/unknown, null when git was not consulted), tags,
  owner, has_handoff, has_frontmatter

MODES (pick at most one): --list, --json, --ndjson, --paths, --count,
--handoff, --copy, --dupes, --history, --undo, --scan-report.  With none of
them you get the TUI.

STATUS DEFAULTS: machine modes dump everything unless you pass --status or the
config sets one; --list shows live work (TODO/WIP/UAT); the TUI shows live work
plus UNKNOWN, which is most of the legacy corpus. Every mode but --dupes drops
worktree copies of a main-checkout plan unless --worktrees is passed or the
config sets hide_worktree_dupes = false.";

#[derive(Parser, Debug)]
#[command(
    name = "plans",
    version,
    about = "Global plan dashboard across every plans/ directory under ~/src",
    after_help = EXAMPLES,
    group(ArgGroup::new("mode").args([
        "list",
        "json",
        "ndjson",
        "paths",
        "count",
        "handoff",
        "copy",
        "set_status",
        "dupes",
        "history",
        "undo",
        "scan_report",
    ]))
)]
struct Cli {
    /// Directory to scan: a repo, or any ancestor of its plans dirs.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Scan only the current repo.
    #[arg(long, conflicts_with = "path")]
    here: bool,

    /// Plain grouped text dump instead of the TUI.
    #[arg(long)]
    list: bool,

    /// JSON array of every matching plan.
    #[arg(long)]
    json: bool,

    /// Newline-delimited JSON, one plan per line.
    #[arg(long)]
    ndjson: bool,

    /// Absolute paths only, one per line.
    #[arg(long)]
    paths: bool,

    /// Print how many plans match and nothing else.
    #[arg(long)]
    count: bool,

    /// Print a discovery report: counts per match tier, the plans that only the
    /// broader rules found, and what `exclude` dropped. Exits without the TUI.
    #[arg(long)]
    scan_report: bool,

    /// List byte-identical duplicate groups, each canonical path first and its
    /// copies indented. Applies --status/--repo filters first.
    #[arg(long)]
    dupes: bool,

    /// Print one plan's handoff prompt (slug or path). Never touches the
    /// clipboard, so it is safe to run from an agent.
    #[arg(long, value_name = "SLUG|PATH")]
    handoff: Option<String>,

    /// Like --handoff, but also copies it to the macOS clipboard.
    #[arg(long, value_name = "SLUG|PATH")]
    copy: Option<String>,

    /// Set one plan's status in place. Rewrites only its `status:` and
    /// `dateUpdated:` lines; refuses a plan with no frontmatter. Statuses:
    /// todo, wip, uat, done, dropped.
    #[arg(long, num_args = 2, value_names = ["SLUG|PATH", "STATUS"])]
    set_status: Option<Vec<String>>,

    /// Print the last N status-journal entries (default 20).
    #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "20")]
    history: Option<usize>,

    /// Undo the N most recent status batches (default 1), restoring each
    /// plan's previous status and `dateUpdated` from the journal.
    #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "1")]
    undo: Option<usize>,

    /// Comma-separated statuses: todo,wip,uat,done,dropped,unknown,all.
    #[arg(long, value_name = "STATUSES")]
    status: Option<String>,

    /// Keep plans whose repo name contains this string.
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,

    /// Keep plans updated on or after this date (YYYY-MM-DD).
    #[arg(long, value_name = "DATE")]
    since: Option<String>,

    /// Keep only plans that have a `## Handoff Prompt` block.
    #[arg(long)]
    has_handoff: bool,

    /// Drop the non-canonical copies of byte-identical plans.
    #[arg(long)]
    canonical_only: bool,

    /// Include plans that are worktree copies of a plan in the main checkout.
    /// Hidden by default; the config's `hide_worktree_dupes` can flip that.
    #[arg(long = "worktrees", conflicts_with = "no_worktrees")]
    show_worktrees: bool,

    /// Hide worktree copies even when the config has `hide_worktree_dupes =
    /// false`.
    #[arg(long)]
    no_worktrees: bool,

    /// Consult git and mark each plan clean, modified or untracked. Off by
    /// default because it spawns one process per repo.
    #[arg(long)]
    git: bool,

    /// Keep only plans whose file is modified or untracked (implies --git).
    #[arg(long)]
    dirty: bool,

    /// Keep only untracked plans (implies --git).
    #[arg(long)]
    untracked: bool,

    /// Keep only clean plans (implies --git).
    #[arg(long)]
    clean: bool,

    /// Keep only stale plans: TODO/WIP/UAT whose dateUpdated is at least DAYS
    /// days ago (default: the `stale_days` config key, else 14). With no
    /// status flag this replaces the usual status set; `--status all` widens
    /// it to every status. `--stale 0` keeps everything.
    #[arg(long, value_name = "DAYS", num_args = 0..=1)]
    stale: Option<Option<i64>>,

    /// Order by updated, created, title or repo.
    #[arg(long, value_enum, value_name = "FIELD")]
    sort: Option<SortArg>,

    /// Reverse the sort order.
    #[arg(long, overrides_with = "no_reverse")]
    reverse: bool,

    /// Force ascending order, overriding a config `reverse = true`.
    #[arg(long, hide = true, overrides_with = "reverse")]
    no_reverse: bool,

    /// Keep only the first N matches (after sorting).
    #[arg(long, value_name = "N")]
    limit: Option<usize>,

    /// Group --list output by status, repo, week or none.
    #[arg(long, value_enum, value_name = "BY")]
    group_by: Option<GroupArg>,

    /// Dates in the list: iso (2026-09-14) or short (9/14).
    #[arg(long, value_enum, value_name = "FORMAT")]
    date_format: Option<DateFormatArg>,

    /// When to show the repo column: auto hides it when grouped by repo.
    #[arg(long, value_enum, value_name = "WHEN")]
    repo_column: Option<RepoColumnArg>,

    /// Strip a leading `YYYY-MM-DD-` from displayed plan filenames.
    #[arg(long, overrides_with = "no_strip_slug_date")]
    strip_slug_date: bool,

    /// Keep the date prefix on displayed plan filenames.
    #[arg(long, overrides_with = "strip_slug_date")]
    no_strip_slug_date: bool,

    /// Force the detail pane open.
    #[arg(long, overrides_with = "no_detail")]
    detail: bool,

    /// Force the detail pane closed.
    #[arg(long, overrides_with = "detail")]
    no_detail: bool,

    /// Force mouse reporting on, overriding the config.
    #[arg(long, overrides_with = "no_mouse")]
    mouse: bool,

    /// Don't enable mouse reporting; keeps the terminal's own text selection.
    #[arg(long, overrides_with = "mouse")]
    no_mouse: bool,

    /// Suppress the `(hiding …)` footer on --list.
    #[arg(long)]
    no_footer: bool,

    /// Config file to read instead of ~/.config/plans/config.toml.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SortArg {
    Updated,
    Created,
    Title,
    Repo,
}

impl From<SortArg> for Sort {
    fn from(value: SortArg) -> Self {
        match value {
            SortArg::Updated => Sort::Updated,
            SortArg::Created => Sort::Created,
            SortArg::Title => Sort::Title,
            SortArg::Repo => Sort::Repo,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum GroupArg {
    Status,
    Repo,
    Week,
    None,
}

impl From<GroupArg> for GroupBy {
    fn from(value: GroupArg) -> Self {
        match value {
            GroupArg::Status => GroupBy::Status,
            GroupArg::Repo => GroupBy::Repo,
            GroupArg::Week => GroupBy::Date,
            GroupArg::None => GroupBy::None,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DateFormatArg {
    Iso,
    Short,
}

impl From<DateFormatArg> for DateFormat {
    fn from(value: DateFormatArg) -> Self {
        match value {
            DateFormatArg::Iso => DateFormat::Iso,
            DateFormatArg::Short => DateFormat::Short,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RepoColumnArg {
    Auto,
    Always,
    Never,
}

impl From<RepoColumnArg> for RepoColumn {
    fn from(value: RepoColumnArg) -> Self {
        match value {
            RepoColumnArg::Auto => RepoColumn::Auto,
            RepoColumnArg::Always => RepoColumn::Always,
            RepoColumnArg::Never => RepoColumn::Never,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let loaded = config::load(cli.config.as_deref())?;
    for warning in &loaded.warnings {
        eprintln!("plans: {warning}");
    }
    let config = loaded.config;

    // Journal commands never touch the corpus, so they run before the scan.
    if let Some(count) = cli.history {
        return print_history(count);
    }
    if let Some(count) = cli.undo {
        return undo_command(count);
    }

    let roots = resolve_roots(&cli, &config);
    // The rescan path in the TUI has nowhere to thread this through, so it
    // lives in `scan` and every scan (initial and rescan) reads it.
    scan::set_exclude(config.exclude.clone().unwrap_or_default());
    if cli.scan_report {
        return print_scan_report(&roots);
    }
    let all = scan::find_plans(&roots);

    // Git is consulted when asked for, or whenever a git filter needs it.
    let git_filters = GitFilters {
        dirty: cli.dirty,
        untracked: cli.untracked,
        clean: cli.clean,
    };
    let git_index = (cli.git || config.git.unwrap_or(false) || git_filters.active())
        .then(|| git::GitIndex::build(&all));

    // Handoff modes take a selector and search the whole corpus, so they are
    // deliberately not affected by the filters.
    if let Some(selector) = cli.handoff.as_deref() {
        return print_handoff(&all, selector);
    }
    if let Some(selector) = cli.copy.as_deref() {
        return copy_handoff(&all, selector);
    }
    // Also selector-based, and it writes: it must see the whole corpus rather
    // than the current filter, and it must run before any mode output.
    if let Some(values) = cli.set_status.as_deref() {
        return set_status_command(&all, values);
    }

    let filters = filters_for(&cli, &config);
    let sort = cli
        .sort
        .map(Sort::from)
        .or(config.sort)
        .unwrap_or(Sort::Updated);
    let reverse = resolve_reverse(&cli, &config);
    let strip_slug_date = resolve_strip_slug_date(&cli, &config);
    let group_by = cli
        .group_by
        .map(GroupBy::from)
        .or(config.group_by)
        .unwrap_or(GroupBy::Status);

    let mut selected: Vec<&Plan> = all.iter().filter(|plan| filters.matches(plan)).collect();
    if git_filters.active()
        && let Some(index) = &git_index
    {
        selected.retain(|plan| git_filters.matches(index.status(plan)));
    }
    app::sort_indices_refs(&mut selected, sort, !reverse);
    if let Some(limit) = cli.limit {
        selected.truncate(limit);
    }

    if cli.json {
        return print_json(&selected, git_index.as_ref());
    }
    if cli.ndjson {
        return print_ndjson(&selected, git_index.as_ref());
    }
    if cli.paths {
        return print_paths(&selected);
    }
    if cli.count {
        return print_count(&selected);
    }
    if cli.dupes {
        return print_dupes(&selected);
    }
    if cli.list {
        return print_list(&all, &selected, &filters, &cli, group_by, strip_slug_date);
    }

    // TUI: hand it the whole corpus plus the starting view, so `a` can widen
    // the filter and `R` can rescan.
    ui::run(
        AppInit::new(all, filters, roots)
            .sort(sort, !reverse)
            .group_by(group_by)
            .date_format(
                cli.date_format
                    .map(DateFormat::from)
                    .or(config.date_format)
                    .unwrap_or_default(),
            )
            .repo_column(
                cli.repo_column
                    .map(RepoColumn::from)
                    .or(config.repo_column)
                    .unwrap_or_default(),
            )
            .strip_slug_date(strip_slug_date)
            .detail(resolve_detail(&cli, &config))
            .config_path(loaded.path.clone())
            .stale_days(resolve_stale_marker(&cli, &config))
            .hide_duplicates(cli.canonical_only || config.hide_duplicates.unwrap_or(false))
            .hide_worktree_copies(resolve_hide_worktree_dupes(&cli, &config))
            .git(git_index)
            .git_filters(git_filters)
            .mouse(resolve_mouse(&cli, &config)),
    )
}

/// `--status` wins, then the config's `status`, then a per-mode default: the
/// machine modes dump everything, `--list` shows live work, the TUI includes
/// UNKNOWN. `--stale` supplies its own default (TODO/WIP/UAT) unless a status
/// flag or config key is already in play, so `--stale --status all` really does
/// show every status.
fn filters_for(cli: &Cli, config: &Config) -> Filters {
    let (config_statuses, status_warning) = config.statuses();
    if let Some(warning) = status_warning {
        eprintln!("plans: {warning}");
    }

    let stale_days = resolve_stale_days(cli, config);
    let statuses = match &cli.status {
        Some(spec) => app::parse_statuses(spec).unwrap_or_else(|err| {
            eprintln!("plans: {err}");
            std::process::exit(2);
        }),
        None => match config_statuses {
            Some(list) => Some(list.into_iter().collect()),
            None if stale_days.is_some() => Some(
                [Status::Todo, Status::Wip, Status::Uat]
                    .into_iter()
                    .collect(),
            ),
            None if cli.list => Filters::live_for_list().statuses,
            None if cli.json || cli.ndjson || cli.paths || cli.count || cli.dupes => None,
            None => Filters::live().statuses,
        },
    };

    let since = cli
        .since
        .as_deref()
        .map(parse_date)
        .transpose()
        .unwrap_or_else(|err| {
            eprintln!("plans: {err}");
            std::process::exit(2);
        });

    Filters {
        statuses,
        repo: cli.repo.clone(),
        since,
        has_handoff: cli.has_handoff,
        stale_days,
        canonical_only: cli.canonical_only,
        hide_worktree_copies: resolve_hide_worktree_dupes(cli, config),
    }
}

/// The `--stale` age cutoff, or `None` when the flag was not given. A bare
/// `--stale` uses the config key, then 14. A value of `0` disables the filter.
fn resolve_stale_days(cli: &Cli, config: &Config) -> Option<i64> {
    match cli.stale {
        None => None,
        Some(None) => Some(config.stale_days.unwrap_or(14).max(0)),
        Some(Some(days)) => Some(days.max(0)),
    }
}

/// The TUI's red-marker threshold. `--stale DAYS` overrides it for the run;
/// otherwise the config key, then 14. `0` disables the marker.
fn resolve_stale_marker(cli: &Cli, config: &Config) -> i64 {
    match cli.stale {
        Some(Some(days)) => days.max(0),
        _ => config.stale_days.unwrap_or(14).max(0),
    }
}

fn resolve_roots(cli: &Cli, config: &Config) -> Vec<PathBuf> {
    if cli.here {
        return vec![repo::git_root_or_cwd()];
    }
    if let Some(path) = &cli.path {
        return vec![path.clone()];
    }
    let configured = config.roots();
    if !configured.is_empty() {
        return configured;
    }
    vec![scan::default_root()]
}

fn resolve_reverse(cli: &Cli, config: &Config) -> bool {
    if cli.no_reverse {
        false
    } else if cli.reverse {
        true
    } else {
        config.reverse.unwrap_or(false)
    }
}

/// Displayed slugs keep their date prefix unless the config or a flag strips it.
fn resolve_strip_slug_date(cli: &Cli, config: &Config) -> bool {
    if cli.no_strip_slug_date {
        false
    } else if cli.strip_slug_date {
        true
    } else {
        config.strip_slug_date.unwrap_or(false)
    }
}

/// Mouse reporting is on unless the config or a flag says otherwise.
fn resolve_mouse(cli: &Cli, config: &Config) -> bool {
    if cli.mouse {
        true
    } else if cli.no_mouse {
        false
    } else {
        config.mouse.unwrap_or(true)
    }
}

/// Worktree copies are hidden unless `--worktrees` or the config says show.
/// `--no-worktrees` beats the config. `--dupes` shows them: listing copies is
/// its whole job.
fn resolve_hide_worktree_dupes(cli: &Cli, config: &Config) -> bool {
    if cli.show_worktrees {
        false
    } else if cli.no_worktrees {
        true
    } else if cli.dupes {
        false
    } else {
        config.hide_worktree_dupes.unwrap_or(true)
    }
}

/// `None` means "let the width decide".
fn resolve_detail(cli: &Cli, config: &Config) -> Option<bool> {
    if cli.detail {
        Some(true)
    } else if cli.no_detail {
        Some(false)
    } else {
        config.detail
    }
}

#[derive(Serialize)]
struct PlanJson<'a> {
    path: String,
    repo_root: String,
    repo_name: &'a str,
    rel_path: &'a str,
    slug: &'a str,
    title: &'a str,
    /// Human label; one-off values keep their own name (`READY`, `AUDIT`).
    status: &'a str,
    /// Stable machine key (`todo`, `wip`, `uat`, `done`, `dropped`,
    /// `unknown`, `other`).
    status_key: &'static str,
    /// Exactly what the frontmatter said, e.g. `#UAT`, or the body
    /// `**Status:**` text when the status was inferred.
    status_raw: Option<&'a str>,
    /// `true` when the status came from a body `**Status:**` line or a
    /// filename prefix instead of frontmatter.
    status_inferred: bool,
    date_created: Option<String>,
    date_updated: Option<String>,
    /// Days since `date_updated`, `null` when the plan has no date at all.
    age_days: Option<i64>,
    /// BLAKE3 of the file bytes, hex. Identical files share it.
    content_hash: &'a str,
    /// The canonical copy's absolute path, or `null` when this *is* the
    /// canonical copy (or the file has no twin). A worktree copy points at its
    /// main-checkout twin even when the bytes differ.
    duplicate_of: Option<String>,
    /// `true` when the file lives in a linked git worktree checkout rather
    /// than the main checkout.
    worktree: bool,
    /// `clean`, `modified`, `untracked` or `unknown`, or `null` when git was
    /// never consulted.
    git_status: Option<&'static str>,
    tags: &'a [String],
    owner: Option<&'a str>,
    has_handoff: bool,
    has_frontmatter: bool,
}

fn json_rows<'a>(plans: &[&'a Plan], git: Option<&git::GitIndex>) -> Vec<PlanJson<'a>> {
    let today = Local::now().date_naive();
    plans
        .iter()
        .map(|plan| PlanJson {
            path: plan.path.to_string_lossy().into_owned(),
            repo_root: plan.repo_root.to_string_lossy().into_owned(),
            repo_name: &plan.repo_name,
            rel_path: &plan.rel_path,
            slug: &plan.slug,
            title: &plan.title,
            status: plan.status.label(),
            status_key: plan.status.key(),
            status_raw: plan.status_raw.as_deref(),
            status_inferred: plan.status_inferred,
            date_created: plan.date_created.map(|d| d.format("%Y-%m-%d").to_string()),
            date_updated: plan.date_updated.map(|d| d.format("%Y-%m-%d").to_string()),
            age_days: plan.age_days(today),
            content_hash: &plan.content_hash,
            duplicate_of: plan
                .duplicate_of
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            worktree: plan.in_worktree,
            git_status: git.map(|index| index.status(plan).label()),
            tags: &plan.tags,
            owner: plan.owner.as_deref(),
            has_handoff: plan.has_handoff,
            has_frontmatter: plan.has_frontmatter,
        })
        .collect()
}

fn print_json(plans: &[&Plan], git: Option<&git::GitIndex>) -> Result<()> {
    emit(&format!(
        "{}\n",
        serde_json::to_string_pretty(&json_rows(plans, git))?
    ));
    Ok(())
}

fn print_ndjson(plans: &[&Plan], git: Option<&git::GitIndex>) -> Result<()> {
    let mut out = String::new();
    for row in json_rows(plans, git) {
        out.push_str(&serde_json::to_string(&row)?);
        out.push('\n');
    }
    emit(&out);
    Ok(())
}

fn print_paths(plans: &[&Plan]) -> Result<()> {
    let mut out = String::new();
    for plan in plans {
        out.push_str(&plan.path.to_string_lossy());
        out.push('\n');
    }
    emit(&out);
    Ok(())
}

fn print_count(plans: &[&Plan]) -> Result<()> {
    emit(&format!("{}\n", plans.len()));
    Ok(())
}

/// `--scan-report`: what discovery found and why. The review surface for the
/// broader tiers — run it once, add `exclude` entries for anything that is not
/// actually a plan.
fn print_scan_report(roots: &[PathBuf]) -> Result<()> {
    let started = std::time::Instant::now();
    let scan = scan::find_plans_with_reasons(roots);
    let elapsed = started.elapsed();

    let mut dirs = 0usize;
    let mut dated = 0usize;
    let mut frontmatter = 0usize;
    for plan in &scan.plans {
        match scan.reasons.get(&plan.path) {
            Some(scan::MatchReason::PlansDir) => dirs += 1,
            Some(scan::MatchReason::DatedName) => dated += 1,
            Some(scan::MatchReason::Frontmatter) => frontmatter += 1,
            None => {}
        }
    }

    let noun = if roots.len() == 1 { "root" } else { "roots" };
    let mut out = format!(
        "scanned {} {noun} in {:.2}s — {} plans (dir {dirs}, dated {dated}, frontmatter {frontmatter})\n",
        roots.len(),
        elapsed.as_secs_f64(),
        scan.plans.len(),
    );

    // The whole point of the report: what the old, directory-only rule missed.
    // "Old rule" is the exact-name predicate, so a plan under `docs/planning/`
    // counts as new even though its match reason is the directory tier.
    let mut fresh: Vec<&Plan> = scan
        .plans
        .iter()
        .filter(|plan| !scan::under_exact_plans_dir(&plan.path))
        .collect();
    fresh.sort_by(|a, b| {
        a.repo_name
            .cmp(&b.repo_name)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    out.push_str(&format!(
        "\nnew since directory-only matching ({}):\n",
        fresh.len()
    ));
    let repo_width = fresh
        .iter()
        .map(|plan| plan.repo_name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 32);
    let path_width = fresh
        .iter()
        .map(|plan| plan.rel_path.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 64);
    for plan in &fresh {
        let reason = scan
            .reasons
            .get(&plan.path)
            .map(|reason| reason.label())
            .unwrap_or("?");
        out.push_str(&format!(
            "  {:<repo_width$}  {:<path_width$}  [{reason}]\n",
            truncate(&plan.repo_name, repo_width),
            truncate(&plan.rel_path, path_width),
        ));
    }

    out.push_str(&format!(
        "\nskipped by exclude ({}):\n",
        scan.excluded.len()
    ));
    for path in &scan.excluded {
        out.push_str(&format!("  {}\n", relative_to_roots(path, roots)));
    }

    emit(&out);
    Ok(())
}

/// Shorten to `width` characters, marking the cut with an ellipsis.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Strip the longest scan root prefix, so the report shows repo-relative paths.
fn relative_to_roots(path: &Path, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .filter_map(|root| path.strip_prefix(root).ok())
        .min_by_key(|rel| rel.as_os_str().len())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// `--dupes`: group the filtered plans by content hash and print every group
/// with more than one member, the canonical path first and its copies indented.
/// Byte-identical only — near-duplicates are a different problem.
fn print_dupes(plans: &[&Plan]) -> Result<()> {
    let mut groups: BTreeMap<&str, Vec<&Plan>> = BTreeMap::new();
    for plan in plans {
        groups
            .entry(plan.content_hash.as_str())
            .or_default()
            .push(plan);
    }

    let mut duplicates: Vec<Vec<&Plan>> = groups
        .into_values()
        .filter(|group| group.len() > 1)
        .collect();
    duplicates.sort_by(|a, b| canonical_of(a).path.cmp(&canonical_of(b).path));

    let mut out = String::new();
    for group in &duplicates {
        let canonical = canonical_of(group);
        out.push_str(&format!(
            "{}  ({} copies)\n",
            canonical.path.display(),
            group.len()
        ));
        for plan in group {
            if plan.path != canonical.path {
                out.push_str(&format!("    {}\n", plan.path.display()));
            }
        }
    }
    if duplicates.is_empty() {
        out.push_str("(no duplicate plans)\n");
    }
    emit(&out);
    Ok(())
}

/// The canonical member of a duplicate group, or the first plan when none is
/// marked (e.g. a hand-built corpus in a test).
fn canonical_of<'a>(group: &[&'a Plan]) -> &'a Plan {
    group
        .iter()
        .find(|plan| plan.is_canonical())
        .copied()
        .unwrap_or(group[0])
}

/// Write to stdout without panicking on a closed pipe
/// (`plans --list | head` is a normal thing to do).
fn emit(text: &str) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

fn print_list(
    all: &[Plan],
    selected: &[&Plan],
    filters: &Filters,
    cli: &Cli,
    group_by: GroupBy,
    strip_slug_date: bool,
) -> Result<()> {
    let mut out = String::new();
    let width = selected
        .iter()
        .map(|plan| plan.repo_name.len())
        .max()
        .unwrap_or(0)
        .min(30);

    if group_by == GroupBy::None {
        for plan in selected {
            out.push_str(&format!(
                "  {:width$}  {}\n",
                plan.repo_name,
                display_slug(plan, strip_slug_date)
            ));
        }
    } else {
        let mut groups: Vec<(String, String, Vec<&Plan>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for plan in selected {
            let (key, label) = app::group_for(plan, group_by);
            let slot = *index.entry(key.clone()).or_insert_with(|| {
                groups.push((key.clone(), label, Vec::new()));
                groups.len() - 1
            });
            groups[slot].2.push(plan);
        }
        groups.sort_by(|a, b| match group_by {
            // Newest week first, matching the TUI.
            GroupBy::Date => b.0.cmp(&a.0),
            _ => a.0.cmp(&b.0),
        });

        for (_, label, plans) in &groups {
            out.push_str(&format!("{label} ({})\n", plans.len()));
            for plan in plans {
                out.push_str(&format!(
                    "  {:width$}  {}\n",
                    plan.repo_name,
                    display_slug(plan, strip_slug_date)
                ));
            }
        }
    }

    if selected.is_empty() {
        out.push_str("(no plans match)\n");
    }

    if !cli.no_footer
        && let Some(footer) = hidden_footer(all, filters)
    {
        out.push_str(&footer);
        out.push('\n');
    }
    emit(&out);
    Ok(())
}

/// The slug to print in `--list`, with or without its date prefix.
fn display_slug(plan: &Plan, strip_slug_date: bool) -> &str {
    if strip_slug_date {
        plan.slug_without_date()
    } else {
        plan.slug.as_str()
    }
}

/// `(hiding 174 DONE / 3 DROPPED — pass --status all)`.
fn hidden_footer(all: &[Plan], filters: &Filters) -> Option<String> {
    let statuses = filters.statuses.as_ref()?;
    let mut counts: BTreeMap<u8, (String, usize)> = BTreeMap::new();
    for plan in all {
        if statuses.contains(&plan.status) {
            continue;
        }
        let entry = counts
            .entry(plan.status.rank())
            .or_insert_with(|| (plan.status.label().to_string(), 0));
        entry.1 += 1;
    }
    if counts.is_empty() {
        return None;
    }
    let parts: Vec<String> = counts
        .values()
        .map(|(label, n)| format!("{n} {label}"))
        .collect();
    Some(format!(
        "(hiding {} — pass --status all)",
        parts.join(" / ")
    ))
}

/// The prompt `--handoff` and `--copy` share: the plan's own block, or a
/// synthesized one when it never wrote any.
fn prompt_for(all: &[Plan], selector: &str) -> Result<(String, bool)> {
    let plan = find_plan(all, selector)?;
    match plan.handoff_prompt() {
        Some(prompt) => Ok((prompt, false)),
        None => Ok((plan.synthesized_handoff(), true)),
    }
}

fn print_handoff(all: &[Plan], selector: &str) -> Result<()> {
    let (prompt, synthesized) = prompt_for(all, selector)?;
    let mut out = prompt;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    emit(&out);
    if synthesized {
        eprintln!("plans: no Handoff Prompt in {selector} — synthesized one (nothing copied)");
    }
    Ok(())
}

fn copy_handoff(all: &[Plan], selector: &str) -> Result<()> {
    let (prompt, synthesized) = prompt_for(all, selector)?;
    let mut out = prompt.clone();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    emit(&out);
    if synthesized {
        eprintln!("plans: no Handoff Prompt in {selector} — synthesized one");
    }
    if !clipboard::copy(&prompt) {
        eprintln!("plans: pbcopy unavailable; prompt printed to stdout only");
    }
    Ok(())
}

/// The five statuses a user may set, and nothing else. UNKNOWN is a scanner
/// verdict for a plan with no frontmatter, not something to write by hand.
fn settable_status(name: &str) -> Result<Status> {
    match Status::named(name) {
        Some(status) if !matches!(status, Status::Unknown) => Ok(status),
        _ => bail!("unknown status `{name}`; use todo, wip, uat, done or dropped"),
    }
}

/// `--set-status <SLUG|PATH> <STATUS>`: the one command that writes. The splice
/// itself is `plan::set_status_in_file`, the same path the TUI's `S`/`D` use, so
/// the two front ends cannot disagree about what gets rewritten.
fn set_status_command(all: &[Plan], values: &[String]) -> Result<()> {
    let [selector, status_name] = values else {
        bail!("--set-status needs a plan (slug or path) and a status");
    };
    let target = settable_status(status_name)?;
    let plan = find_plan(all, selector)?;
    if plan.status == target {
        emit(&format!(
            "{} is already {}\n",
            plan.rel_path,
            target.label()
        ));
        return Ok(());
    }
    let from = plan.status.label().to_string();
    let today = Local::now().date_naive();
    // Journal first (best-effort), then write: the entry has to describe the
    // file *before* the edit.
    let entry = journal::Entry::from_write(&journal::new_batch(), plan, &target, today, "cli");
    set_status_in_file(&plan.path, &target, today)?;
    journal_note(&entry);
    emit(&format!(
        "{}: {} -> {}\n",
        plan.rel_path,
        from,
        target.label()
    ));
    Ok(())
}

/// Append one entry to the journal, warning on failure. Bookkeeping must never
/// fail the edit that already happened.
fn journal_note(entry: &journal::Entry) {
    let Some(path) = journal::default_path() else {
        return;
    };
    if let Err(err) = journal::append(&path, std::slice::from_ref(entry)) {
        eprintln!("plans: status written but journal not updated: {err}");
    }
}

/// `--history [N]`: the last N journal entries, oldest first.
fn print_history(count: usize) -> Result<()> {
    let Some(path) = journal::default_path() else {
        bail!("no home directory to find the status journal");
    };
    let entries = journal::read_all(&path)?;
    let mut out = String::new();
    if entries.is_empty() {
        out.push_str("(status journal is empty)\n");
    } else {
        let start = entries.len().saturating_sub(count.max(1));
        for entry in &entries[start..] {
            out.push_str(&format!(
                "{}  {:<4} {} → {}  {}\n",
                friendly_time(&entry.at),
                entry.source,
                entry.from_status,
                entry.to_status,
                entry.rel_path
            ));
        }
    }
    emit(&out);
    Ok(())
}

/// `--undo [N]`: restore the previous status and date for the N most recent
/// batches.
fn undo_command(count: usize) -> Result<()> {
    let Some(path) = journal::default_path() else {
        bail!("no home directory to find the status journal");
    };
    let report = journal::revert_batches(&path, count.max(1))?;
    let mut out = if report.files == 0 && report.errors.is_empty() {
        "nothing to undo\n".to_string()
    } else {
        format!(
            "undid {} status change(s) across {} batch(es)\n",
            report.files, report.batches
        )
    };
    for error in &report.errors {
        out.push_str(&format!("  failed: {error}\n"));
    }
    emit(&out);
    Ok(())
}

/// RFC3339 to `2026-09-15 12:30`; the raw string if it cannot be parsed.
fn friendly_time(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| raw.to_string())
}

/// Resolve a slug, a repo-relative path, an absolute path or a trailing path
/// fragment to exactly one plan.
fn find_plan<'a>(all: &'a [Plan], selector: &str) -> Result<&'a Plan> {
    let matches: Vec<&Plan> = all
        .iter()
        .filter(|plan| {
            plan.slug == selector
                || plan.rel_path == selector
                || plan.path.to_string_lossy() == selector
                || plan.path.to_string_lossy().ends_with(selector)
        })
        .collect();

    match matches.len() {
        0 => bail!("no plan matches `{selector}`"),
        1 => Ok(matches[0]),
        _ => {
            let list = matches
                .iter()
                .map(|p| format!("  {} ({})", p.rel_path, p.repo_name))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("`{selector}` matches {} plans:\n{list}", matches.len())
        }
    }
}

fn parse_date(raw: &str) -> Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .with_context(|| format!("expected a YYYY-MM-DD date, got `{raw}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn stale_is_optional_valued() {
        let absent = Cli::try_parse_from(["plans"]).unwrap();
        assert_eq!(absent.stale, None);

        let bare = Cli::try_parse_from(["plans", "--stale"]).unwrap();
        assert_eq!(bare.stale, Some(None));

        let valued = Cli::try_parse_from(["plans", "--stale", "30"]).unwrap();
        assert_eq!(valued.stale, Some(Some(30)));
    }

    #[test]
    fn settable_status_accepts_only_the_five_real_states() {
        for (name, expected) in [
            ("todo", Status::Todo),
            ("WIP", Status::Wip),
            ("#UAT", Status::Uat),
            ("done", Status::Done),
            ("dropped", Status::Dropped),
        ] {
            assert_eq!(settable_status(name).unwrap(), expected);
        }
        // UNKNOWN is a scanner verdict for a plan with no frontmatter, and ALL
        // is a filter keyword: neither is a status to write.
        assert!(settable_status("unknown").is_err());
        assert!(settable_status("all").is_err());
        assert!(settable_status("ready").is_err());
    }
}
