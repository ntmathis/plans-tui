//! Map a plan path back to its repo root and display name.

use std::path::{Path, PathBuf};

/// The repo containing `plan_path`: the nearest ancestor with a `.git` entry,
/// never climbing above `scan_root`.
///
/// Falls back to the first directory under `scan_root` when the tree has no
/// git metadata at all, so a plan still lands somewhere sensible.
pub fn repo_for(plan_path: &Path, scan_root: &Path) -> (PathBuf, String) {
    let start = plan_path.parent().unwrap_or(plan_path);
    let mut first_under_root: Option<PathBuf> = None;
    let mut cur = Some(start);

    while let Some(dir) = cur {
        if first_under_root.is_none() && dir.parent() == Some(scan_root) {
            first_under_root = Some(dir.to_path_buf());
        }
        // `.git` is a directory in a normal clone and a file in a worktree or
        // submodule; `exists()` covers both.
        if dir.join(".git").exists() {
            return (dir.to_path_buf(), display_name(dir, scan_root));
        }
        if dir == scan_root {
            break;
        }
        cur = dir.parent();
    }

    match first_under_root {
        Some(dir) => {
            let name = display_name(&dir, scan_root);
            (dir, name)
        }
        None => (scan_root.to_path_buf(), display_name(scan_root, scan_root)),
    }
}

/// The current repo root, or the cwd when we are not inside one.
pub fn git_root_or_cwd() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut cur = Some(cwd.as_path());
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        cur = dir.parent();
    }
    cwd
}

/// `apps/notes`, `dotfiles`, `scratch` — the path of the repo
/// relative to the scan root, which is what the user actually recognises.
fn display_name(dir: &Path, scan_root: &Path) -> String {
    match dir.strip_prefix(scan_root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        _ => dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_is_relative_to_the_scan_root() {
        let root = Path::new("/src");
        assert_eq!(
            display_name(Path::new("/src/apps/notes"), root),
            "apps/notes"
        );
        assert_eq!(display_name(Path::new("/src/dotfiles"), root), "dotfiles");
    }

    #[test]
    fn display_name_falls_back_to_the_leaf_outside_the_root() {
        assert_eq!(
            display_name(Path::new("/elsewhere/foo"), Path::new("/src")),
            "foo"
        );
    }
}
