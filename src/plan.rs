//! Plan struct, frontmatter parsing, status enum, handoff extraction.
//!
//! Files without frontmatter are the majority in the wild and are *not* an
//! error: they get [`Status::Unknown`] and a date inferred from the
//! `YYYY-MM-DD` filename prefix (falling back to the filesystem mtime).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;
use chrono::{DateTime, Local, NaiveDate};

/// Files bigger than this are not plans we care about; skip them outright.
pub const MAX_PLAN_BYTES: u64 = 1024 * 1024;

/// How much of the file head we parse for frontmatter and the title.
pub const HEADER_PREFIX_BYTES: usize = 4096;

const HANDOFF_NEEDLE: &str = "handoff prompt";

/// Where a plan sits in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Status {
    Todo,
    Wip,
    Uat,
    Done,
    Dropped,
    /// A value the plan author used that we do not recognise, uppercased
    /// (e.g. `READY-TO-HAND-OFF`, `AUDIT`). Left alone rather than guessed at.
    Other(String),
    /// No frontmatter, or frontmatter with no usable `status:`.
    Unknown,
}

impl Status {
    /// Normalize a raw `status:` value: trim, strip surrounding quotes and a
    /// leading `#`, uppercase, then match the known lifecycle values.
    pub fn parse(raw: &str) -> Self {
        let mut t = raw.trim();
        if t.len() >= 2 {
            let first = t.as_bytes()[0];
            let last = t.as_bytes()[t.len() - 1];
            if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
                t = t[1..t.len() - 1].trim();
            }
        }
        let t = t.trim_start_matches('#').trim();
        match t.to_ascii_uppercase().as_str() {
            "" => Status::Unknown,
            "TODO" => Status::Todo,
            "WIP" => Status::Wip,
            "UAT" => Status::Uat,
            "DONE" => Status::Done,
            "DROPPED" => Status::Dropped,
            other => Status::Other(other.to_string()),
        }
    }

    /// The five statuses reachable from `--status` and the `1`-`5` keys,
    /// plus `unknown` for scripting.
    pub fn named(name: &str) -> Option<Self> {
        let t = name.trim().trim_start_matches('#');
        match t.to_ascii_uppercase().as_str() {
            "TODO" => Some(Status::Todo),
            "WIP" => Some(Status::Wip),
            "UAT" => Some(Status::Uat),
            "DONE" => Some(Status::Done),
            "DROPPED" => Some(Status::Dropped),
            "UNKNOWN" => Some(Status::Unknown),
            "ALL" => None, // handled by callers as "no filter"
            _ => None,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Status::Todo => "TODO",
            Status::Wip => "WIP",
            Status::Uat => "UAT",
            Status::Done => "DONE",
            Status::Dropped => "DROPPED",
            Status::Other(s) => s.as_str(),
            Status::Unknown => "UNKNOWN",
        }
    }

    /// Stable lowercase key for machine consumers. `Other` collapses to
    /// `other` — the raw value is in `status_raw`.
    pub fn key(&self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::Wip => "wip",
            Status::Uat => "uat",
            Status::Done => "done",
            Status::Dropped => "dropped",
            Status::Other(_) => "other",
            Status::Unknown => "unknown",
        }
    }

    /// Canonical display order for group headers.
    pub fn rank(&self) -> u8 {
        match self {
            Status::Todo => 0,
            Status::Wip => 1,
            Status::Uat => 2,
            Status::Done => 3,
            Status::Dropped => 4,
            Status::Unknown => 5,
            Status::Other(_) => 6,
        }
    }

    /// The statuses shown by default (both TUI and `--list`).
    pub fn live_set() -> Vec<Status> {
        vec![Status::Todo, Status::Wip, Status::Uat, Status::Unknown]
    }

    /// The five statuses the `S` editor can write, in menu order.
    pub fn lifecycle() -> [Status; 5] {
        [
            Status::Todo,
            Status::Wip,
            Status::Uat,
            Status::Done,
            Status::Dropped,
        ]
    }

    pub fn is_lifecycle(&self) -> bool {
        Self::lifecycle().contains(self)
    }
}

/// A plan file on disk, after parsing.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Absolute path to the markdown file.
    pub path: PathBuf,
    /// Nearest ancestor (up to the scan root) containing `.git`.
    pub repo_root: PathBuf,
    /// Repo path relative to the scan root, e.g. `apps/notes`.
    pub repo_name: String,
    /// Path relative to `repo_root`, e.g. `docs/plans/2026-…md`.
    pub rel_path: String,
    /// Filename stem, e.g. `2026-09-14-widgetbox-hud-menu`.
    pub slug: String,
    pub title: String,
    pub status: Status,
    /// The raw `status:` value as written, quotes and `#` intact.
    pub status_raw: Option<String>,
    /// The status came from a body `**Status:**` line or a filename prefix,
    /// not from frontmatter.
    pub status_inferred: bool,
    pub date_created: Option<NaiveDate>,
    pub date_updated: Option<NaiveDate>,
    pub tags: Vec<String>,
    pub owner: Option<String>,
    pub has_handoff: bool,
    /// Did the file actually open with a `---` fence?
    pub has_frontmatter: bool,
    /// BLAKE3 of the file's bytes, hex. Byte-identical files share a hash;
    /// near-duplicates deliberately do not.
    pub content_hash: String,
    /// For a non-canonical copy of an identical file, the canonical copy's
    /// absolute path. Also set for a plan that lives in a worktree whose
    /// main checkout holds the same repo-relative path, even when the two
    /// files differ. `None` for the canonical copy and for files with no
    /// twin. Assigned by [`crate::scan::find_plans`], not by parsing.
    pub duplicate_of: Option<PathBuf>,
    /// The plan lives in a linked git worktree checkout (its repo's `.git`
    /// points at the main checkout's `worktrees/`), not the main checkout
    /// itself. Set by the scanner, not by parsing.
    pub in_worktree: bool,
}

impl Plan {
    /// Best date to show in the list: updated, else created.
    pub fn display_date(&self) -> Option<NaiveDate> {
        self.date_updated.or(self.date_created)
    }

    /// The canonical copy of an identical group (or a file with no twin).
    pub fn is_canonical(&self) -> bool {
        self.duplicate_of.is_none()
    }

    /// A worktree plan whose main checkout has the same repo-relative file.
    /// The default filters drop these: a worktree is a transient checkout of
    /// a repo the list already shows.
    pub fn is_worktree_copy(&self) -> bool {
        self.in_worktree && self.duplicate_of.is_some()
    }

    /// Days since `date_updated`, or `None` when the plan has no date at all.
    /// This is the number the staleness filter and marker are built on, so it
    /// deliberately ignores `date_created`.
    pub fn age_days(&self, today: NaiveDate) -> Option<i64> {
        self.date_updated.map(|date| (today - date).num_days())
    }

    /// The string fuzzy search runs against.
    pub fn search_blob(&self) -> String {
        let mut s = String::with_capacity(128);
        s.push_str(&self.repo_name);
        s.push(' ');
        s.push_str(&self.slug);
        s.push(' ');
        s.push_str(&self.title);
        for tag in &self.tags {
            s.push(' ');
            s.push_str(tag);
        }
        s
    }

    /// Read the plan and return its `## Handoff Prompt` code block, fences
    /// stripped. Lazy: only called when the user asks for it.
    pub fn handoff_prompt(&self) -> Option<String> {
        read_handoff(&self.path).ok().flatten()
    }

    /// Read the plan once for the detail pane: body plus handoff. Lazy.
    pub fn detail(&self) -> Option<Detail> {
        read_detail(&self.path).ok()
    }

    /// A handoff prompt built from the plan's location, for plans that never
    /// wrote one. Always available.
    pub fn synthesized_handoff(&self) -> String {
        synthesize_handoff(self)
    }

    /// The slug with its `YYYY-MM-DD-` filename prefix removed, when present.
    /// Display-only: the canonical `slug` is what selectors (`--handoff`,
    /// `--copy`) and the JSON field keep, so stripping the date can never make
    /// a plan unreachable.
    pub fn slug_without_date(&self) -> &str {
        strip_date_prefix(&self.slug)
    }

    /// Parse the file at `path`, reading it once.
    pub fn from_path(
        path: &Path,
        repo_root: &Path,
        repo_name: &str,
        rel_path: &str,
    ) -> anyhow::Result<Plan> {
        let meta = std::fs::metadata(path)?;
        if meta.len() > MAX_PLAN_BYTES {
            anyhow::bail!("plan is larger than 1 MiB");
        }
        let bytes = std::fs::read(path)?;
        // One read feeds both the parse and the duplicate hash.
        let content_hash = blake3::hash(&bytes).to_hex().to_string();
        let content = String::from_utf8_lossy(&bytes);
        let mtime = meta.modified().ok().map(system_time_to_date);
        let slug = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();

        let mut header = parse_header(head(&content, HEADER_PREFIX_BYTES), &slug, mtime);
        // `has_handoff` needs the whole file; a heading past the 4 KiB prefix
        // still counts.
        header.has_handoff = header.has_handoff || has_handoff_section(&content);

        Ok(Plan {
            path: path.to_path_buf(),
            repo_root: repo_root.to_path_buf(),
            repo_name: repo_name.to_string(),
            rel_path: rel_path.to_string(),
            slug,
            title: header.title,
            status: header.status,
            status_raw: header.status_raw,
            status_inferred: header.status_inferred,
            date_created: header.date_created,
            date_updated: header.date_updated,
            tags: header.tags,
            owner: header.owner,
            has_handoff: header.has_handoff,
            has_frontmatter: header.has_frontmatter,
            content_hash,
            duplicate_of: None,
            in_worktree: false,
        })
    }
}

/// Everything we can learn from the head of a plan file.
#[derive(Debug, Clone)]
pub struct ParsedHeader {
    pub title: String,
    pub status: Status,
    pub status_raw: Option<String>,
    pub status_inferred: bool,
    pub date_created: Option<NaiveDate>,
    pub date_updated: Option<NaiveDate>,
    pub tags: Vec<String>,
    pub owner: Option<String>,
    pub has_handoff: bool,
    pub has_frontmatter: bool,
}

/// Parse the header slice of a plan. `mtime` supplies the date fallback.
pub fn parse_header(prefix: &str, stem: &str, mtime: Option<NaiveDate>) -> ParsedHeader {
    let content = prefix.strip_prefix('\u{feff}').unwrap_or(prefix);
    let (block, body) = split_frontmatter(content);

    let mut status_raw = None;
    let mut fm_created = None;
    let mut fm_updated = None;
    let mut tags = Vec::new();
    let mut owner = None;

    if let Some(block) = block {
        // YAML loses `status: #TODO` (the value reads as a comment), so the
        // status always comes from a line scan. YAML supplies the rest and is
        // never allowed to hard-fail.
        status_raw = scan_status(block);
        if let Ok(serde_yaml::Value::Mapping(map)) =
            serde_yaml::from_str::<serde_yaml::Value>(block)
        {
            fm_created = map_get(&map, "dateCreated")
                .and_then(scalar)
                .and_then(|s| parse_date(&s));
            fm_updated = map_get(&map, "dateUpdated")
                .and_then(scalar)
                .and_then(|s| parse_date(&s));
            owner = map_get(&map, "owner")
                .and_then(scalar)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            tags = map_get(&map, "tags").map(parse_tags).unwrap_or_default();
        }
    }

    let mut status = status_raw
        .as_deref()
        .map(Status::parse)
        .unwrap_or(Status::Unknown);
    let mut status_inferred = false;
    if status_raw.is_none() {
        if let Some(raw) = scan_body_status(body) {
            status = Status::parse(first_status_word(&raw));
            status_raw = Some(raw);
            status_inferred = true;
        } else if let Some(from_stem) = status_from_stem(stem) {
            status = from_stem;
            status_inferred = true;
        }
    }

    let title = body
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| stem.to_string());

    ParsedHeader {
        title,
        status,
        status_raw,
        status_inferred,
        date_created: fm_created.or_else(|| date_from_filename(stem)).or(mtime),
        date_updated: fm_updated.or(mtime),
        tags,
        owner,
        has_handoff: has_handoff_section(content),
        has_frontmatter: block.is_some(),
    }
}

/// The value of a `**Status:**` line in the first 15 lines of a body, cut at
/// the first `|`. Plain `status:` is ignored: it matches code snippets.
fn scan_body_status(body: &str) -> Option<String> {
    body.lines().take(15).find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("> ").unwrap_or(line);
        let rest = line.strip_prefix("**")?;
        let label = rest.get(..6)?;
        if !label.eq_ignore_ascii_case("status") {
            return None;
        }
        let rest = rest.get(6..)?;
        let value = rest
            .strip_prefix(":**")
            .or_else(|| rest.strip_prefix("**:"))?;
        let value = value.split('|').next().unwrap_or("").trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// The first word of an inferred status line: `Approved in conversation; …`
/// becomes `Approved`.
fn first_status_word(raw: &str) -> &str {
    let raw = raw.trim().trim_start_matches('#');
    let end = raw
        .find(|c: char| c.is_whitespace() || "(,;.—".contains(c))
        .unwrap_or(raw.len());
    &raw[..end]
}

/// A legacy `TODO-`/`WIP-`/`UAT-`/`DONE-`/`DROPPED-` filename prefix.
fn status_from_stem(stem: &str) -> Option<Status> {
    let (prefix, _) = stem.split_once('-')?;
    match prefix.to_ascii_uppercase().as_str() {
        "TODO" => Some(Status::Todo),
        "WIP" => Some(Status::Wip),
        "UAT" => Some(Status::Uat),
        "DONE" => Some(Status::Done),
        "DROPPED" => Some(Status::Dropped),
        _ => None,
    }
}

/// Split `---\n…\n---\n` off the front of a document.
///
/// Returns `(frontmatter, body)`. The fence lines are not included in either.
pub(crate) fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(after_open) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (None, content);
    };

    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        if bare == "---" {
            let body_start = offset + line.len();
            return (Some(&after_open[..offset]), &after_open[body_start..]);
        }
        offset += line.len();
    }

    // A closing fence at EOF without a trailing newline.
    if after_open.trim_end_matches(['\r', '\n']).ends_with("---") {
        let block = after_open
            .trim_end_matches(['\r', '\n'])
            .strip_suffix("---")
            .unwrap_or(after_open);
        return (Some(block), "");
    }

    (None, content)
}

/// `^\s*status:\s*(.+)$`, the first non-empty match.
fn scan_status(block: &str) -> Option<String> {
    for line in block.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("status:") {
            let value = rest.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn map_get<'a>(map: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    map.get(serde_yaml::Value::String(key.to_string()))
}

fn scalar(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn parse_tags(value: &serde_yaml::Value) -> Vec<String> {
    match value {
        serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .filter_map(scalar)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        // Tolerate `tags: plan, tui` written as a bare string.
        other => scalar(other)
            .map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn parse_date(raw: &str) -> Option<NaiveDate> {
    let raw = raw.trim();
    let head = raw.get(..10).unwrap_or(raw);
    NaiveDate::parse_from_str(head, "%Y-%m-%d").ok()
}

/// `YYYY-MM-DD` at the front of the filename stem.
fn date_from_filename(stem: &str) -> Option<NaiveDate> {
    let head = stem.get(..10)?;
    NaiveDate::parse_from_str(head, "%Y-%m-%d").ok()
}

fn system_time_to_date(time: SystemTime) -> NaiveDate {
    let dt: DateTime<Local> = time.into();
    dt.date_naive()
}

/// A UTF-8-safe prefix of at most `max` bytes.
fn head(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Does the file contain a `## Handoff Prompt`-style heading?
pub fn has_handoff_section(content: &str) -> bool {
    content.lines().any(is_handoff_heading)
}

fn is_handoff_heading(line: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return false;
    }
    starts_with_handoff_prompt(trimmed[hashes..].trim_start())
}

fn starts_with_handoff_prompt(s: &str) -> bool {
    match s.get(..HANDOFF_NEEDLE.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(HANDOFF_NEEDLE) => {
            match s.as_bytes().get(HANDOFF_NEEDLE.len()) {
                // `\b` after "prompt": end of string, or a non-word char.
                None => true,
                Some(b) => !(b.is_ascii_alphanumeric() || *b == b'_'),
            }
        }
        _ => false,
    }
}

/// Pull the content of the first fenced block after a `## Handoff Prompt`
/// heading. Fences are stripped; a block that runs to EOF is returned as-is.
pub fn extract_handoff(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();

    let heading = lines.iter().position(|line| is_handoff_heading(line))?;

    // First fenced line after the heading is the opener; anything it says on
    // its own line is an info string we discard.
    let opener = lines[heading + 1..]
        .iter()
        .position(|line| fence(line).is_some())?
        + heading
        + 1;
    let (fence_char, fence_len, _) = fence(lines[opener])?;

    let mut out: Vec<&str> = Vec::new();
    for line in &lines[opener + 1..] {
        if let Some((c, n, has_info)) = fence(line)
            && c == fence_char
            && n >= fence_len
            && !has_info
        {
            return Some(out.join("\n"));
        }
        out.push(line);
    }

    // Ran to EOF with no closer.
    Some(out.join("\n"))
}

/// `(char, count, has_info_string)` for a fenced-code line.
fn fence(line: &str) -> Option<(char, usize, bool)> {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|c| *c == ch).count();
    if count < 3 {
        return None;
    }
    let rest = &trimmed[count..];
    Some((ch, count, !rest.trim().is_empty()))
}

/// `2026-09-13-warden-endless-chunks` -> `warden-endless-chunks` when the
/// leading `YYYY-MM-DD-` really is a date; otherwise the slug is unchanged.
/// Byte-indexed: a slug that is not exactly ASCII digits at 0..=9 keeps its
/// full form, and a date-only slug (`2026-09-13`) is left intact rather than
/// stripped to nothing.
pub fn strip_date_prefix(slug: &str) -> &str {
    let bytes = slug.as_bytes();
    if bytes.len() <= 11 || bytes[10] != b'-' {
        return slug;
    }
    let is_digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if bytes[4] == b'-'
        && bytes[7] == b'-'
        && is_digits(0..4)
        && is_digits(5..7)
        && is_digits(8..10)
    {
        &slug[11..]
    } else {
        slug
    }
}

/// Synthesize a handoff prompt from the plan's location and title.
pub fn synthesize_handoff(plan: &Plan) -> String {
    format!(
        "Repo: {repo}\n\
         Plan: {rel}  (read it in full first)\n\
         \n\
         {title}\n\
         \n\
         Read the plan and execute it. Commit in small steps as you go; never rewrite git history.\n\
         When the acceptance criteria pass, set the plan's frontmatter status: to #UAT (or #DONE if\n\
         you verified it yourself), bump dateUpdated:, and commit that with the work.\n",
        repo = plan.repo_root.display(),
        rel = plan.rel_path,
        title = plan.title,
    )
}

/// Read a plan's handoff block straight from disk, lazily.
pub fn read_handoff(path: &Path) -> anyhow::Result<Option<String>> {
    let bytes = std::fs::read(path)?;
    let content = String::from_utf8_lossy(&bytes);
    Ok(extract_handoff(&content))
}

/// A plan as the detail pane shows it: the body without frontmatter, plus the
/// extracted handoff block (if any).
#[derive(Debug, Clone)]
pub struct Detail {
    pub handoff: Option<String>,
    pub body: String,
}

/// Read a plan once from disk and return its body and handoff block.
pub fn read_detail(path: &Path) -> anyhow::Result<Detail> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let content = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let body = split_frontmatter(content).1.to_string();
    Ok(Detail {
        handoff: extract_handoff(content),
        body,
    })
}

// ------------------------------------------------------------- status edits

/// Rewrite only the `status:` and `dateUpdated:` lines inside the frontmatter,
/// byte-for-byte preserving everything else, then rename a temp file over the
/// original. The filename is never touched and nothing is staged or committed.
///
/// Refuses plans with no frontmatter rather than inventing one.
pub fn set_status_in_file(path: &Path, status: &Status, today: NaiveDate) -> anyhow::Result<()> {
    set_status_and_date_in_file(path, status, today).map(|_| ())
}

/// [`set_status_in_file`] with the date supplied, so an undo can restore the
/// `dateUpdated` that was there before a write. Returns whether the file
/// actually changed — an undo that restores an already-restored file is a
/// no-op and should not be reported as work.
pub fn set_status_and_date_in_file(
    path: &Path,
    status: &Status,
    date: NaiveDate,
) -> anyhow::Result<bool> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let Some(rewritten) = rewrite_status(&content, status, date) else {
        anyhow::bail!(
            "{} has no frontmatter; refusing to invent one",
            path.display()
        );
    };
    if rewritten == content {
        return Ok(false);
    }

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.plans-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, &rewritten).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::from(err).context(format!("replacing {}", path.display())));
    }
    Ok(true)
}

/// The pure half of [`set_status_in_file`]. `None` means "no frontmatter".
fn rewrite_status(content: &str, status: &Status, today: NaiveDate) -> Option<String> {
    let mut lines: Vec<String> = content.split_inclusive('\n').map(str::to_string).collect();
    if lines.first().map(|line| line.trim_end()) != Some("---") {
        return None;
    }
    let close = closing_fence(&lines)?;
    // Inserted keys match the file's own line endings.
    let fallback = lines
        .first()
        .map(|line| line_ending(line))
        .filter(|ending| !ending.is_empty())
        .unwrap_or("\n");

    // Locate the keys first: inserting one shifts every later index.
    let mut status_at = None;
    let mut updated_at = None;
    for (i, line) in lines.iter().enumerate().take(close).skip(1) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("status:") {
            status_at = Some(i);
        } else if trimmed.starts_with("dateUpdated:") {
            updated_at = Some(i);
        }
    }

    let write = |lines: &mut Vec<String>, at: Option<usize>, key: &str, value: &str| match at {
        Some(i) => {
            let indent = indent_of(&lines[i]).to_string();
            let ending = line_ending(&lines[i]).to_string();
            lines[i] = format!("{indent}{key}: {value}{ending}");
        }
        None => {
            let close = closing_fence(lines).unwrap_or(lines.len().saturating_sub(1));
            lines.insert(close, format!("{key}: {value}{fallback}"));
        }
    };

    write(
        &mut lines,
        status_at,
        "status",
        &format!("#{}", status.label()),
    );
    write(
        &mut lines,
        updated_at,
        "dateUpdated",
        &today.format("%Y-%m-%d").to_string(),
    );

    Some(lines.concat())
}

fn closing_fence(lines: &[String]) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim_end() == "---")
        .map(|(i, _)| i)
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

fn line_ending(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(y, m, d)
    }

    #[test]
    fn strip_date_prefix_only_when_a_real_date_is_there() {
        assert_eq!(
            strip_date_prefix("2026-09-13-warden-endless-chunks"),
            "warden-endless-chunks"
        );
        // A slug that is only a date is left whole, not stripped to nothing.
        assert_eq!(strip_date_prefix("2026-09-13"), "2026-09-13");
        // Not a date: no four-digit year, or the separators are wrong.
        assert_eq!(
            strip_date_prefix("widgetbox-hud-menu"),
            "widgetbox-hud-menu"
        );
        assert_eq!(strip_date_prefix("2026-09_13-warden"), "2026-09_13-warden");
        assert_eq!(strip_date_prefix("20a6-09-13-warden"), "20a6-09-13-warden");
    }

    #[test]
    fn full_frontmatter_with_hash_uat() {
        let content = "---\n\
             dateCreated: 2026-09-13\n\
             dateUpdated: 2026-09-14\n\
             tags: [plan, tui]\n\
             owner: unassigned\n\
             status: #UAT\n\
             ---\n\
             \n\
             # Gotchi Box — face-always HUD\n";
        let h = parse_header(content, "2026-09-13-widgetbox-hud-menu", None);
        assert_eq!(h.status, Status::Uat);
        assert_eq!(h.status_raw.as_deref(), Some("#UAT"));
        assert!(h.has_frontmatter);
        assert_eq!(h.title, "Gotchi Box — face-always HUD");
        assert_eq!(h.tags, vec!["plan", "tui"]);
        assert_eq!(h.owner.as_deref(), Some("unassigned"));
        assert_eq!(h.date_created, date(2026, 9, 13));
        assert_eq!(h.date_updated, date(2026, 9, 14));
    }

    #[test]
    fn hash_todo_is_not_swallowed_by_yaml_comment() {
        // serde_yaml sees `status: #TODO` as an absent key.
        let content = "---\nstatus: #TODO\n---\n\n# Thing\n";
        let h = parse_header(content, "thing", None);
        assert_eq!(h.status, Status::Todo);
        assert_eq!(h.status_raw.as_deref(), Some("#TODO"));
    }

    #[test]
    fn no_frontmatter_infers_status_and_filename_date() {
        let h = parse_header("# Some Body\n", "2026-09-13-foo", None);
        assert_eq!(h.status, Status::Unknown);
        assert!(h.status_raw.is_none());
        assert!(!h.has_frontmatter);
        assert_eq!(h.date_created, date(2026, 9, 13));
        assert_eq!(h.title, "Some Body");
    }

    #[test]
    fn unknown_status_value_becomes_other_uppercased() {
        let content = "---\nstatus: ready-to-hand-off\n---\n\n# Foo\n";
        let h = parse_header(content, "foo", None);
        assert_eq!(h.status, Status::Other("READY-TO-HAND-OFF".to_string()));
    }

    #[test]
    fn title_comes_from_body_heading_not_frontmatter() {
        let content = "---\ntitle: Frontmatter Title\nstatus: #WIP\n---\n\n# Real Title\n";
        let h = parse_header(content, "fallback", None);
        assert_eq!(h.title, "Real Title");
    }

    #[test]
    fn mtime_fills_missing_dates() {
        let h = parse_header("# Foo\n", "no-date-here", date(2026, 1, 2));
        assert_eq!(h.date_created, date(2026, 1, 2));
        assert_eq!(h.date_updated, date(2026, 1, 2));
    }

    #[test]
    fn quoted_status_is_accepted() {
        assert_eq!(Status::parse("\"#DONE\""), Status::Done);
        assert_eq!(Status::parse("#wip"), Status::Wip);
        assert_eq!(Status::parse("  "), Status::Unknown);
    }

    #[test]
    fn status_keys_are_stable_for_machines() {
        assert_eq!(Status::Todo.key(), "todo");
        assert_eq!(Status::Unknown.key(), "unknown");
        assert_eq!(Status::Other("READY".into()).key(), "other");
        // The human label keeps the one-off value; the key does not.
        assert_eq!(Status::Other("READY".into()).label(), "READY");
    }

    #[test]
    fn malformed_yaml_keeps_status_and_is_not_fatal() {
        let content = "---\nstatus: #WIP\ntags: [unclosed\n---\n\n# Still Parsed\n";
        let h = parse_header(content, "still-parsed", None);
        assert_eq!(h.status, Status::Wip);
        assert_eq!(h.title, "Still Parsed");
        assert!(h.tags.is_empty());
    }

    #[test]
    fn handoff_block_is_extracted_without_fences() {
        let content =
            "# T\n\n## Handoff Prompt\n\n```\nRepo: /x\nPlan: docs/plans/y.md\n```\n\n## Next\n";
        assert!(has_handoff_section(content));
        let body = extract_handoff(content).unwrap();
        assert_eq!(body, "Repo: /x\nPlan: docs/plans/y.md");
        assert!(!body.contains("```"));
    }

    #[test]
    fn handoff_survives_a_longer_closing_fence_and_eof() {
        let content = "## Handoff Prompt\n\n~~~~\nbody\n~~~~~\n";
        assert_eq!(extract_handoff(content).unwrap(), "body");

        let unterminated = "## handoff prompt\n\n```\nno closer\n";
        assert_eq!(extract_handoff(unterminated).unwrap(), "no closer");
    }

    #[test]
    fn read_detail_returns_body_without_fence_and_handoff() {
        let dir = std::env::temp_dir().join(format!("plans-tui-detail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.md");
        std::fs::write(
            &path,
            "---\nstatus: #TODO\n---\n\n# Title\n\n## Handoff Prompt\n\n```\nRepo: /x\n```\n\ntrailing prose\n",
        )
        .unwrap();
        let d = read_detail(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(!d.body.contains("---"));
        assert!(d.body.contains("trailing prose"));
        assert_eq!(d.handoff.as_deref(), Some("Repo: /x"));
    }

    #[test]
    fn body_status_line_is_inferred() {
        let h = parse_header("# T\n\n**Status:** DONE | **Owner:** x\n", "t", None);
        assert_eq!(h.status, Status::Done);
        assert!(h.status_inferred);
    }

    #[test]
    fn quoted_body_status_keeps_full_text_and_other_status() {
        let h = parse_header(
            "# T\n\n> **Status:** Approved in conversation; awaiting review\n",
            "t",
            None,
        );
        assert_eq!(h.status, Status::Other("APPROVED".to_string()));
        assert_eq!(
            h.status_raw.as_deref(),
            Some("Approved in conversation; awaiting review")
        );
        assert!(h.status_inferred);
    }

    #[test]
    fn code_block_status_is_not_a_status() {
        let h = parse_header("# T\n\n```\nstatus: text(\"status\")\n```\n", "t", None);
        assert_eq!(h.status, Status::Unknown);
        assert!(!h.status_inferred);
    }

    #[test]
    fn filename_prefix_infers_status() {
        let h = parse_header("# T\n", "DONE-2026-09-10-foo", None);
        assert_eq!(h.status, Status::Done);
        assert!(h.status_raw.is_none());
        assert!(h.status_inferred);
    }

    #[test]
    fn frontmatter_status_beats_body_status() {
        let h = parse_header("---\nstatus: #WIP\n---\n\n**Status:** DONE\n", "t", None);
        assert_eq!(h.status, Status::Wip);
        assert!(!h.status_inferred);
    }

    #[test]
    fn no_handoff_when_heading_absent() {
        assert!(!has_handoff_section("# Title\n\nplain body\n"));
        assert_eq!(extract_handoff("# Title\n\nplain body\n"), None);
    }

    #[test]
    fn status_edit_rewrites_only_the_two_lines() {
        let today = date(2026, 9, 14).unwrap();
        let content = "---\ndateCreated: 2026-01-01\ndateUpdated: 2026-01-02\ntags: [plan, a]\nowner: unassigned\nstatus: #WIP\n---\n\n# Title\n\nbody — untouched\n";
        let out = rewrite_status(content, &Status::Uat, today).unwrap();
        assert_eq!(
            out,
            "---\ndateCreated: 2026-01-01\ndateUpdated: 2026-09-14\ntags: [plan, a]\nowner: unassigned\nstatus: #UAT\n---\n\n# Title\n\nbody — untouched\n"
        );
    }

    #[test]
    fn status_edit_inserts_missing_keys_at_the_end_of_the_block() {
        let today = date(2026, 9, 14).unwrap();
        let out = rewrite_status("---\ntags: [plan]\n---\n\nbody\n", &Status::Done, today).unwrap();
        assert_eq!(
            out,
            "---\ntags: [plan]\nstatus: #DONE\ndateUpdated: 2026-09-14\n---\n\nbody\n"
        );
    }

    #[test]
    fn status_edit_preserves_crlf() {
        let today = date(2026, 9, 14).unwrap();
        let out = rewrite_status(
            "---\r\nstatus: #TODO\r\n---\r\nbody\r\n",
            &Status::Wip,
            today,
        )
        .unwrap();
        assert_eq!(
            out,
            "---\r\nstatus: #WIP\r\ndateUpdated: 2026-09-14\r\n---\r\nbody\r\n"
        );
    }

    #[test]
    fn status_edit_refuses_plans_without_frontmatter() {
        let today = date(2026, 9, 14).unwrap();
        assert!(rewrite_status("# no frontmatter\n", &Status::Done, today).is_none());
        assert!(rewrite_status("---\nstatus: #TODO\n", &Status::Done, today).is_none());
    }

    #[test]
    fn set_status_in_file_is_atomic_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("plans-tui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-01-01-x.md");
        std::fs::write(&path, "---\nstatus: #TODO\n---\n\n# X\n").unwrap();

        set_status_in_file(&path, &Status::Uat, date(2026, 9, 14).unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "---\nstatus: #UAT\ndateUpdated: 2026-09-14\n---\n\n# X\n"
        );
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("plans-tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_status_and_date_restores_an_older_date() {
        let dir = std::env::temp_dir().join(format!("plans-tui-date-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-01-01-x.md");
        std::fs::write(
            &path,
            "---\nstatus: #DONE\ndateUpdated: 2026-09-15\n---\n\n# X\n",
        )
        .unwrap();
        set_status_and_date_in_file(&path, &Status::Wip, date(2026, 3, 3).unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "---\nstatus: #WIP\ndateUpdated: 2026-03-03\n---\n\n# X\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
