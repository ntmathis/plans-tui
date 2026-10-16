//! Find plans directories, walk them, parse in parallel.
//!
//! The cold path is deliberately boring: one parallel directory walk, then a
//! rayon parse of the ~768 files it found. Nothing is cached between runs.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ignore::{DirEntry, WalkBuilder, WalkState};
use rayon::prelude::*;

use crate::plan::{MAX_PLAN_BYTES, Plan};
use crate::repo;

/// Depth is counted from the scan root, so this has to clear the deepest real
/// layout (`scratch/.worktrees/<wt>/…/docs/superpowers/plans/x.md`).
const MAX_WALK_DEPTH: usize = 16;

/// Directory names we never descend into. The first block is the important
/// one (dependency/build trees that would otherwise dominate the walk); the
/// second block is extra ballast pruned to keep a cold scan well under a
/// second, and contains no plan layouts today.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "build",
    "DerivedData",
    "Pods",
    ".venv",
    "venv",
    "dist",
    ".next",
    "vendor",
    "Library",
    ".cache",
    // Extra ballast: `bower_components` alone is ~100k entries in sample-app.
    "bower_components",
    ".hg",
    ".svn",
    ".build",
    ".swiftpm",
    "Carthage",
    ".gradle",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".turbo",
    ".parcel-cache",
    "coverage",
    ".dart_tool",
    ".expo",
    ".terraform",
    ".nx",
];

/// Hidden directories worth descending into. Everything else that starts with
/// a dot is treated as ballast (`.git`, `.hg`, `.build`, `.idea`, …); these are
/// the only hidden layouts that hold plans today.
const HIDDEN_DIRS: &[&str] = &[".claude", ".opencode", ".worktrees", ".plans"];

/// Ancestor directory names (compared case-insensitively) that mark a plan
/// tree. `plans` is the house convention; the rest are the shapes other repos
/// use for the same thing.
const PLAN_DIR_NAMES: &[&str] = &["plans", "plan", "planning", "_plans", ".plans"];

/// Why a file was treated as a plan. Kept internal: the JSON wire format is
/// unchanged and the TUI never shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchReason {
    /// Tier 1: an ancestor directory is named like a plan tree.
    PlansDir,
    /// Tier 2: the filename is `YYYY-MM-DD-…md`.
    DatedName,
    /// Tier 3: the first bytes are frontmatter with a plan status or tag.
    Frontmatter,
}

impl MatchReason {
    /// Short label used by `--scan-report`.
    pub fn label(self) -> &'static str {
        match self {
            MatchReason::PlansDir => "dir",
            MatchReason::DatedName => "dated",
            MatchReason::Frontmatter => "frontmatter",
        }
    }
}

/// A file that survived the walk, before it is parsed into a [`Plan`].
struct Candidate {
    path: PathBuf,
    root: PathBuf,
    reason: MatchReason,
}

/// The walk's output: what to parse, and what `exclude` dropped.
#[derive(Default)]
struct Walked {
    candidates: Vec<Candidate>,
    excluded: Vec<PathBuf>,
}

/// `exclude` patterns from the config, set once from `main`. The TUI's rescan
/// path (`App`) has nowhere to thread them through, so `find_plans` reads them
/// from here, and a rescan honours the same list as the initial scan.
static EXCLUDE: OnceLock<Vec<String>> = OnceLock::new();

/// Record the config's `exclude` list. The first call wins; later calls are a
/// no-op.
pub fn set_exclude(patterns: Vec<String>) {
    let _ = EXCLUDE.set(patterns);
}

fn exclude_patterns() -> &'static [String] {
    EXCLUDE.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Every plan found under `roots`, parsed and deduplicated by canonical path.
pub fn find_plans(roots: &[PathBuf]) -> Vec<Plan> {
    find_plans_with_reasons(roots).plans
}

/// The scan result plus why each file matched and what was excluded, for
/// `--scan-report`.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub plans: Vec<Plan>,
    /// Match reason per plan, keyed by the plan's absolute path.
    pub reasons: HashMap<PathBuf, MatchReason>,
    /// Files the `exclude` list dropped, in scan order.
    pub excluded: Vec<PathBuf>,
}

pub fn find_plans_with_reasons(roots: &[PathBuf]) -> ScanResult {
    let roots: Vec<PathBuf> = roots.iter().map(|r| canonical_or_self(r)).collect();
    let walked = collect_plan_files(&roots, exclude_patterns());

    let parsed: Vec<(Plan, MatchReason)> = walked
        .candidates
        .par_iter()
        .filter_map(|candidate| {
            build_plan(&candidate.path, &candidate.root).map(|plan| (plan, candidate.reason))
        })
        .collect();

    let mut reasons = HashMap::with_capacity(parsed.len());
    let mut plans = Vec::with_capacity(parsed.len());
    for (plan, reason) in parsed {
        reasons.insert(plan.path.clone(), reason);
        plans.push(plan);
    }

    mark_duplicates(&mut plans);

    plans.sort_by(|a, b| {
        a.status
            .rank()
            .cmp(&b.status.rank())
            .then_with(|| a.repo_name.cmp(&b.repo_name))
            .then_with(|| b.display_date().cmp(&a.display_date()))
            .then_with(|| a.slug.cmp(&b.slug))
    });
    ScanResult {
        plans,
        reasons,
        excluded: walked.excluded,
    }
}

/// Walk `roots` once (in parallel) and collect every markdown file that any of
/// the three match tiers claims, minus anything `patterns` excludes.
fn collect_plan_files(roots: &[PathBuf], patterns: &[String]) -> Walked {
    let walked: Mutex<Walked> = Mutex::new(Walked::default());

    for root in roots {
        let root = root.clone();
        let walked = &walked;
        WalkBuilder::new(&root)
            .follow_links(false)
            // We *do* want `.claude/plans` and `.opencode/plans`.
            .hidden(false)
            // Plans inside a gitignored `agent_workspace/` still count.
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false)
            .max_depth(Some(MAX_WALK_DEPTH))
            .filter_entry(keep_entry)
            .build_parallel()
            .run(|| {
                let root = root.clone();
                Box::new(move |result| {
                    if let Ok(entry) = result {
                        let path = entry.path();
                        if entry.file_type().is_some_and(|t| t.is_file()) && is_plan_file(path) {
                            let len = entry.metadata().ok().map(|meta| meta.len());
                            if let Some(reason) = plan_match(path, len) {
                                let relative =
                                    path.strip_prefix(&root).unwrap_or(path).to_string_lossy();
                                let mut walked = walked.lock().expect("plan collector poisoned");
                                if is_excluded(&relative, patterns) {
                                    walked.excluded.push(path.to_path_buf());
                                } else {
                                    walked.candidates.push(Candidate {
                                        path: path.to_path_buf(),
                                        root: root.clone(),
                                        reason,
                                    });
                                }
                            }
                        }
                    }
                    WalkState::Continue
                })
            });
    }

    let mut walked = walked.into_inner().expect("plan collector poisoned");

    // Symlinked repos can hand us the same file twice.
    let mut seen = HashSet::new();
    walked.candidates.retain(|candidate| {
        let key = candidate
            .path
            .canonicalize()
            .unwrap_or_else(|_| candidate.path.clone());
        seen.insert(key)
    });
    walked.candidates.sort_by(|a, b| a.path.cmp(&b.path));
    walked
}

/// `exclude` is a plain substring test on the path relative to the scan root.
/// Deliberately dumb: no glob crate, no surprises about `**`.
fn is_excluded(relative: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| !pattern.is_empty() && relative.contains(pattern.as_str()))
}

fn build_plan(path: &Path, root: &Path) -> Option<Plan> {
    let (repo_root, repo_name) = repo::repo_for(path, root);
    let rel_path = path
        .strip_prefix(&repo_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    Plan::from_path(path, &repo_root, &repo_name, &rel_path).ok()
}

/// Directory names that mean "a copy of somebody else's checkout": a plan
/// under one of these is never the canonical copy of a duplicate group.
const VENDORED_COMPONENTS: &[&str] = &[
    ".worktrees",
    "worktrees",
    "z-backup",
    "bower_components",
    "backup",
];

/// The vendored `free-claude-code/agent_workspace` path is two components.
const VENDORED_PAIR: (&str, &str) = ("free-claude-code", "agent_workspace");

/// Group byte-identical plans and point every copy at its canonical twin.
///
/// The canonical copy is the one that is not vendored, then the one with the
/// shortest repo root, then repo name, then path — deterministic without any
/// fixtures. Files with no twin keep `duplicate_of: None`.
pub fn mark_duplicates(plans: &mut [Plan]) {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, plan) in plans.iter().enumerate() {
        groups
            .entry(plan.content_hash.clone())
            .or_default()
            .push(index);
    }

    let mut assignments: Vec<(usize, PathBuf)> = Vec::new();
    for indices in groups.values() {
        if indices.len() < 2 {
            continue;
        }
        let canonical = *indices
            .iter()
            .min_by(|a, b| canonical_cmp(&plans[**a], &plans[**b]))
            .expect("non-empty group");
        for &index in indices {
            if index != canonical {
                assignments.push((index, plans[canonical].path.clone()));
            }
        }
    }
    for (index, canonical) in assignments {
        plans[index].duplicate_of = Some(canonical);
    }
}

/// Ordering that puts the canonical copy first.
fn canonical_cmp(a: &Plan, b: &Plan) -> Ordering {
    is_vendored(&a.path)
        .cmp(&is_vendored(&b.path))
        .then_with(|| {
            a.repo_root
                .as_os_str()
                .len()
                .cmp(&b.repo_root.as_os_str().len())
        })
        .then_with(|| a.repo_name.cmp(&b.repo_name))
        .then_with(|| a.path.cmp(&b.path))
}

/// Is this path inside a vendored or worktree checkout?
fn is_vendored(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts
        .iter()
        .any(|part| VENDORED_COMPONENTS.contains(&part.as_str()))
    {
        return true;
    }
    parts
        .windows(2)
        .any(|pair| pair[0] == VENDORED_PAIR.0 && pair[1] == VENDORED_PAIR.1)
}

/// Prune the walk: heavy/build dirs and unknown hidden dirs. Every non-README
/// markdown file is left for the closure, which runs the three match tiers in
/// parallel (tier 3 needs a read, so it does not belong in this serial test).
fn keep_entry(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    let is_dir = entry.file_type().is_some_and(|t| t.is_dir());

    if !is_dir {
        return is_plan_file(entry.path());
    }
    if SKIP_DIRS.iter().any(|skip| name == *skip) {
        return false;
    }
    if name.starts_with('.') && !HIDDEN_DIRS.iter().any(|hidden| name == *hidden) {
        return false;
    }
    true
}

/// Why this file is a plan, if it is one. Tiers are checked cheap-to-costly;
/// the first hit wins, and `len` is the file size the walk already knows, so
/// an over-cap file is rejected without opening it.
fn plan_match(path: &Path, len: Option<u64>) -> Option<MatchReason> {
    if under_plans_dir(path) {
        return Some(MatchReason::PlansDir);
    }
    if dated_name(path) {
        return Some(MatchReason::DatedName);
    }
    if len.is_some_and(|len| len > MAX_PLAN_BYTES) {
        return None;
    }
    sniff_frontmatter(path).then_some(MatchReason::Frontmatter)
}

/// A filename shaped like `YYYY-MM-DD-<something>.md`. Hand-rolled rather than
/// regex'd: the house naming convention, so a dated markdown file anywhere in
/// a repo is a plan by intent. Requires the full `-` after the day and at least
/// one character before the extension, so `2026-09-22-.md` does not match.
fn dated_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let bytes = name.as_bytes();
    // `YYYY-MM-DD-` is 11 bytes; the stem needs one more and `.md` three.
    if bytes.len() <= 14 {
        return false;
    }
    let digit = |index: usize| bytes[index].is_ascii_digit();
    digit(0)
        && digit(1)
        && digit(2)
        && digit(3)
        && bytes[4] == b'-'
        && digit(5)
        && digit(6)
        && bytes[7] == b'-'
        && digit(8)
        && digit(9)
        && bytes[10] == b'-'
        && name.to_ascii_lowercase().ends_with(".md")
}

/// How much of a file's head tier 3 looks at. The frontmatter we care about is
/// always at the top; a `status:` or `tags: plan` line that sits past this is
/// not a convention we are chasing.
const SNIFF_BYTES: usize = 512;

/// Tier 3: does this file open with frontmatter that looks like a plan's?
/// Reads at most [`SNIFF_BYTES`] bytes and never the whole file. Files over the
/// plan size cap are rejected without a read, since [`Plan::from_path`] would
/// refuse them anyway.
fn sniff_frontmatter(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    if file
        .metadata()
        .is_ok_and(|meta| meta.len() > MAX_PLAN_BYTES)
    {
        return false;
    }
    let mut buf = [0u8; SNIFF_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(_) => return false,
        }
    }
    frontmatter_sniff(&buf[..filled])
}

/// The pure half of [`sniff_frontmatter`]: accept a `---` fenced header that
/// carries a lifecycle `status:` or a `plan` tag. Case-insensitive; the `#` in
/// `status: #WIP` is optional, because serde_yaml reads it as a comment anyway.
fn frontmatter_sniff(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return false;
    };

    // The header runs to the closing fence. If the fence did not make it into
    // the prefix, the whole prefix is header.
    let mut header = rest;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            header = &rest[..offset];
            break;
        }
        offset += line.len();
    }

    header
        .lines()
        .any(|line| status_line_matches(line) || tags_line_mentions_plan(line))
}

fn status_line_matches(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("status:") else {
        return false;
    };
    let value = rest.trim().trim_matches(['"', '\'']);
    let value = value.trim_start_matches('#').trim();
    matches!(
        value.to_ascii_uppercase().as_str(),
        "TODO" | "WIP" | "UAT" | "DONE" | "DROPPED"
    )
}

fn tags_line_mentions_plan(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("tags:") else {
        return false;
    };
    rest.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token.eq_ignore_ascii_case("plan") || token.eq_ignore_ascii_case("plans"))
}

fn is_plan_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.eq_ignore_ascii_case("readme.md") {
        return false;
    }
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// The original, pre-broadening tier-1 rule: an ancestor directory named
/// exactly `plans` (case-sensitive). `--scan-report` uses this to separate the
/// plans the old rule already found from the ones the broader rules added, so
/// `docs/planning/` and `Plans/` show up as new while `docs/plans/` does not.
pub fn under_exact_plans_dir(path: &Path) -> bool {
    ancestor_dir_matches(path, |name| name == "plans")
}

/// Any ancestor directory is named like a plan tree (see [`PLAN_DIR_NAMES`]).
/// Case-insensitive so `Plans/`, `PLANS/` and `.Plans/` all count.
fn under_plans_dir(path: &Path) -> bool {
    ancestor_dir_matches(path, is_plan_dir_name)
}

/// Walk a path's ancestors, inclusive of its immediate parent, and return true
/// when any directory name satisfies `predicate`.
fn ancestor_dir_matches(path: &Path, predicate: impl Fn(&std::ffi::OsStr) -> bool) -> bool {
    path.parent().is_some_and(|mut dir| {
        loop {
            if dir.file_name().is_some_and(&predicate) {
                return true;
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => return false,
            }
        }
    })
}

fn is_plan_dir_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        PLAN_DIR_NAMES
            .iter()
            .any(|plan| name.eq_ignore_ascii_case(plan))
    })
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Where `plans` looks by default.
pub fn default_root() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let src = home.join("src");
    if src.is_dir() { src } else { home }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Status;

    fn fake(path: &str, repo_root: &str, hash: &str) -> Plan {
        Plan {
            path: PathBuf::from(path),
            repo_root: PathBuf::from(repo_root),
            repo_name: Path::new(repo_root)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            rel_path: "docs/plans/a.md".to_string(),
            slug: "a".to_string(),
            title: "A".to_string(),
            status: Status::Wip,
            status_raw: None,
            status_inferred: false,
            date_created: None,
            date_updated: None,
            tags: Vec::new(),
            owner: None,
            has_handoff: false,
            has_frontmatter: true,
            content_hash: hash.to_string(),
            duplicate_of: None,
        }
    }

    #[test]
    fn canonical_copy_prefers_non_vendored_then_shortest_root() {
        let mut plans = vec![
            fake(
                "/src/sample-repo/free-claude-code/agent_workspace/plans/a.md",
                "/src/sample-repo",
                "same",
            ),
            fake(
                "/src/sample-site/.worktrees/x/docs/plans/a.md",
                "/src/sample-site",
                "same",
            ),
            fake("/src/dotfiles/docs/plans/a.md", "/src/dotfiles", "same"),
            fake("/src/scratch/plans/a.md", "/src/scratch", "same"),
        ];
        mark_duplicates(&mut plans);

        let canonicals: Vec<&Plan> = plans.iter().filter(|plan| plan.is_canonical()).collect();
        assert_eq!(canonicals.len(), 1, "exactly one canonical copy");
        // scratch (12 chars) beats dotfiles (13); both beat the two vendored
        // paths regardless of length.
        assert_eq!(canonicals[0].path, PathBuf::from("/src/scratch/plans/a.md"));
        for plan in plans.iter().filter(|plan| !plan.is_canonical()) {
            assert_eq!(
                plan.duplicate_of.as_deref(),
                Some(Path::new("/src/scratch/plans/a.md"))
            );
        }
    }

    #[test]
    fn ties_break_on_repo_name_then_path() {
        // Equal repo_root lengths, so repo_name decides.
        let mut plans = vec![
            fake("/src/bbbb/plans/a.md", "/src/bbbb", "h"),
            fake("/src/aaaa/plans/a.md", "/src/aaaa", "h"),
        ];
        mark_duplicates(&mut plans);
        let canonical = plans.iter().find(|plan| plan.is_canonical()).unwrap();
        assert_eq!(canonical.repo_name, "aaaa");
    }

    #[test]
    fn different_content_is_never_a_duplicate() {
        let mut plans = vec![
            fake("/src/a/plans/a.md", "/src/a", "one"),
            fake("/src/b/plans/a.md", "/src/b", "two"),
        ];
        mark_duplicates(&mut plans);
        assert!(plans.iter().all(|plan| plan.is_canonical()));
    }

    #[test]
    fn vendored_paths_are_recognised() {
        for path in [
            "/src/x/free-claude-code/agent_workspace/plans/a.md",
            "/src/x/.worktrees/wt/docs/plans/a.md",
            "/src/x/.claude/worktrees/wt/docs/plans/a.md",
            "/src/x/z-backup/plans/a.md",
            "/src/x/backup/plans/a.md",
            "/src/x/bower_components/plans/a.md",
        ] {
            assert!(is_vendored(Path::new(path)), "{path}");
        }
        assert!(!is_vendored(Path::new("/src/x/docs/plans/a.md")));
    }

    #[test]
    fn plans_dirs_of_every_shape_are_recognised() {
        for path in [
            "/src/dotfiles/docs/plans/x.md",
            "/src/scratch/plans/y.md",
            "/src/a/notes-vault/plans/z.md",
            "/src/a/.claude/plans/w.md",
            "/src/a/.opencode/plans/v.md",
            "/src/a/free-claude-code/agent_workspace/plans/u.md",
            "/src/a/docs/plans/nested/deep/t.md",
        ] {
            assert!(under_plans_dir(Path::new(path)), "{path}");
        }
        assert!(!under_plans_dir(Path::new("/src/a/docs/superpowers/x.md")));
        assert!(!under_plans_dir(Path::new("/src/a/plans.md")));
    }

    #[test]
    fn plan_dir_name_variants_are_recognised() {
        for dir in ["plans", "plan", "planning", "_plans", ".plans"] {
            let path = format!("/src/a/docs/{dir}/x.md");
            assert!(under_plans_dir(Path::new(&path)), "{path}");
        }
    }

    #[test]
    fn plan_dir_name_matching_is_case_insensitive() {
        for path in [
            "/src/a/Plans/x.md",
            "/src/a/PLANS/x.md",
            "/src/a/docs/Planning/x.md",
            "/src/a/PLAN/x.md",
            "/src/a/.Plans/x.md",
        ] {
            assert!(under_plans_dir(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn exact_plans_dir_is_the_legacy_subset() {
        // The old rule, verbatim: only a literal `plans` ancestor.
        assert!(under_exact_plans_dir(Path::new("/src/a/docs/plans/x.md")));
        assert!(!under_exact_plans_dir(Path::new(
            "/src/a/docs/planning/x.md"
        )));
        assert!(!under_exact_plans_dir(Path::new("/src/a/Plans/x.md")));
        assert!(!under_exact_plans_dir(Path::new("/src/a/.plans/x.md")));
        // Every exact match is still a broad match.
        assert!(under_plans_dir(Path::new("/src/a/docs/plans/x.md")));
    }

    #[test]
    fn near_miss_directory_names_do_not_match() {
        for path in [
            "/src/plants/x.md",
            "/src/planner-app/src/x.md",
            "/src/a/docs/planning-notes/x.md",
            "/src/a/_plan/x.md",
            "/src/a/plans-archive/x.md",
        ] {
            assert!(!under_plans_dir(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn dated_filenames_are_recognised() {
        for path in [
            "/src/a/docs/2026-09-22-foo.md",
            "/src/a/NOTES/2026-01-01-x.md",
            "/src/a/2026-12-31-z.MD",
        ] {
            assert!(dated_name(Path::new(path)), "{path}");
        }
        for path in [
            "/src/a/docs/2026-9-2-foo.md",
            "/src/a/docs/2026-09-22-.md",
            "/src/a/docs/20260922-foo.md",
            "/src/a/docs/notes.md",
            "/src/a/docs/2026-09-22-foo.markdown",
            "/src/a/docs/2026-09-22-foo.txt",
        ] {
            assert!(!dated_name(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn frontmatter_sniff_accepts_lifecycle_status() {
        for status in ["#TODO", "WIP", "#uat", "done", "Dropped", "'#WIP'"] {
            let text = format!("---\ndateCreated: 2026-09-22\nstatus: {status}\n---\n# Title\n");
            assert!(frontmatter_sniff(text.as_bytes()), "{status}");
        }
    }

    #[test]
    fn frontmatter_sniff_rejects_non_plan_headers() {
        for text in [
            "# Just a note\nstatus: #TODO\n",
            "---\ntitle: not a plan\n---\nbody\n",
            "---\nstatus: ready\n---\nbody\n",
            "---\ntags: [notes, scratch]\n---\nbody\n",
            "",
        ] {
            assert!(!frontmatter_sniff(text.as_bytes()), "{text:?}");
        }
    }

    #[test]
    fn frontmatter_sniff_accepts_plan_tags_and_bom() {
        for text in [
            "---\ntags: [plan, rust]\n---\nbody\n",
            "---\ntags: [plans-tui, plan]\n---\nbody\n",
            "\u{feff}---\ntags: [PLAN]\n---\nbody\n",
            "---\r\ntags: plans\r\n---\r\nbody\r\n",
        ] {
            assert!(frontmatter_sniff(text.as_bytes()), "{text:?}");
        }
    }

    #[test]
    fn frontmatter_sniff_only_sees_the_bounded_prefix() {
        let mut text = String::from("---\n");
        while text.len() < SNIFF_BYTES {
            text.push_str("# padding line\n");
        }
        text.push_str("tags: [plan]\n---\n");
        assert!(frontmatter_sniff(text.as_bytes()));
        assert!(!frontmatter_sniff(&text.as_bytes()[..SNIFF_BYTES]));
        assert_eq!(SNIFF_BYTES, 512);
    }

    #[test]
    fn sniff_frontmatter_reads_a_real_file() {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(format!(
            "plans-sniff-{}-{:?}.md",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut file = File::create(&path).expect("temp file");
        file.write_all(b"---\nstatus: #UAT\n---\n# Scratch\n")
            .expect("write");
        drop(file);
        assert!(sniff_frontmatter(&path));
        std::fs::remove_file(&path).expect("cleanup");
        assert!(!sniff_frontmatter(&path), "a missing file is not a plan");
    }

    #[test]
    fn plan_match_reports_each_tier() {
        assert_eq!(
            plan_match(Path::new("/src/a/plans/x.md"), None),
            Some(MatchReason::PlansDir)
        );
        assert_eq!(
            plan_match(Path::new("/src/a/NOTES/2026-09-01-retry.md"), None),
            Some(MatchReason::DatedName)
        );
        // A dated name wins even outside a plan dir, and even inside a miss.
        assert_eq!(
            plan_match(Path::new("/src/plants/2026-09-01-x.md"), None),
            Some(MatchReason::DatedName)
        );
        // A missing file has no frontmatter to sniff.
        assert_eq!(
            plan_match(Path::new("/src/a/docs/undated-notes.md"), None),
            None
        );
        // Over-cap files are rejected before the read.
        assert_eq!(
            plan_match(
                Path::new("/src/a/docs/undated-notes.md"),
                Some(MAX_PLAN_BYTES + 1)
            ),
            None
        );
        assert_eq!(MatchReason::PlansDir.label(), "dir");
        assert_eq!(MatchReason::DatedName.label(), "dated");
        assert_eq!(MatchReason::Frontmatter.label(), "frontmatter");
    }

    #[test]
    fn plan_match_sniffs_undated_frontmatter_files() {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(format!(
            "plans-match-{}-{:?}.md",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut file = File::create(&path).expect("temp file");
        file.write_all(b"---\ntags: [plan]\n---\n# Scratch\n")
            .expect("write");
        drop(file);
        assert_eq!(plan_match(&path, None), Some(MatchReason::Frontmatter));
        assert_eq!(plan_match(&path, Some(MAX_PLAN_BYTES + 1)), None);
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn find_plans_with_reasons_labels_each_tier() {
        let root = std::env::temp_dir().join(format!(
            "plans-scan-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let repo = root.join("scratchrepo");
        std::fs::create_dir_all(repo.join("docs/plans")).expect("dirs");
        std::fs::create_dir_all(repo.join("NOTES")).expect("dirs");
        std::fs::write(repo.join("docs/plans/a.md"), "# A\n").expect("write");
        std::fs::write(repo.join("NOTES/2026-09-01-b.md"), "# B\n").expect("write");
        std::fs::write(repo.join("docs/c.md"), "---\nstatus: #WIP\n---\n# C\n").expect("write");
        std::fs::write(repo.join("docs/d.md"), "# D\n").expect("write");

        let result = find_plans_with_reasons(std::slice::from_ref(&root));
        let labels: HashMap<String, &str> = result
            .reasons
            .iter()
            .map(|(path, reason)| {
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    reason.label(),
                )
            })
            .collect();
        assert_eq!(result.plans.len(), 3, "plain markdown is not a plan");
        assert_eq!(labels.get("a.md"), Some(&"dir"));
        assert_eq!(labels.get("2026-09-01-b.md"), Some(&"dated"));
        assert_eq!(labels.get("c.md"), Some(&"frontmatter"));
        assert!(!labels.contains_key("d.md"));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn exclude_is_a_substring_test_on_the_relative_path() {
        let patterns = vec!["node_modules".to_string(), "docs/adr".to_string()];
        assert!(is_excluded("web/node_modules/pkg/x.md", &patterns));
        assert!(is_excluded("repo/docs/adr/0001.md", &patterns));
        // Not a glob: `**` is just two characters, so a glob-shaped pattern
        // matches nothing. Use the literal substring (`node_modules`).
        assert!(!is_excluded(
            "web/node_modules/pkg/x.md",
            &["**/node_modules/**".to_string()]
        ));
        assert!(!is_excluded(
            "repo/docs/plans/x.md",
            &["**/plans/**".to_string()]
        ));
        assert!(!is_excluded("repo/docs/plans/x.md", &patterns));
        assert!(!is_excluded("repo/docs/plans/x.md", &[]));
        assert!(!is_excluded("repo/docs/plans/x.md", &[String::new()]));
    }

    #[test]
    fn collect_plan_files_drops_excluded_paths() {
        let root = std::env::temp_dir().join(format!(
            "plans-exclude-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let repo = root.join("scratchrepo");
        std::fs::create_dir_all(repo.join("docs/plans")).expect("dirs");
        std::fs::create_dir_all(repo.join("docs/adr")).expect("dirs");
        std::fs::write(repo.join("docs/plans/a.md"), "# A\n").expect("write");
        std::fs::write(repo.join("docs/adr/2026-09-01-note.md"), "# Note\n").expect("write");

        let all = collect_plan_files(std::slice::from_ref(&root), &[]);
        assert_eq!(all.candidates.len(), 2);
        assert!(all.excluded.is_empty());

        let filtered = collect_plan_files(std::slice::from_ref(&root), &["docs/adr".to_string()]);
        assert_eq!(filtered.candidates.len(), 1);
        assert_eq!(filtered.excluded.len(), 1);
        assert!(filtered.excluded[0].ends_with("docs/adr/2026-09-01-note.md"));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn readme_and_non_markdown_are_not_plans() {
        assert!(!is_plan_file(Path::new("/src/a/plans/README.md")));
        assert!(!is_plan_file(Path::new("/src/a/plans/notes.txt")));
        assert!(is_plan_file(Path::new("/src/a/plans/x.md")));
    }

    #[test]
    fn skip_list_covers_the_heavy_trees() {
        assert!(SKIP_DIRS.contains(&"node_modules"));
        assert!(SKIP_DIRS.contains(&"bower_components"));
        assert!(SKIP_DIRS.contains(&".git"));
        assert!(SKIP_DIRS.contains(&"DerivedData"));
        // `.claude/plugins/cache` holds real vendored plans; `cache` must not
        // be in the skip list.
        assert!(!SKIP_DIRS.contains(&"cache"));
        assert!(HIDDEN_DIRS.contains(&".claude"));
        assert!(HIDDEN_DIRS.contains(&".opencode"));
        assert!(HIDDEN_DIRS.contains(&".worktrees"));
        // `.plans` is a tier-1 name, so the walk has to descend into it.
        assert!(HIDDEN_DIRS.contains(&".plans"));
    }
}
