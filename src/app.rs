//! App state: rows, grouping, collapse, filter, selection.
//!
//! `plans` is immutable after the scan; `rows` is a derived view rebuilt on
//! every keystroke. 765 items is cheap enough that caching would only add
//! staleness bugs.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, NaiveDate, Weekday};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::layout::Rect;
use serde::Deserialize;

use crate::git::{GitIndex, GitStatus};
use crate::plan::{Plan, Status};

/// How long a toast stays on screen.
pub const TOAST_TTL: Duration = Duration::from_secs(2);

/// Non-interactive prefilters, shared by `--list`, `--json`, `--copy` and the
/// TUI's starting state.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// `None` means every status is allowed.
    pub statuses: Option<HashSet<Status>>,
    /// Case-insensitive substring match on `repo_name`.
    pub repo: Option<String>,
    /// Keep plans updated on or after this date.
    pub since: Option<NaiveDate>,
    /// Keep only plans that have a `## Handoff Prompt` block.
    pub has_handoff: bool,
    /// `--stale`: keep only plans whose `date_updated` is at least this many
    /// days old. `None` (and `0`) means no age filter. The status restriction
    /// that normally comes with `--stale` lives in the status set instead, so
    /// `--stale --status all` really does show every status.
    pub stale_days: Option<i64>,
    /// `--canonical-only`: drop the non-canonical copies of identical files.
    pub canonical_only: bool,
}

impl Filters {
    pub fn matches(&self, plan: &Plan) -> bool {
        self.matches_on(plan, Local::now().date_naive())
    }

    /// [`Filters::matches`] with the clock passed in, so the arithmetic is
    /// testable without waiting for a date to pass.
    pub fn matches_on(&self, plan: &Plan, today: NaiveDate) -> bool {
        if self.has_handoff && !plan.has_handoff {
            return false;
        }
        if let Some(statuses) = &self.statuses
            && !statuses.contains(&plan.status)
        {
            return false;
        }
        if let Some(repo) = &self.repo
            && !plan
                .repo_name
                .to_ascii_lowercase()
                .contains(&repo.to_ascii_lowercase())
        {
            return false;
        }
        if let Some(since) = self.since {
            let date = plan.date_updated.or(plan.date_created);
            if date.is_none_or(|d| d < since) {
                return false;
            }
        }
        if let Some(days) = self.stale_days
            && days > 0
            && plan.age_days(today).is_none_or(|age| age < days)
        {
            return false;
        }
        if self.canonical_only && plan.duplicate_of.is_some() {
            return false;
        }
        true
    }

    /// Statuses allowed at startup for the TUI: live work, including the
    /// legacy UNKNOWN bucket that is most of the corpus.
    pub fn live() -> Filters {
        Filters {
            statuses: Some(Status::live_set().into_iter().collect()),
            ..Filters::default()
        }
    }

    /// `--list` keeps the old muscle memory: only the statuses that are still
    /// asking for something.
    pub fn live_for_list() -> Filters {
        Filters {
            statuses: Some(
                [Status::Todo, Status::Wip, Status::Uat]
                    .into_iter()
                    .collect(),
            ),
            ..Filters::default()
        }
    }
}

/// Parse a `--status todo,wip` spec. `all` clears the filter entirely.
pub fn parse_statuses(spec: &str) -> anyhow::Result<Option<HashSet<Status>>> {
    let mut set = HashSet::new();
    for token in spec.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if token.eq_ignore_ascii_case("all") {
            return Ok(None);
        }
        let status = Status::named(token).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown status `{token}` (try todo, wip, uat, done, dropped, unknown, all)"
            )
        })?;
        set.insert(status);
    }
    Ok(Some(set))
}

/// `--dirty` / `--untracked` / `--clean`. Only meaningful while git is on: if
/// the user toggled git off, these are ignored rather than hiding everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitFilters {
    pub dirty: bool,
    pub untracked: bool,
    pub clean: bool,
}

impl GitFilters {
    pub fn active(self) -> bool {
        self.dirty || self.untracked || self.clean
    }

    pub fn matches(self, status: GitStatus) -> bool {
        (self.dirty && matches!(status, GitStatus::Modified | GitStatus::Untracked))
            || (self.untracked && status == GitStatus::Untracked)
            || (self.clean && status == GitStatus::Clean)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy {
    Status,
    Repo,
    #[serde(rename = "week")]
    Date,
    None,
}

impl GroupBy {
    pub fn next(self) -> Self {
        match self {
            GroupBy::Status => GroupBy::Repo,
            GroupBy::Repo => GroupBy::Date,
            GroupBy::Date => GroupBy::None,
            GroupBy::None => GroupBy::Status,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GroupBy::Status => "status",
            GroupBy::Repo => "repo",
            GroupBy::Date => "week",
            GroupBy::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    Updated,
    Created,
    Title,
    Repo,
}

impl Sort {
    pub fn next(self) -> Self {
        match self {
            Sort::Updated => Sort::Created,
            Sort::Created => Sort::Title,
            Sort::Title => Sort::Repo,
            Sort::Repo => Sort::Updated,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Updated => "updated",
            Sort::Created => "created",
            Sort::Title => "title",
            Sort::Repo => "repo",
        }
    }
}

/// How the date column is rendered. Machine output (`--json`) always uses ISO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DateFormat {
    /// `2026-09-14`
    #[default]
    Iso,
    /// `9/14`
    Short,
}

impl DateFormat {
    pub fn render(self, date: NaiveDate) -> String {
        match self {
            DateFormat::Iso => date.format("%Y-%m-%d").to_string(),
            DateFormat::Short => format!("{}/{}", date.month(), date.day()),
        }
    }

    /// Widest possible rendering, for the column budget.
    pub fn width(self) -> usize {
        match self {
            DateFormat::Iso => 10,
            DateFormat::Short => 5,
        }
    }
}

/// When the repo column is worth its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoColumn {
    /// Hide it when the list is already grouped by repo.
    #[default]
    Auto,
    Always,
    Never,
}

/// One rendered line in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Group {
        key: String,
        label: String,
        count: usize,
        collapsed: bool,
        /// Set when grouping by status, so the header can take its colour.
        status: Option<Status>,
    },
    /// Index into [`App::plans`].
    Plan(usize),
}

/// An in-progress `S` status change. `targets` is one plan index for the
/// cursor, or every selected plan for a bulk edit. See
/// [`App::confirm_status_edit`].
#[derive(Debug, Clone)]
pub struct StatusEdit {
    pub targets: Vec<usize>,
    pub choice: Status,
    /// Where the modal was opened from: `D`, `S`. Recorded in the journal.
    pub source: &'static str,
}

/// Everything the TUI needs to start. Built by `main` and by the tests.
#[derive(Debug, Clone)]
pub struct AppInit {
    pub plans: Vec<Plan>,
    pub filters: Filters,
    pub roots: Vec<PathBuf>,
    pub sort: Sort,
    pub sort_desc: bool,
    pub group_by: GroupBy,
    pub date_format: DateFormat,
    pub repo_column: RepoColumn,
    /// Strip a `YYYY-MM-DD-` prefix from the displayed slug (display only).
    pub strip_slug_date: bool,
    /// `None` means "decide by terminal width".
    pub detail: Option<bool>,
    /// The config file that was read, shown in the help overlay.
    pub config_path: Option<PathBuf>,
    pub width: u16,
    pub mouse: bool,
    /// Days after which a live plan is marked stale in red; `0` disables it.
    pub stale_days: i64,
    /// What counts as "today", injectable so the arithmetic is testable.
    pub today: NaiveDate,
    /// Start with duplicate copies hidden (the `x` toggle).
    pub hide_duplicates: bool,
    /// Where the undo journal lives; `None` disables journaling (tests).
    pub journal_path: Option<PathBuf>,
    /// Git status, when git was consulted; `None` keeps every row unmarked.
    pub git: Option<GitIndex>,
    /// `--dirty` / `--untracked` / `--clean`.
    pub git_filters: GitFilters,
}

impl AppInit {
    pub fn new(plans: Vec<Plan>, filters: Filters, roots: Vec<PathBuf>) -> Self {
        AppInit {
            plans,
            filters,
            roots,
            // Newest first is the only sensible default for a plan list.
            sort: Sort::Updated,
            sort_desc: true,
            group_by: GroupBy::Status,
            date_format: DateFormat::Iso,
            repo_column: RepoColumn::Auto,
            strip_slug_date: false,
            detail: None,
            config_path: None,
            width: 100,
            mouse: false,
            stale_days: 14,
            today: Local::now().date_naive(),
            hide_duplicates: false,
            journal_path: crate::journal::default_path(),
            git: None,
            git_filters: GitFilters::default(),
        }
    }

    pub fn sort(mut self, sort: Sort, desc: bool) -> Self {
        self.sort = sort;
        self.sort_desc = desc;
        self
    }

    pub fn group_by(mut self, group_by: GroupBy) -> Self {
        self.group_by = group_by;
        self
    }

    pub fn date_format(mut self, date_format: DateFormat) -> Self {
        self.date_format = date_format;
        self
    }

    pub fn repo_column(mut self, repo_column: RepoColumn) -> Self {
        self.repo_column = repo_column;
        self
    }

    pub fn strip_slug_date(mut self, strip_slug_date: bool) -> Self {
        self.strip_slug_date = strip_slug_date;
        self
    }

    pub fn detail(mut self, detail: Option<bool>) -> Self {
        self.detail = detail;
        self
    }

    pub fn config_path(mut self, config_path: Option<PathBuf>) -> Self {
        self.config_path = config_path;
        self
    }

    pub fn width(mut self, width: u16) -> Self {
        self.width = width;
        self
    }

    pub fn mouse(mut self, mouse: bool) -> Self {
        self.mouse = mouse;
        self
    }

    pub fn stale_days(mut self, stale_days: i64) -> Self {
        self.stale_days = stale_days;
        self
    }

    pub fn hide_duplicates(mut self, hide: bool) -> Self {
        self.hide_duplicates = hide;
        self
    }

    /// Override the journal path. Test-only: production uses the default.
    #[cfg(test)]
    pub fn journal_path(mut self, path: Option<PathBuf>) -> Self {
        self.journal_path = path;
        self
    }

    pub fn git(mut self, git: Option<GitIndex>) -> Self {
        self.git = git;
        self
    }

    pub fn git_filters(mut self, git_filters: GitFilters) -> Self {
        self.git_filters = git_filters;
        self
    }

    /// Pin "today" so the age arithmetic can be tested without waiting for a
    /// date to pass.
    #[cfg(test)]
    pub fn today(mut self, today: NaiveDate) -> Self {
        self.today = today;
        self
    }
}

pub struct App {
    /// The full corpus, immutable after a scan.
    pub plans: Vec<Plan>,
    /// Scan roots, so `R` can rescan.
    pub roots: Vec<PathBuf>,
    /// Derived view rebuilt by [`App::rebuild`].
    pub rows: Vec<Row>,

    pub group_by: GroupBy,
    pub collapsed: HashSet<String>,
    /// Empty means "all statuses".
    pub status_filter: HashSet<Status>,
    pub repo_filter: Option<String>,
    pub since: Option<NaiveDate>,
    /// Keep only plans with a `## Handoff Prompt` block.
    pub has_handoff: bool,

    pub date_format: DateFormat,
    pub repo_column: RepoColumn,
    /// Strip a `YYYY-MM-DD-` prefix from the slug column (display only).
    pub strip_slug_date: bool,
    /// Shown in the help overlay so it is obvious which file is in effect.
    pub config_path: Option<PathBuf>,

    /// Fuzzy query. Non-empty suppresses group headers.
    pub query: String,
    /// True while `/` is capturing keystrokes.
    pub filtering: bool,
    pub selected: usize,
    pub sort: Sort,
    pub sort_desc: bool,

    /// Plan indices the user has selected. Indices, not row indices, so that
    /// folding, filtering, sorting and fuzzy search cannot re-point it.
    pub selection: BTreeSet<usize>,
    /// Row index where the current gesture started, used only for range
    /// extension. Re-pointed at the same plan across rebuilds when it can be.
    pub anchor: Option<usize>,
    /// `v`: vim-style select mode. Entering selects the cursor row.
    pub select_mode: bool,

    pub detail_open: bool,
    /// Handoff prompts, loaded lazily and cached per path.
    pub detail_cache: HashMap<PathBuf, Option<String>>,
    /// Rows the detail pane is scrolled down by (mouse wheel / page keys).
    pub detail_scroll: u16,
    /// Logical lines in the current detail content, for clamping the scroll.
    pub detail_lines: usize,
    /// Open status-edit modal, if any. The only thing in the app that writes.
    pub status_edit: Option<StatusEdit>,
    pub toast: Option<(String, Instant)>,
    pub help_open: bool,
    /// Waiting for the second key of a `z`-prefixed chord.
    pub pending_z: bool,

    /// Geometry recorded during the last draw so mouse events can be mapped
    /// back to rows. `list_offset` is the list's scroll offset.
    pub list_area: Rect,
    pub list_offset: usize,
    pub detail_area: Rect,
    /// Whether mouse reporting was enabled for this run.
    pub mouse: bool,
    /// Days after which a visible plan is marked stale in red; `0` disables it.
    pub stale_days: i64,
    /// The `--stale` age cutoff currently applied to visibility, if any.
    pub stale_filter_days: Option<i64>,
    /// The clock the age arithmetic uses, so tests can pin it.
    pub today: NaiveDate,
    /// `x`: hide the non-canonical copies of identical plans.
    pub hide_duplicates: bool,
    /// Where status writes are journaled for undo; `None` skips journaling.
    pub journal_path: Option<PathBuf>,
    /// Git status, when git was consulted; `None` keeps every row unmarked.
    pub git: Option<GitIndex>,
    /// `--dirty` / `--untracked` / `--clean`.
    pub git_filters: GitFilters,
}

impl App {
    pub fn new(init: AppInit) -> App {
        let AppInit {
            plans,
            filters,
            roots,
            sort,
            sort_desc,
            group_by,
            date_format,
            repo_column,
            strip_slug_date,
            detail,
            config_path,
            width,
            mouse,
            stale_days,
            today,
            hide_duplicates,
            journal_path,
            git,
            git_filters,
        } = init;
        let mut app = App {
            plans,
            roots,
            rows: Vec::new(),
            group_by,
            collapsed: HashSet::new(),
            status_filter: filters.statuses.unwrap_or_default(),
            repo_filter: filters.repo,
            since: filters.since,
            has_handoff: filters.has_handoff,
            stale_filter_days: filters.stale_days,
            date_format,
            repo_column,
            strip_slug_date,
            config_path,
            query: String::new(),
            filtering: false,
            selected: 0,
            sort,
            sort_desc,
            selection: BTreeSet::new(),
            anchor: None,
            select_mode: false,
            detail_open: detail.unwrap_or(width >= 120),
            detail_cache: HashMap::new(),
            detail_scroll: 0,
            detail_lines: 0,
            status_edit: None,
            toast: None,
            help_open: false,
            pending_z: false,
            list_area: Rect::new(0, 0, 0, 0),
            list_offset: 0,
            detail_area: Rect::new(0, 0, 0, 0),
            mouse,
            stale_days,
            today,
            hide_duplicates,
            journal_path,
            git,
            git_filters,
        };
        app.rebuild();
        app
    }

    // ---------------------------------------------------------------- derived

    /// Rebuild `rows` from the current filters, query, grouping and sort,
    /// keeping the selection on the same plan when it is still visible.
    pub fn rebuild(&mut self) {
        let previous = self.selected_plan().map(|p| p.path.clone());
        // The anchor is a row index, but it should track a *plan* across a
        // rebuild: folding a group or typing a search must not re-point a
        // half-finished range at a different plan.
        let anchored = self.anchor.and_then(|row| match self.rows.get(row) {
            Some(Row::Plan(index)) => self.plans.get(*index).map(|p| p.path.clone()),
            _ => None,
        });
        let searching = !self.query.trim().is_empty();

        let mut visible: Vec<usize> = (0..self.plans.len())
            .filter(|i| self.is_visible(&self.plans[*i]))
            .collect();

        if searching {
            self.rows = self.fuzzy_rows(&visible);
        } else {
            self.sort_visible(&mut visible);
            self.rows = self.grouped_rows(&visible);
        }

        self.selected = if searching {
            // A search lands on the best match. Chasing the old selection down
            // the score list would leave the viewport scrolled into the noise.
            self.rows
                .iter()
                .position(|row| matches!(row, Row::Plan(_)))
                .unwrap_or(0)
        } else {
            match previous {
                Some(path) => self
                    .rows
                    .iter()
                    .position(|row| matches!(row, Row::Plan(i) if self.plans[*i].path == path))
                    .unwrap_or(self.selected),
                // First paint should land on a plan, not on a group header, so
                // the detail pane has something to show.
                None => self
                    .rows
                    .iter()
                    .position(|row| matches!(row, Row::Plan(_)))
                    .unwrap_or(0),
            }
        };
        self.anchor = match anchored {
            Some(path) => self
                .rows
                .iter()
                .position(|row| matches!(row, Row::Plan(i) if self.plans[*i].path == path)),
            None => self.anchor.filter(|row| *row < self.rows.len()),
        };
        self.clamp_selection();
    }

    fn is_visible(&self, plan: &Plan) -> bool {
        if self.has_handoff && !plan.has_handoff {
            return false;
        }
        if !self.status_filter.is_empty() && !self.status_filter.contains(&plan.status) {
            return false;
        }
        if let Some(repo) = &self.repo_filter
            && !plan
                .repo_name
                .to_ascii_lowercase()
                .contains(&repo.to_ascii_lowercase())
        {
            return false;
        }
        if let Some(since) = self.since {
            let date = plan.date_updated.or(plan.date_created);
            if date.is_none_or(|d| d < since) {
                return false;
            }
        }
        if let Some(days) = self.stale_filter_days
            && days > 0
            && plan.age_days(self.today).is_none_or(|age| age < days)
        {
            return false;
        }
        if self.hide_duplicates && plan.duplicate_of.is_some() {
            return false;
        }
        if self.git_filters.active()
            && let Some(git) = &self.git
            && !self.git_filters.matches(git.status(plan))
        {
            return false;
        }
        true
    }

    fn sort_visible(&self, visible: &mut [usize]) {
        sort_indices(visible, &self.plans, self.sort, self.sort_desc);
    }

    /// Flat, score-ordered results. Grouping while fuzzy-searching is noise.
    fn fuzzy_rows(&self, visible: &[usize]) -> Vec<Row> {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(
            self.query.trim(),
            CaseMatching::Ignore,
            Normalization::Smart,
        );

        let mut buf: Vec<char> = Vec::new();
        let mut scored: Vec<(u32, usize)> = Vec::new();
        for &index in visible {
            let blob = self.plans[index].search_blob();
            let haystack = Utf32Str::new(&blob, &mut buf);
            if let Some(score) = pattern.score(haystack, &mut matcher) {
                scored.push((score, index));
            }
        }
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| self.plans[a.1].slug.cmp(&self.plans[b.1].slug))
        });
        scored
            .into_iter()
            .map(|(_, index)| Row::Plan(index))
            .collect()
    }

    fn grouped_rows(&self, visible: &[usize]) -> Vec<Row> {
        if self.group_by == GroupBy::None {
            return visible.iter().map(|i| Row::Plan(*i)).collect();
        }

        // Preserve first-seen order, then order the groups themselves.
        let mut groups: Vec<(String, String, Vec<usize>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for &i in visible {
            let (key, label) = self.group_of(&self.plans[i]);
            let slot = *index.entry(key.clone()).or_insert_with(|| {
                groups.push((key.clone(), label, Vec::new()));
                groups.len() - 1
            });
            groups[slot].2.push(i);
        }
        groups.sort_by(|a, b| match self.group_by {
            // Newest week first.
            GroupBy::Date => b.0.cmp(&a.0),
            _ => a.0.cmp(&b.0),
        });

        let mut rows = Vec::new();
        for (key, label, members) in groups {
            let collapsed = self.collapsed.contains(&key);
            let status =
                (self.group_by == GroupBy::Status).then(|| self.plans[members[0]].status.clone());
            rows.push(Row::Group {
                key,
                label,
                count: members.len(),
                collapsed,
                status,
            });
            if !collapsed {
                rows.extend(members.into_iter().map(Row::Plan));
            }
        }
        rows
    }

    /// `(sort key, display label)` for the group a plan belongs to.
    fn group_of(&self, plan: &Plan) -> (String, String) {
        group_for(plan, self.group_by)
    }

    // -------------------------------------------------------------- selection

    pub fn selected_plan(&self) -> Option<&Plan> {
        match self.rows.get(self.selected)? {
            Row::Plan(index) => self.plans.get(*index),
            Row::Group { .. } => None,
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        match self.rows.get(self.selected)? {
            Row::Plan(index) => Some(*index),
            Row::Group { .. } => None,
        }
    }

    pub fn selected_plan_path(&self) -> Option<PathBuf> {
        self.selected_plan().map(|p| p.path.clone())
    }

    /// The group under the cursor, whether the cursor is on the header or on
    /// one of its plans.
    pub fn selected_group(&self) -> Option<(usize, String)> {
        match self.rows.get(self.selected)? {
            Row::Group { key, .. } => Some((self.selected, key.clone())),
            Row::Plan(index) => {
                let key = self.group_of(self.plans.get(*index)?).0;
                if key.is_empty() {
                    return None;
                }
                // Walk back to the owning header.
                let at = (0..self.selected)
                    .rev()
                    .find(|i| matches!(&self.rows[*i], Row::Group { key: k, .. } if *k == key))?;
                Some((at, key))
            }
        }
    }

    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(last)
        };
        self.reset_detail_scroll();
    }

    /// Move the cursor to an absolute row, e.g. after a mouse click.
    pub fn select_row(&mut self, index: usize) {
        if index >= self.rows.len() {
            return;
        }
        // Clicking a group header folds it, vim-tree style; clicking a plan
        // just moves the cursor.
        if matches!(self.rows[index], Row::Group { .. }) {
            self.toggle_group_at(index);
            return;
        }
        self.selected = index;
        self.reset_detail_scroll();
    }

    /// Fold or unfold the group header at `index`, keeping the cursor on it.
    pub fn toggle_group_at(&mut self, index: usize) {
        let Some(Row::Group { key, .. }) = self.rows.get(index).cloned() else {
            return;
        };
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key.clone());
        }
        self.rebuild();
        if let Some(at) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Group { key: k, .. } if *k == key))
        {
            self.selected = at;
        }
        self.reset_detail_scroll();
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
        self.reset_detail_scroll();
    }

    pub fn select_last(&mut self) {
        if !self.rows.is_empty() {
            self.selected = self.rows.len() - 1;
        }
        self.reset_detail_scroll();
    }

    pub fn half_page(&mut self, rows: u16, down: bool) {
        let delta = (rows / 2).max(1) as isize;
        self.move_selection(if down { delta } else { -delta });
    }

    // ----------------------------------------------------------- multi-select

    /// Something Esc can clear, or `e`/`O` should warn about.
    pub fn has_selection(&self) -> bool {
        !self.selection.is_empty() || self.select_mode
    }

    pub fn selection_len(&self) -> usize {
        self.selection.len()
    }

    pub fn is_plan_selected(&self, plan_index: usize) -> bool {
        self.selection.contains(&plan_index)
    }

    /// `v`: enter or leave vim-style select mode. Entering puts the anchor on
    /// the cursor row and selects that plan; leaving keeps the selection so an
    /// action can still be fired at it. Esc is what clears.
    pub fn toggle_select_mode(&mut self) {
        if self.select_mode {
            self.select_mode = false;
            return;
        }
        self.select_mode = true;
        self.anchor = Some(self.selected);
        if let Some(index) = self.selected_index() {
            self.selection.insert(index);
        }
        self.toast("select mode: J/K or shift+arrows extend · space toggles · esc clears");
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.anchor = None;
        self.select_mode = false;
    }

    /// `Space`: toggle the plan under the cursor and move the anchor to it.
    pub fn toggle_cursor_plan(&mut self) {
        let Some(index) = self.selected_index() else {
            return;
        };
        self.anchor = Some(self.selected);
        if !self.selection.remove(&index) {
            self.selection.insert(index);
        }
    }

    /// Ctrl/Cmd+click: toggle one plan row. Group headers are ignored rather
    /// than folded, so a stray toggle cannot collapse the list.
    pub fn toggle_plan_at_row(&mut self, row: usize) {
        let Some(Row::Plan(index)) = self.rows.get(row).cloned() else {
            return;
        };
        if !self.selection.remove(&index) {
            self.selection.insert(index);
        }
        self.anchor = Some(row);
        self.selected = row;
        self.select_mode = true;
        self.reset_detail_scroll();
    }

    /// `J`/`K` and Shift+arrows: extend one row from the anchor, starting the
    /// selection at the cursor when there is none yet.
    pub fn extend_selection_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        if self.anchor.is_none() {
            self.anchor = Some(self.selected);
            if let Some(index) = self.selected_index() {
                self.selection.insert(index);
            }
        }
        let target = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(self.rows.len() - 1)
        };
        self.extend_selection_to_row(target);
    }

    /// Shift+click and the range half of J/K: the selection becomes every plan
    /// row between the anchor and `row`, inclusive, skipping group headers.
    pub fn extend_selection_to_row(&mut self, row: usize) {
        if self.rows.is_empty() {
            return;
        }
        let row = row.min(self.rows.len() - 1);
        let anchor = self
            .anchor
            .unwrap_or(self.selected)
            .min(self.rows.len() - 1);
        let (lo, hi) = (anchor.min(row), anchor.max(row));
        let range: Vec<usize> = self.rows[lo..=hi]
            .iter()
            .filter_map(|row| match row {
                Row::Plan(index) => Some(*index),
                Row::Group { .. } => None,
            })
            .collect();
        self.selection = range.into_iter().collect();
        self.selected = row;
        self.select_mode = true;
        self.reset_detail_scroll();
    }

    /// `g`/`G` while selecting: extend to the first or last row.
    pub fn extend_selection_to_edge(&mut self, last: bool) {
        if self.rows.is_empty() {
            return;
        }
        if self.anchor.is_none() {
            self.anchor = Some(self.selected);
            if let Some(index) = self.selected_index() {
                self.selection.insert(index);
            }
        }
        let row = if last { self.rows.len() - 1 } else { 0 };
        self.extend_selection_to_row(row);
    }

    /// Selected plans in the order they appear on screen, with any the current
    /// filter hides appended, so a bulk action never silently drops them.
    pub fn selection_targets(&self) -> Vec<usize> {
        let mut row_position: HashMap<usize, usize> = HashMap::new();
        for (position, row) in self.rows.iter().enumerate() {
            if let Row::Plan(index) = row {
                row_position.insert(*index, position);
            }
        }
        let mut targets: Vec<usize> = self.selection.iter().copied().collect();
        targets.sort_by_key(|index| {
            (
                u8::from(!row_position.contains_key(index)),
                row_position.get(index).copied().unwrap_or(*index),
                *index,
            )
        });
        targets
    }

    /// What an action key acts on: the selection when there is one, otherwise
    /// the plan under the cursor.
    pub fn action_targets(&self) -> Vec<usize> {
        if self.selection.is_empty() {
            self.selected_index().into_iter().collect()
        } else {
            self.selection_targets()
        }
    }

    /// Selected plans the current filter keeps out of `rows`.
    pub fn hidden_selected_count(&self) -> usize {
        self.selection
            .iter()
            .filter(|index| {
                self.plans
                    .get(**index)
                    .is_none_or(|plan| !self.is_visible(plan))
            })
            .count()
    }

    // ----------------------------------------------------------- mouse/detail

    /// Scroll the detail pane by `delta` lines, clamped to its content.
    pub fn scroll_detail(&mut self, delta: isize) {
        let max = self.detail_lines.saturating_sub(1) as isize;
        self.detail_scroll = (self.detail_scroll as isize + delta).clamp(0, max.max(0)) as u16;
    }

    pub fn reset_detail_scroll(&mut self) {
        self.detail_scroll = 0;
    }

    // ------------------------------------------------------------ collapsing

    pub fn set_collapsed(&mut self, collapsed: bool) {
        match self.selected_group() {
            Some((_, key)) => {
                if collapsed {
                    self.collapsed.insert(key);
                } else {
                    self.collapsed.remove(&key);
                }
                self.rebuild();
            }
            None => self.toast("nothing to collapse here"),
        }
    }

    pub fn collapse_all(&mut self) {
        self.collapsed = self
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Group { key, .. } => Some(key.clone()),
                Row::Plan(_) => None,
            })
            .collect();
        self.rebuild();
    }

    pub fn expand_all(&mut self) {
        self.collapsed.clear();
        self.rebuild();
    }

    // ---------------------------------------------------------------- filters

    /// `1`-`5`. Removing the last status falls back to showing everything.
    pub fn toggle_status(&mut self, status: Status) {
        if !self.status_filter.remove(&status) {
            self.status_filter.insert(status);
        }
        self.rebuild();
    }

    pub fn show_all_statuses(&mut self) {
        self.status_filter.clear();
        self.rebuild();
    }

    pub fn status_filter_label(&self) -> String {
        if self.status_filter.is_empty() {
            return "all statuses".to_string();
        }
        let mut statuses: Vec<&Status> = self.status_filter.iter().collect();
        statuses.sort();
        statuses
            .iter()
            .map(|s| s.label())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn cycle_group(&mut self) {
        self.group_by = self.group_by.next();
        self.rebuild();
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.rebuild();
    }

    /// `x`: hide or show the non-canonical copies of identical plans.
    pub fn toggle_duplicates(&mut self) {
        self.hide_duplicates = !self.hide_duplicates;
        self.rebuild();
        self.toast(if self.hide_duplicates {
            "hiding duplicate copies"
        } else {
            "showing duplicate copies"
        });
    }

    /// `g`: consult git for the first time, or stop showing its markers.
    pub fn toggle_git(&mut self) {
        if self.git.is_some() {
            self.git = None;
            self.toast("git status off");
        } else {
            self.git = Some(GitIndex::build(&self.plans));
            self.toast("git status on");
        }
        self.rebuild();
    }

    /// Git's verdict for a plan, or `None` when git was not consulted.
    pub fn git_status(&self, plan: &Plan) -> Option<GitStatus> {
        self.git.as_ref().map(|git| git.status(plan))
    }

    pub fn reverse_sort(&mut self) {
        self.sort_desc = !self.sort_desc;
        self.rebuild();
    }

    // ------------------------------------------------------------------ misc

    pub fn toggle_detail(&mut self) {
        self.detail_open = !self.detail_open;
    }

    pub fn toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
    }

    /// Drop an expired toast; returns true when the screen needs a repaint.
    pub fn expire_toast(&mut self) -> bool {
        match &self.toast {
            Some((_, at)) if at.elapsed() >= TOAST_TTL => {
                self.toast = None;
                true
            }
            _ => false,
        }
    }

    /// The handoff prompt for the selection, read once and cached.
    pub fn handoff_for(&mut self, index: usize) -> Option<String> {
        let plan = self.plans.get(index)?;
        if let Some(cached) = self.detail_cache.get(&plan.path) {
            return cached.clone();
        }
        let prompt = plan.handoff_prompt();
        self.detail_cache.insert(plan.path.clone(), prompt.clone());
        prompt
    }

    pub fn invalidate(&mut self, path: &Path) {
        self.detail_cache.remove(path);
    }

    /// Rescan the roots, replacing the corpus in place.
    pub fn rescan(&mut self) {
        let count = self.reload_silent();
        self.toast(format!("rescanned: {count} plans"));
    }

    /// Re-read the corpus from disk without a toast, keeping the selection
    /// cleared. Returns the new plan count.
    fn reload_silent(&mut self) -> usize {
        self.plans = crate::scan::find_plans(&self.roots);
        self.detail_cache.clear();
        self.clear_selection();
        self.rebuild();
        self.plans.len()
    }

    // ------------------------------------------------------------ status edit

    /// `S`. Opens the modal; refuses outright if there is no frontmatter to
    /// edit, because inventing one would rewrite the author's file shape.
    pub fn begin_status_edit(&mut self) {
        let current = self
            .selected_index()
            .or_else(|| self.selection.iter().next().copied())
            .and_then(|index| self.plans.get(index))
            .map(|plan| plan.status.clone());
        let choice = match current {
            Some(status) if status.is_lifecycle() => status,
            _ => Status::Todo,
        };
        self.open_status_edit(choice, "S");
    }

    /// `D`: open the confirm modal already pointed at DONE, on the whole
    /// selection when there is one. Still one Enter away from writing.
    pub fn begin_quick_done(&mut self) {
        self.open_status_edit(Status::Done, "D");
    }

    /// `D` (via the modal's own menu) and tests. Acts on the whole selection
    /// when there is one.
    #[cfg(test)]
    pub fn begin_status_edit_with(&mut self, choice: Status) {
        self.open_status_edit(choice, "S");
    }

    fn open_status_edit(&mut self, choice: Status, source: &'static str) {
        let targets = self.action_targets();
        if targets.is_empty() {
            self.toast("select a plan first");
            return;
        }
        if !targets
            .iter()
            .any(|index| self.plans[*index].has_frontmatter)
        {
            self.toast("no frontmatter — refusing to invent one");
            return;
        }
        self.status_edit = Some(StatusEdit {
            targets,
            choice,
            source,
        });
    }

    /// Where the highlighted menu row is.
    pub fn status_choice_index(&self) -> usize {
        self.status_edit
            .as_ref()
            .and_then(|edit| Status::lifecycle().iter().position(|s| *s == edit.choice))
            .unwrap_or(0)
    }

    pub fn move_status_choice(&mut self, delta: isize) {
        let list = Status::lifecycle();
        let current = self.status_choice_index();
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            (current + delta as usize).min(list.len() - 1)
        };
        if let Some(edit) = &mut self.status_edit {
            edit.choice = list[next].clone();
        }
    }

    pub fn set_status_choice(&mut self, index: usize) {
        let list = Status::lifecycle();
        if let (Some(edit), Some(status)) = (&mut self.status_edit, list.get(index).cloned()) {
            edit.choice = status;
        }
    }

    pub fn cancel_status_edit(&mut self) {
        self.status_edit = None;
    }

    /// Confirm the modal: splice `status:` and `dateUpdated:` into every
    /// target, re-parse each, and rebuild. Nothing is staged or committed, and
    /// a target without frontmatter is skipped rather than invented.
    pub fn confirm_status_edit(&mut self) {
        let Some(edit) = self.status_edit.take() else {
            return;
        };
        let today = chrono::Local::now().date_naive();
        let single = edit.targets.len() == 1;
        let batch = crate::journal::new_batch();
        let mut entries: Vec<crate::journal::Entry> = Vec::new();
        let mut written = 0usize;
        let mut skipped = 0usize;
        let mut first_error: Option<String> = None;
        let mut only_rel_path = String::new();

        for index in &edit.targets {
            let Some(plan) = self.plans.get(*index) else {
                continue;
            };
            if !plan.has_frontmatter {
                skipped += 1;
                continue;
            }
            let path = plan.path.clone();
            let repo_root = plan.repo_root.clone();
            let repo_name = plan.repo_name.clone();
            let rel_path = plan.rel_path.clone();
            if single {
                only_rel_path = rel_path.clone();
            }
            // The journal entry has to describe the state *before* the write.
            entries.push(crate::journal::Entry::from_write(
                &batch,
                plan,
                &edit.choice,
                today,
                edit.source,
            ));

            let result = crate::plan::set_status_in_file(&path, &edit.choice, today)
                .and_then(|()| Plan::from_path(&path, &repo_root, &repo_name, &rel_path));
            match result {
                Ok(updated) => {
                    self.plans[*index] = updated;
                    self.detail_cache.remove(&path);
                    written += 1;
                }
                Err(err) => {
                    first_error.get_or_insert(format!("{rel_path}: {err}"));
                }
            }
        }

        if written > 0 {
            self.clear_selection();
            self.rebuild();
        }

        let mut message = if single && written == 1 {
            format!(
                "{} → #{}",
                only_rel_path.rsplit('/').next().unwrap_or(&only_rel_path),
                edit.choice.label()
            )
        } else {
            format!("marked {written} plans #{}", edit.choice.label())
        };
        if skipped > 0 {
            message.push_str(&format!(" · {skipped} skipped (no frontmatter)"));
        }
        if let Some(error) = first_error {
            message.push_str(&format!(" · failed on {error}"));
        }
        // Best-effort: a journal that cannot be written must not stop the edit.
        if written > 0
            && let Some(journal) = &self.journal_path
            && let Err(err) = crate::journal::append(journal, &entries)
        {
            message.push_str(&format!(" · journal warning: {err}"));
        }
        self.toast(message);
    }

    /// `u`: restore the previous status and date for the most recent batch.
    /// Reverts are not journaled, so this cannot loop.
    pub fn undo_last_batch(&mut self) {
        let Some(journal) = self.journal_path.clone() else {
            self.toast("no journal path available");
            return;
        };
        match crate::journal::revert_batches(&journal, 1) {
            Err(err) => self.toast(format!("undo failed: {err}")),
            Ok(report) if report.files == 0 && report.errors.is_empty() => {
                self.toast("nothing to undo");
            }
            Ok(report) => {
                self.reload_silent();
                let mut message = format!("undid {} status change(s)", report.files);
                if let Some(error) = report.errors.first() {
                    message.push_str(&format!(" · {error}"));
                }
                self.toast(message);
            }
        }
    }

    // ------------------------------------------------------------------ stats

    pub fn repo_count(&self) -> usize {
        self.plans
            .iter()
            .map(|p| p.repo_name.as_str())
            .collect::<HashSet<_>>()
            .len()
    }

    pub fn status_count(&self, status: &Status) -> usize {
        self.plans.iter().filter(|p| p.status == *status).count()
    }

    /// Is this plan past the stale threshold?
    pub fn is_stale(&self, plan: &Plan) -> bool {
        self.stale_days > 0
            && plan
                .age_days(self.today)
                .is_some_and(|age| age >= self.stale_days)
    }

    /// How many visible rows are stale, for the header chip.
    pub fn visible_stale_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| match row {
                Row::Plan(index) => self
                    .plans
                    .get(*index)
                    .is_some_and(|plan| self.is_stale(plan)),
                Row::Group { .. } => false,
            })
            .count()
    }

    /// How many visible rows git reports as modified or untracked.
    pub fn visible_dirty_count(&self) -> usize {
        let Some(git) = &self.git else {
            return 0;
        };
        self.rows
            .iter()
            .filter(|row| match row {
                Row::Plan(index) => self.plans.get(*index).is_some_and(|plan| {
                    matches!(git.status(plan), GitStatus::Modified | GitStatus::Untracked)
                }),
                Row::Group { .. } => false,
            })
            .count()
    }

    /// Counts for the header summary, in canonical order. UNKNOWN is included
    /// only while it is actually being shown.
    pub fn header_status_counts(&self) -> Vec<(String, usize)> {
        let mut wanted = vec![Status::Todo, Status::Wip, Status::Uat];
        if self.status_filter.is_empty() {
            wanted.extend([Status::Done, Status::Dropped]);
        } else {
            for status in &self.status_filter {
                if !wanted.contains(status) {
                    wanted.push(status.clone());
                }
            }
        }
        if self.status_filter.is_empty() || self.status_filter.contains(&Status::Unknown) {
            wanted.push(Status::Unknown);
        }
        wanted.sort();
        wanted.dedup();

        wanted
            .into_iter()
            .map(|status| {
                let count = self.status_count(&status);
                (status.label().to_string(), count)
            })
            .filter(|(_, count)| *count > 0)
            .collect()
    }
}

// ----------------------------------------------------------------- shared view
//
// The TUI and the non-interactive CLI modes have to agree on ordering and
// grouping, so both go through these.

/// Sort plan indices in place, ascending or descending per `desc`.
pub fn sort_indices(indices: &mut [usize], plans: &[Plan], sort: Sort, desc: bool) {
    indices.sort_by(|&a, &b| {
        let ord = compare_plans(&plans[a], &plans[b], sort);
        if desc { ord.reverse() } else { ord }
    });
}

/// The same ordering for the CLI's `&[&Plan]` views.
pub fn sort_indices_refs(plans: &mut [&Plan], sort: Sort, desc: bool) {
    plans.sort_by(|a, b| {
        let ord = compare_plans(a, b, sort);
        if desc { ord.reverse() } else { ord }
    });
}

fn compare_plans(a: &Plan, b: &Plan, sort: Sort) -> Ordering {
    match sort {
        Sort::Updated => a
            .display_date()
            .cmp(&b.display_date())
            .then_with(|| a.slug.cmp(&b.slug)),
        Sort::Created => a
            .date_created
            .cmp(&b.date_created)
            .then_with(|| a.slug.cmp(&b.slug)),
        Sort::Title => a
            .title
            .to_ascii_lowercase()
            .cmp(&b.title.to_ascii_lowercase()),
        Sort::Repo => a
            .repo_name
            .cmp(&b.repo_name)
            .then_with(|| a.slug.cmp(&b.slug)),
    }
}

/// `(sort key, display label)` for the group a plan belongs to. Status keys
/// encode `rank` so the canonical order falls out of a string sort, and week
/// keys sort chronologically as `YYYY-Www`.
pub fn group_for(plan: &Plan, group_by: GroupBy) -> (String, String) {
    match group_by {
        GroupBy::Status => {
            let label = plan.status.label().to_string();
            let key = format!("status:{:02}:{label}", plan.status.rank());
            (key, label)
        }
        GroupBy::Repo => (format!("repo:{}", plan.repo_name), plan.repo_name.clone()),
        GroupBy::Date => match plan.display_date() {
            Some(date) => {
                let iso = date.iso_week();
                let key = format!("date:{:04}-W{:02}", iso.year(), iso.week());
                let monday = NaiveDate::from_isoywd_opt(iso.year(), iso.week(), Weekday::Mon)
                    .unwrap_or(date);
                let label = format!(
                    "{:04}-W{:02}  {}",
                    iso.year(),
                    iso.week(),
                    monday.format("%b %d")
                );
                (key, label)
            }
            None => ("date:0000-W00".to_string(), "no date".to_string()),
        },
        GroupBy::None => (String::new(), String::new()),
    }
}

/// Map a screen cell to a row index in the list, given its area and scroll
/// offset. `None` when the cell is outside the list.
pub fn row_at(area: Rect, offset: usize, column: u16, row: u16) -> Option<usize> {
    if !contains(area, column, row) {
        return None;
    }
    Some(offset + (row - area.y) as usize)
}

/// Does a screen cell fall inside `area`?
pub fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.x + area.width && row >= area.y && row < area.y + area.height
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(slug: &str, status: Status, repo: &str, date: &str, title: &str) -> Plan {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
        Plan {
            path: PathBuf::from(format!("/tmp/{repo}/{slug}.md")),
            repo_root: PathBuf::from(format!("/tmp/{repo}")),
            repo_name: repo.to_string(),
            rel_path: format!("docs/plans/{slug}.md"),
            slug: slug.to_string(),
            title: title.to_string(),
            status,
            status_raw: None,
            date_created: Some(date),
            date_updated: Some(date),
            tags: Vec::new(),
            owner: None,
            has_handoff: false,
            has_frontmatter: true,
            content_hash: String::new(),
            duplicate_of: None,
        }
    }

    fn corpus() -> Vec<Plan> {
        vec![
            plan(
                "a-widget",
                Status::Wip,
                "dotfiles",
                "2026-09-10",
                "Gotchi box",
            ),
            plan(
                "b-widget",
                Status::Uat,
                "dotfiles",
                "2026-09-12",
                "Gotchi case",
            ),
            plan(
                "c-other",
                Status::Done,
                "scratch",
                "2026-09-01",
                "Unrelated",
            ),
            plan("d-more", Status::Done, "apps/notes", "2026-08-01", "Noise"),
        ]
    }

    fn app() -> App {
        let mut app = App::new(AppInit::new(
            corpus(),
            Filters::default(),
            vec![PathBuf::from("/tmp")],
        ));
        app.rebuild();
        app
    }

    fn plan_row_slugs(app: &App) -> Vec<&str> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                Row::Plan(i) => Some(app.plans[*i].slug.as_str()),
                Row::Group { .. } => None,
            })
            .collect()
    }

    #[test]
    fn status_grouping_emits_headers_in_live_order() {
        let app = app();
        let headers: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Group { label, .. } => Some(label.as_str()),
                Row::Plan(_) => None,
            })
            .collect();
        assert_eq!(headers, vec!["WIP", "UAT", "DONE"]);
    }

    #[test]
    fn collapsing_hides_members_but_keeps_the_header() {
        let mut app = app();
        app.collapsed.insert("status:03:DONE".to_string());
        app.rebuild();
        assert!(plan_row_slugs(&app).contains(&"a-widget"));
        assert!(!plan_row_slugs(&app).contains(&"c-other"));
        assert!(app.rows.iter().any(|row| matches!(
            row,
            Row::Group { label, count: 2, collapsed: true, .. } if label == "DONE"
        )));
    }

    #[test]
    fn fuzzy_search_is_flat_and_ordered_by_score() {
        let mut app = app();
        app.query = "widget".to_string();
        app.rebuild();
        assert!(app.rows.iter().all(|row| matches!(row, Row::Plan(_))));
        let slugs = plan_row_slugs(&app);
        assert_eq!(slugs.len(), 2);
        assert!(slugs.contains(&"a-widget") && slugs.contains(&"b-widget"));

        app.query = "zzzz".to_string();
        app.rebuild();
        assert!(app.rows.is_empty());
    }

    #[test]
    fn query_matches_tags_and_repo_too() {
        let mut app = app();
        app.query = "notes".to_string();
        app.rebuild();
        assert_eq!(plan_row_slugs(&app), vec!["d-more"]);
    }

    #[test]
    fn selection_follows_the_same_plan_across_a_rebuild() {
        let mut app = app();
        app.select_last();
        let path = app.selected_plan_path().unwrap();
        app.cycle_group(); // status -> repo
        assert_eq!(app.selected_plan_path().as_ref(), Some(&path));
    }

    #[test]
    fn searching_lands_on_the_best_match_not_the_old_selection() {
        let mut app = app();
        app.select_last(); // the worst-scoring plan is now selected
        app.query = "widget".to_string();
        app.rebuild();
        assert_eq!(app.selected, 0);
        assert!(app.selected_plan().unwrap().slug.contains("widget"));
    }

    #[test]
    fn status_toggle_selects_then_clears() {
        let mut app = app();
        // From "all", the first press focuses on just that status...
        app.toggle_status(Status::Done);
        assert_eq!(plan_row_slugs(&app), vec!["c-other", "d-more"]);
        // ...and the second press is back to everything.
        app.toggle_status(Status::Done);
        assert_eq!(plan_row_slugs(&app).len(), 4);
    }

    #[test]
    fn show_all_statuses_clears_the_filter() {
        let mut app = App::new(AppInit::new(
            corpus(),
            Filters::live(),
            vec![PathBuf::from("/tmp")],
        ));
        assert_eq!(plan_row_slugs(&app), vec!["a-widget", "b-widget"]);
        app.show_all_statuses();
        assert_eq!(plan_row_slugs(&app).len(), 4);
        assert_eq!(app.status_filter_label(), "all statuses");
    }

    #[test]
    fn sort_reversal_flips_the_order() {
        let mut app = app();
        app.group_by = GroupBy::None;
        app.sort = Sort::Title;
        app.sort_desc = false;
        app.rebuild();
        assert_eq!(
            plan_row_slugs(&app),
            vec!["a-widget", "b-widget", "d-more", "c-other"]
        );
        app.reverse_sort();
        assert_eq!(
            plan_row_slugs(&app),
            vec!["c-other", "d-more", "b-widget", "a-widget"]
        );
    }

    #[test]
    fn selection_clamps_to_the_new_row_count() {
        let mut app = app();
        app.select_last();
        app.query = "widget".to_string();
        app.rebuild();
        assert!(app.selected < app.rows.len());
        assert!(app.selected_plan().is_some());
    }

    #[test]
    fn date_grouping_labels_iso_weeks_newest_first() {
        let mut app = app();
        app.group_by = GroupBy::Date;
        app.rebuild();
        let headers: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Group { key, .. } => Some(key.as_str()),
                Row::Plan(_) => None,
            })
            .collect();
        assert_eq!(headers.first(), Some(&"date:2026-W37"));
        assert!(headers.windows(2).all(|w| w[0] > w[1]));
    }

    #[test]
    fn row_at_maps_screen_cells_to_rows() {
        let area = Rect::new(0, 1, 80, 20);
        assert_eq!(row_at(area, 0, 0, 1), Some(0));
        assert_eq!(row_at(area, 5, 40, 3), Some(7));
        // Above, below and beside the list.
        assert_eq!(row_at(area, 0, 0, 0), None);
        assert_eq!(row_at(area, 0, 0, 21), None);
        assert_eq!(row_at(area, 0, 80, 1), None);
        assert!(contains(area, 0, 1));
        assert!(!contains(area, 0, 21));
    }

    #[test]
    fn clicking_a_group_header_folds_it() {
        let mut app = app();
        assert!(plan_row_slugs(&app).contains(&"c-other"));
        app.select_row(0); // the first header
        assert_eq!(app.collapsed.len(), 1);
        app.select_row(0);
        assert!(app.collapsed.is_empty());
    }

    #[test]
    fn clicking_a_plan_row_selects_it() {
        let mut app = app();
        // Row 1 is the first plan under the first header.
        app.select_row(1);
        assert_eq!(app.selected, 1);
        assert!(app.selected_plan().is_some());
        // Out of range is a no-op.
        app.select_row(9_999);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn quick_done_opens_the_confirm_modal_on_done() {
        let mut app = app();
        app.select_row(1);
        app.begin_status_edit_with(Status::Done);
        let edit = app.status_edit.clone().expect("modal open");
        assert_eq!(edit.choice, Status::Done);
        // Nothing is written until Enter, which the UI owns.
        assert_eq!(app.status_choice_index(), 3);
    }

    #[test]
    fn has_handoff_filter_keeps_only_plans_with_a_prompt() {
        let mut plans = corpus();
        plans[0].has_handoff = true;
        let mut app = App::new(AppInit::new(
            plans,
            Filters {
                has_handoff: true,
                ..Filters::default()
            },
            vec![PathBuf::from("/tmp")],
        ));
        app.rebuild();
        assert_eq!(plan_row_slugs(&app), vec!["a-widget"]);
    }

    #[test]
    fn detail_scroll_clamps_to_the_content() {
        let mut app = app();
        app.detail_lines = 10;
        app.scroll_detail(5);
        assert_eq!(app.detail_scroll, 5);
        app.scroll_detail(100);
        assert_eq!(app.detail_scroll, 9);
        app.scroll_detail(-100);
        assert_eq!(app.detail_scroll, 0);
        // Moving the selection resets it.
        app.detail_scroll = 7;
        app.move_selection(1);
        assert_eq!(app.detail_scroll, 0);
    }

    // ------------------------------------------------------------ multi-select

    #[test]
    fn select_mode_selects_the_cursor_row() {
        let mut app = app();
        app.selected = 1;
        assert!(app.selected_plan().is_some());
        app.toggle_select_mode();
        assert!(app.select_mode);
        assert_eq!(app.selection_len(), 1);
        assert!(app.has_selection());
        // Leaving select mode keeps the selection; only Esc clears it.
        app.toggle_select_mode();
        assert!(!app.select_mode);
        assert_eq!(app.selection_len(), 1);
        app.clear_selection();
        assert!(!app.has_selection());
        assert_eq!(app.selection_len(), 0);
    }

    #[test]
    fn v_then_two_extends_selects_three_plans() {
        let mut app = app();
        // Flat grouping keeps plan rows contiguous, mirroring the acceptance
        // run: v, J, J.
        app.group_by = GroupBy::None;
        app.sort = Sort::Created;
        app.sort_desc = false;
        app.rebuild();
        app.selected = 0;
        app.toggle_select_mode();
        app.extend_selection_by(1);
        app.extend_selection_by(1);
        assert_eq!(app.selection_len(), 3);
        assert_eq!(app.action_targets().len(), 3);
    }

    #[test]
    fn range_extension_skips_group_headers() {
        let mut app = app();
        let plan_count = app
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Plan(_)))
            .count();
        app.selected = 0; // the first group header
        app.extend_selection_to_row(app.rows.len() - 1);
        assert_eq!(app.selection_len(), plan_count);
    }

    #[test]
    fn selection_survives_folding_and_a_search() {
        let mut app = app();
        app.selected = 1;
        app.toggle_select_mode();
        let chosen: Vec<PathBuf> = app
            .selection
            .iter()
            .map(|index| app.plans[*index].path.clone())
            .collect();

        app.collapse_all();
        assert_eq!(
            app.selection_len(),
            1,
            "folding must not drop the selection"
        );

        app.query = "zzzz".to_string();
        app.rebuild();
        assert_eq!(app.selection_len(), 1, "searching must not drop it either");
        app.query.clear();
        app.rebuild();

        let after: Vec<PathBuf> = app
            .selection
            .iter()
            .map(|index| app.plans[*index].path.clone())
            .collect();
        assert_eq!(chosen, after);
    }

    #[test]
    fn hidden_selected_count_tracks_the_filter() {
        let mut app = app();
        app.show_all_statuses();
        let done = app
            .plans
            .iter()
            .position(|plan| plan.status == Status::Done)
            .unwrap();
        app.selection.insert(done);
        assert_eq!(app.hidden_selected_count(), 0);

        app.status_filter = [Status::Wip].into_iter().collect();
        app.rebuild();
        assert_eq!(app.hidden_selected_count(), 1);
    }

    #[test]
    fn action_targets_follow_the_selection_not_the_cursor() {
        let mut app = app();
        app.group_by = GroupBy::None;
        app.rebuild();
        app.selected = 0;
        app.toggle_select_mode();
        app.extend_selection_by(2);
        let targets = app.action_targets();
        assert_eq!(targets.len(), 3);
        // Visible targets come back in row order, not numeric plan order.
        let row_order: Vec<usize> = app
            .rows
            .iter()
            .take(3)
            .filter_map(|row| match row {
                Row::Plan(index) => Some(*index),
                Row::Group { .. } => None,
            })
            .collect();
        assert_eq!(targets, row_order);
        // With nothing selected, the action falls back to the cursor plan.
        app.clear_selection();
        assert_eq!(app.action_targets(), vec![app.selected_index().unwrap()]);
    }

    // -------------------------------------------------- bulk status writes

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plans-tui-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn disk_plan(dir: &Path, slug: &str, status: &str) -> Plan {
        let path = dir.join(format!("{slug}.md"));
        std::fs::write(
            &path,
            format!("---\nstatus: #{status}\ndateUpdated: 2026-01-01\n---\n\n# {slug}\n"),
        )
        .unwrap();
        Plan::from_path(&path, dir, "test", &format!("docs/plans/{slug}.md")).unwrap()
    }

    #[test]
    fn bulk_status_edit_touches_only_the_selection() {
        let dir = temp_dir("bulk-edit");
        let plans = vec![
            disk_plan(&dir, "2026-01-01-a", "TODO"),
            disk_plan(&dir, "2026-01-02-b", "TODO"),
            disk_plan(&dir, "2026-01-03-c", "TODO"),
        ];
        let mut app = App::new(
            AppInit::new(plans, Filters::default(), vec![dir.clone()])
                .journal_path(Some(dir.join("status-journal.jsonl"))),
        );
        app.group_by = GroupBy::None;
        app.sort = Sort::Created;
        app.sort_desc = false;
        app.rebuild();
        app.selected = 0;
        app.toggle_select_mode();
        app.extend_selection_by(1);

        app.begin_status_edit_with(Status::Done);
        assert!(app.status_edit.is_some());
        app.confirm_status_edit();
        assert!(app.status_edit.is_none());

        let done = ["2026-01-01-a", "2026-01-02-b", "2026-01-03-c"]
            .iter()
            .filter(|slug| {
                std::fs::read_to_string(dir.join(format!("{slug}.md")))
                    .unwrap()
                    .contains("status: #DONE")
            })
            .count();
        assert_eq!(done, 2, "exactly the two selected plans change");
        for slug in ["2026-01-01-a", "2026-01-02-b", "2026-01-03-c"] {
            let path = dir.join(format!("{slug}.md"));
            assert!(path.exists(), "no plan file is renamed or removed");
            assert!(
                std::fs::read_to_string(&path)
                    .unwrap()
                    .contains("dateUpdated:"),
                "every edit keeps a dateUpdated line"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plans_without_frontmatter_are_skipped_not_invented() {
        let dir = temp_dir("bulk-skip");
        let plain = dir.join("2026-02-01-plain.md");
        std::fs::write(&plain, "# Just a body\n").unwrap();
        let mut plans = vec![disk_plan(&dir, "2026-01-01-a", "TODO")];
        plans
            .push(Plan::from_path(&plain, &dir, "test", "docs/plans/2026-02-01-plain.md").unwrap());
        let mut app = App::new(
            AppInit::new(plans, Filters::default(), vec![dir.clone()])
                .journal_path(Some(dir.join("status-journal.jsonl"))),
        );
        app.group_by = GroupBy::None;
        app.rebuild();
        app.selection.insert(0);
        app.selection.insert(1);
        app.begin_status_edit_with(Status::Done);
        app.confirm_status_edit();

        assert_eq!(
            std::fs::read_to_string(&plain).unwrap(),
            "# Just a body\n",
            "a plan with no frontmatter is never rewritten"
        );
        assert!(
            std::fs::read_to_string(dir.join("2026-01-01-a.md"))
                .unwrap()
                .contains("status: #DONE")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- staleness

    #[test]
    fn stale_filter_uses_date_updated_and_the_cutoff() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        // 2026-09-01 is exactly 14 days before today.
        let plan = plan("a", Status::Wip, "repo", "2026-09-01", "T");
        let at = |days| Filters {
            stale_days: Some(days),
            ..Filters::default()
        };
        assert!(at(14).matches_on(&plan, today));
        assert!(!at(15).matches_on(&plan, today));
        assert!(at(1).matches_on(&plan, today));
        // 0 disables the age filter entirely.
        assert!(at(0).matches_on(&plan, today));

        let mut undated = plan.clone();
        undated.date_updated = None;
        assert!(
            !at(1).matches_on(&undated, today),
            "a plan with no date cannot be stale"
        );
    }

    #[test]
    fn stale_marker_only_fires_past_the_threshold() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let build = |stale_days| {
            let mut app = App::new(
                AppInit::new(corpus(), Filters::default(), vec![PathBuf::from("/tmp")])
                    .stale_days(stale_days)
                    .today(today),
            );
            app.rebuild();
            app
        };

        let app = build(14);
        // c-other (9/01, 14d) and d-more (8/01, 45d) are stale; the two widget
        // plans (9/10, 9/12) are not.
        assert!(app.is_stale(&app.plans[2]));
        assert!(app.is_stale(&app.plans[3]));
        assert!(!app.is_stale(&app.plans[0]));
        assert_eq!(app.visible_stale_count(), 2);

        // 0 disables the marker without changing what is visible.
        let off = build(0);
        assert_eq!(off.visible_stale_count(), 0);
        assert_eq!(off.rows.len(), app.rows.len());
    }

    #[test]
    fn stale_filter_keeps_only_old_live_plans() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let filters = Filters {
            stale_days: Some(4),
            statuses: Some(
                [Status::Todo, Status::Wip, Status::Uat]
                    .into_iter()
                    .collect(),
            ),
            ..Filters::default()
        };
        let mut app =
            App::new(AppInit::new(corpus(), filters, vec![PathBuf::from("/tmp")]).today(today));
        app.rebuild();
        // a-widget (WIP, 5 days) is the only live plan past the 4-day cutoff;
        // b-widget (UAT, 3 days) is too fresh to be stale.
        assert_eq!(plan_row_slugs(&app), vec!["a-widget"]);
    }

    // --------------------------------------------------------------- undo

    #[test]
    fn git_filters_match_the_documented_sets() {
        let dirty = GitFilters {
            dirty: true,
            ..GitFilters::default()
        };
        assert!(dirty.matches(GitStatus::Modified));
        assert!(dirty.matches(GitStatus::Untracked));
        assert!(!dirty.matches(GitStatus::Clean));
        assert!(!dirty.matches(GitStatus::Unknown));

        let untracked = GitFilters {
            untracked: true,
            ..GitFilters::default()
        };
        assert!(untracked.matches(GitStatus::Untracked));
        assert!(!untracked.matches(GitStatus::Modified));

        let clean = GitFilters {
            clean: true,
            ..GitFilters::default()
        };
        assert!(clean.matches(GitStatus::Clean));
        assert!(!clean.matches(GitStatus::Modified));

        assert!(!GitFilters::default().active());
        assert!(dirty.active());
    }

    #[test]
    fn bulk_edit_is_journaled_and_undo_restores_every_plan() {
        let dir = temp_dir("undo");
        let journal = dir.join("status-journal.jsonl");
        let plans = vec![
            disk_plan(&dir, "2026-01-01-a", "TODO"),
            disk_plan(&dir, "2026-01-02-b", "TODO"),
        ];
        let mut app = App::new(
            AppInit::new(plans, Filters::default(), vec![dir.clone()])
                .journal_path(Some(journal.clone())),
        );
        app.group_by = GroupBy::None;
        app.sort = Sort::Created;
        app.sort_desc = false;
        app.rebuild();
        app.selection.insert(0);
        app.selection.insert(1);

        // `D` records its origin and still asks to confirm.
        app.begin_quick_done();
        let edit = app.status_edit.clone().expect("modal open");
        assert_eq!(edit.source, "D");
        assert_eq!(edit.targets.len(), 2);
        app.confirm_status_edit();

        for slug in ["2026-01-01-a", "2026-01-02-b"] {
            let text = std::fs::read_to_string(dir.join(format!("{slug}.md"))).unwrap();
            assert!(text.contains("status: #DONE"), "{slug} was marked done");
            assert!(
                !text.contains("dateUpdated: 2026-01-01"),
                "{slug} bumped its date"
            );
        }

        // The journal has two entries sharing one batch.
        let entries = crate::journal::read_all(&journal).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].batch, entries[1].batch);
        assert_eq!(entries[0].source, "D");

        app.undo_last_batch();
        for slug in ["2026-01-01-a", "2026-01-02-b"] {
            let text = std::fs::read_to_string(dir.join(format!("{slug}.md"))).unwrap();
            assert!(text.contains("status: #TODO"), "{slug} was restored");
            assert!(
                text.contains("dateUpdated: 2026-01-01"),
                "{slug} date restored"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
