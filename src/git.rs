//! Read-only git status per plan.
//!
//! Git is consulted only when asked for (`--git`, `git = true`, `g`), because it
//! spawns a process per repo. Each repo is asked once
//! (`git status --porcelain=v1 -z --untracked-files=all`), the repos run in
//! parallel, and the answer is cached for the process lifetime. Nothing here
//! ever stages, commits or otherwise writes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::plan::Plan;

/// How long any one `git status` may take before it is abandoned.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// What git thinks of a plan file. `Unknown` is a repo git could not be asked
/// about (no repo, no git, a timeout), and is deliberately distinct from
/// `Clean`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitStatus {
    Clean,
    Modified,
    Untracked,
    Unknown,
}

impl GitStatus {
    /// Stable machine key for `--json`.
    pub fn label(self) -> &'static str {
        match self {
            GitStatus::Clean => "clean",
            GitStatus::Modified => "modified",
            GitStatus::Untracked => "untracked",
            GitStatus::Unknown => "unknown",
        }
    }

    /// The one-character row marker. Clean and unknown stay blank.
    pub fn marker(self) -> &'static str {
        match self {
            GitStatus::Modified => "M",
            GitStatus::Untracked => "?",
            GitStatus::Clean | GitStatus::Unknown => " ",
        }
    }
}

/// Per-plan git status for one run, keyed by absolute plan path.
#[derive(Debug, Default, Clone)]
pub struct GitIndex {
    statuses: HashMap<PathBuf, GitStatus>,
}

impl GitIndex {
    /// Ask git about every repo the plans live in, in parallel.
    pub fn build(plans: &[Plan]) -> GitIndex {
        let repos: Vec<PathBuf> = plans
            .iter()
            .map(|plan| plan.repo_root.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let per_repo: Vec<(PathBuf, Option<HashMap<String, GitStatus>>)> = repos
            .into_par_iter()
            .map(|root| {
                let map = status_for_repo(&root);
                (root, map)
            })
            .collect();
        let per_repo: HashMap<PathBuf, Option<HashMap<String, GitStatus>>> =
            per_repo.into_iter().collect();

        let mut statuses = HashMap::new();
        for plan in plans {
            let status = match per_repo.get(&plan.repo_root) {
                Some(Some(map)) => map.get(&plan.rel_path).copied().unwrap_or(GitStatus::Clean),
                _ => GitStatus::Unknown,
            };
            statuses.insert(plan.path.clone(), status);
        }
        GitIndex { statuses }
    }

    /// The status of one plan; `Unknown` for a plan not in the index.
    pub fn status(&self, plan: &Plan) -> GitStatus {
        self.statuses
            .get(&plan.path)
            .copied()
            .unwrap_or(GitStatus::Unknown)
    }
}

/// Run `git status` once in `root` and parse it. `None` means git could not be
/// asked (not a repo, missing binary, non-zero exit, timeout).
fn status_for_repo(root: &Path) -> Option<HashMap<String, GitStatus>> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Read stdout on its own thread so a full pipe cannot deadlock the wait,
    // and so the main thread can enforce the deadline on `try_wait`.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + GIT_TIMEOUT;
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?;
    if !success {
        return None;
    }
    Some(parse_porcelain_z(&bytes))
}

/// Parse `git status --porcelain=v1 -z` into `path -> status`, with paths
/// relative to the repo root.
///
/// Every record is `XY <path>` followed by NUL. A rename or copy record carries
/// the original path as one extra NUL-terminated field, which is skipped.
pub fn parse_porcelain_z(bytes: &[u8]) -> HashMap<String, GitStatus> {
    let mut map = HashMap::new();
    let mut fields = bytes.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            // Not a full `XY path` record (trailing empty field, junk).
            continue;
        }
        let x = field[0];
        let y = field[1];
        let path = String::from_utf8_lossy(&field[3..]).into_owned();
        let status = if x == b'?' && y == b'?' {
            GitStatus::Untracked
        } else {
            GitStatus::Modified
        };
        map.insert(path, status);
        // `R`/`C` records append the original path in the next field.
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            let _ = fields.next();
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_z_parses_modified_untracked_and_renames() {
        // ` M docs/plans/a.md\0?? docs/plans/new.md\0R  docs/plans/b.md\0docs/plans/old.md\0`
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b" M docs/plans/a.md\0");
        bytes.extend_from_slice(b"?? docs/plans/new.md\0");
        bytes.extend_from_slice(b"R  docs/plans/b.md\0docs/plans/old.md\0");
        // A staged-only change and a deleted file are both "modified".
        bytes.extend_from_slice(b"M  docs/plans/c.md\0");
        bytes.extend_from_slice(b" D docs/plans/gone.md\0");

        let map = parse_porcelain_z(&bytes);
        assert_eq!(map.get("docs/plans/a.md"), Some(&GitStatus::Modified));
        assert_eq!(map.get("docs/plans/new.md"), Some(&GitStatus::Untracked));
        assert_eq!(map.get("docs/plans/b.md"), Some(&GitStatus::Modified));
        // The rename's original path must not become a bogus entry.
        assert!(!map.contains_key("docs/plans/old.md"));
        assert_eq!(map.get("docs/plans/c.md"), Some(&GitStatus::Modified));
        assert_eq!(map.get("docs/plans/gone.md"), Some(&GitStatus::Modified));
    }

    #[test]
    fn porcelain_z_tolerates_empty_and_short_fields() {
        assert!(parse_porcelain_z(b"").is_empty());
        assert!(parse_porcelain_z(b"\0\0").is_empty());
        assert!(parse_porcelain_z(b"??\0").is_empty());
    }

    #[test]
    fn unknown_is_not_clean() {
        assert_ne!(GitStatus::Unknown, GitStatus::Clean);
        assert_eq!(GitStatus::Modified.marker(), "M");
        assert_eq!(GitStatus::Untracked.marker(), "?");
        assert_eq!(GitStatus::Clean.marker(), " ");
        assert_eq!(GitStatus::Unknown.label(), "unknown");
    }
}
