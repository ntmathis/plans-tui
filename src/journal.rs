//! Append-only journal of status writes, and the undo built on top of it.
//!
//! The journal is a **write-ahead log for undo**, not a cache of parsed plans
//! and not a source of truth. It lives at `~/.config/plans/status-journal.jsonl`
//! (override with `$PLANS_JOURNAL`), one JSON object per line. Every entry from
//! a single confirm shares a `batch` id, so a bulk edit is undone as one unit.
//!
//! Writing it is always best-effort: a full disk or a read-only config
//! directory must never stop the status edit itself. Undoing, by contrast, is
//! allowed to report failures.

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::plan::{Plan, Status, set_status_and_date_in_file};

/// The journal is a log, not a cache: on load it is capped to this many lines.
pub const MAX_LINES: usize = 1000;

/// One file's worth of one status edit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// RFC3339 timestamp of the write.
    pub at: String,
    /// Every entry from one confirm shares this id.
    pub batch: String,
    /// Absolute path of the plan.
    pub path: String,
    pub repo_root: String,
    pub rel_path: String,
    /// The status label before the write, e.g. `TODO` or `READY`.
    pub from_status: String,
    /// `YYYY-MM-DD` before the write, when there was one.
    pub from_updated: Option<String>,
    pub to_status: String,
    pub to_updated: Option<String>,
    /// `D`, `S` or `cli`.
    pub source: String,
}

impl Entry {
    /// Record the write of `plan` to `to_status` on `to_updated`.
    pub fn from_write(
        batch: &str,
        plan: &Plan,
        to_status: &Status,
        to_updated: NaiveDate,
        source: &str,
    ) -> Entry {
        Entry {
            at: Local::now().to_rfc3339(),
            batch: batch.to_string(),
            path: plan.path.to_string_lossy().into_owned(),
            repo_root: plan.repo_root.to_string_lossy().into_owned(),
            rel_path: plan.rel_path.clone(),
            from_status: plan.status.label().to_string(),
            from_updated: plan.date_updated.map(|date| date.to_string()),
            to_status: to_status.label().to_string(),
            to_updated: Some(to_updated.to_string()),
            source: source.to_string(),
        }
    }

    fn previous_date(&self) -> NaiveDate {
        self.from_updated
            .as_deref()
            .and_then(|raw| NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok())
            .unwrap_or_else(|| Local::now().date_naive())
    }
}

/// What an undo actually did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RevertReport {
    pub batches: usize,
    pub files: usize,
    /// One message per file that could not be restored.
    pub errors: Vec<String>,
}

/// `~/.config/plans/status-journal.jsonl`, or `$PLANS_JOURNAL` when set.
pub fn default_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PLANS_JOURNAL") {
        return Some(PathBuf::from(path));
    }
    Some(dirs::home_dir()?.join(".config/plans/status-journal.jsonl"))
}

/// A fresh batch id. Unique enough without a uuid dependency: microseconds plus
/// the pid, and two confirms cannot share a microsecond in one process.
pub fn new_batch() -> String {
    format!("{}-{}", Utc::now().timestamp_micros(), std::process::id())
}

/// Append `entries` to the journal, creating the file and its directory.
pub fn append(path: &Path, entries: &[Entry]) -> anyhow::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut text = String::new();
    for entry in entries {
        text.push_str(&serde_json::to_string(entry)?);
        text.push('\n');
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// Read every entry, skipping malformed lines. If the file has grown past
/// [`MAX_LINES`] lines it is rewritten once to its most recent [`MAX_LINES`]
/// entries.
pub fn read_all(path: &Path) -> anyhow::Result<Vec<Entry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };

    let mut entries: Vec<Entry> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        .collect();

    if text.lines().count() > MAX_LINES {
        // Keep the most recent 1000 valid entries, atomically.
        let start = entries.len().saturating_sub(MAX_LINES);
        if let Err(err) = truncate(path, &entries[start..]) {
            eprintln!("plans: could not trim {}: {err}", path.display());
        }
    }
    if entries.len() > MAX_LINES {
        entries = entries.split_off(entries.len() - MAX_LINES);
    }
    Ok(entries)
}

/// Rewrite `path` with `entries`, via a temp file and a rename.
fn truncate(path: &Path, entries: &[Entry]) -> anyhow::Result<()> {
    let mut body = String::new();
    for entry in entries {
        body.push_str(&serde_json::to_string(entry)?);
        body.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Group entries into batches, preserving file order. Entries from one confirm
/// are written contiguously, so a run of equal `batch` ids is one batch.
pub fn batches(entries: &[Entry]) -> Vec<Vec<Entry>> {
    let mut out: Vec<Vec<Entry>> = Vec::new();
    for entry in entries {
        match out.last_mut() {
            Some(batch) if batch[0].batch == entry.batch => batch.push(entry.clone()),
            _ => out.push(vec![entry.clone()]),
        }
    }
    out
}

/// Restore the previous status and `dateUpdated` for the `count` most recent
/// batches. Reverts are not themselves journaled, so undo cannot be undone
/// into a loop.
pub fn revert_batches(path: &Path, count: usize) -> anyhow::Result<RevertReport> {
    let entries = read_all(path)?;
    let batches = batches(&entries);

    let mut report = RevertReport::default();
    for batch in batches.iter().rev().take(count) {
        report.batches += 1;
        for entry in batch {
            let status = Status::parse(&entry.from_status);
            let date = entry.previous_date();
            match set_status_and_date_in_file(Path::new(&entry.path), &status, date) {
                Ok(true) => report.files += 1,
                Ok(false) => {}
                Err(err) => report.errors.push(format!("{}: {err}", entry.rel_path)),
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plans-journal-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("status-journal.jsonl")
    }

    fn entry(batch: &str, rel: &str, from: &str, to: &str, from_updated: &str) -> Entry {
        Entry {
            at: "2026-09-15T12:00:00-07:00".to_string(),
            batch: batch.to_string(),
            path: format!("/tmp/{rel}"),
            repo_root: "/tmp".to_string(),
            rel_path: rel.to_string(),
            from_status: from.to_string(),
            from_updated: Some(from_updated.to_string()),
            to_status: to.to_string(),
            to_updated: Some("2026-09-15".to_string()),
            source: "S".to_string(),
        }
    }

    #[test]
    fn append_then_read_round_trips() {
        let path = temp_path("roundtrip");
        let entries = vec![
            entry("b1", "docs/plans/a.md", "TODO", "DONE", "2026-01-01"),
            entry("b1", "docs/plans/b.md", "WIP", "DONE", "2026-01-02"),
        ];
        append(&path, &entries).unwrap();
        let read = read_all(&path).unwrap();
        assert_eq!(read, entries);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_journal_reads_as_empty() {
        let path = temp_path("missing");
        assert!(read_all(&path).unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let path = temp_path("malformed");
        append(&path, &[entry("b1", "a.md", "TODO", "DONE", "2026-01-01")]).unwrap();
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str("not json\n");
        body.push_str(
            &serde_json::to_string(&entry("b2", "b.md", "WIP", "UAT", "2026-01-02")).unwrap(),
        );
        body.push('\n');
        std::fs::write(&path, body).unwrap();
        let read = read_all(&path).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[1].rel_path, "b.md");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn batches_group_consecutive_entries() {
        let entries = vec![
            entry("b1", "a.md", "TODO", "DONE", "2026-01-01"),
            entry("b1", "b.md", "WIP", "DONE", "2026-01-02"),
            entry("b2", "c.md", "UAT", "DROPPED", "2026-01-03"),
        ];
        let batches = batches(&entries);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn read_all_caps_the_journal_at_max_lines() {
        let path = temp_path("cap");
        let mut entries = Vec::new();
        for i in 0..(MAX_LINES + 5) {
            entries.push(entry(
                "b",
                &format!("p{i}.md"),
                "TODO",
                "DONE",
                "2026-01-01",
            ));
        }
        append(&path, &entries).unwrap();
        let read = read_all(&path).unwrap();
        assert_eq!(read.len(), MAX_LINES);
        // The most recent line survived the trim.
        assert_eq!(
            read.last().unwrap().rel_path,
            format!("p{}.md", MAX_LINES + 4)
        );
        // And the file on disk was rewritten, not just trimmed in memory.
        let on_disk = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(on_disk, MAX_LINES);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn revert_restores_status_and_date_on_disk() {
        let path = temp_path("revert");
        let dir = path.parent().unwrap().to_path_buf();
        let plan_a = dir.join("a.md");
        let plan_b = dir.join("b.md");
        std::fs::write(
            &plan_a,
            "---\nstatus: #DONE\ndateUpdated: 2026-09-15\n---\n\n# A\n",
        )
        .unwrap();
        std::fs::write(
            &plan_b,
            "---\nstatus: #UAT\ndateUpdated: 2026-09-15\n---\n\n# B\n",
        )
        .unwrap();

        let mut first = entry("b1", "a.md", "TODO", "DONE", "2026-01-01");
        first.path = plan_a.to_string_lossy().into_owned();
        let mut second = entry("b1", "b.md", "WIP", "DONE", "2026-02-02");
        second.path = plan_b.to_string_lossy().into_owned();
        append(&path, &[first, second]).unwrap();

        let report = revert_batches(&path, 1).unwrap();
        assert_eq!(report.batches, 1);
        assert_eq!(report.files, 2);
        assert!(report.errors.is_empty());

        let a = std::fs::read_to_string(&plan_a).unwrap();
        assert!(a.contains("status: TODO"));
        assert!(a.contains("dateUpdated: 2026-01-01"));
        let b = std::fs::read_to_string(&plan_b).unwrap();
        assert!(b.contains("status: WIP"));
        assert!(b.contains("dateUpdated: 2026-02-02"));

        // A second undo has nothing left to revert.
        let again = revert_batches(&path, 1).unwrap();
        assert_eq!(again.files, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revert_of_n_batches_walks_back_in_order() {
        let path = temp_path("revert-n");
        let dir = path.parent().unwrap().to_path_buf();
        let plan = dir.join("a.md");
        std::fs::write(
            &plan,
            "---\nstatus: #DONE\ndateUpdated: 2026-09-15\n---\n\n# A\n",
        )
        .unwrap();

        let mut older = entry("b1", "a.md", "TODO", "WIP", "2026-01-01");
        older.path = plan.to_string_lossy().into_owned();
        let mut newer = entry("b2", "a.md", "WIP", "DONE", "2026-03-03");
        newer.path = plan.to_string_lossy().into_owned();
        append(&path, &[older, newer]).unwrap();

        let report = revert_batches(&path, 2).unwrap();
        assert_eq!(report.batches, 2);
        assert_eq!(report.files, 2);
        // The oldest batch wins, because both touch the same file.
        let text = std::fs::read_to_string(&plan).unwrap();
        assert!(text.contains("status: TODO"));
        assert!(text.contains("dateUpdated: 2026-01-01"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
