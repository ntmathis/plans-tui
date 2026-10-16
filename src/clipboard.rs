//! Copy text to the macOS clipboard via `pbcopy`.

use std::io::Write;
use std::process::{Command, Stdio};

/// Copy `text` to the system clipboard.
///
/// Returns `false` when `pbcopy` is missing or fails so callers can fall back
/// to printing the text instead. Deliberately shells out rather than using a
/// clipboard crate: `arboard` drags in X11/Wayland and needs the process to
/// stay alive to own the selection, which is wrong for a TUI that exits.
pub fn copy(text: &str) -> bool {
    let Ok(mut child) = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    {
        let Some(stdin) = child.stdin.as_mut() else {
            return false;
        };
        if stdin.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    // Close stdin so pbcopy sees EOF and actually finishes.
    drop(child.stdin.take());

    child.wait().map(|s| s.success()).unwrap_or(false)
}
