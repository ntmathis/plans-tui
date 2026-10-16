# plans-tui

A terminal dashboard for markdown plan files across all your repos, grouped by
their frontmatter status — built for agent workflows where one model plans and
another executes.

![plans-tui demo](demo/demo.gif)

## Install

```bash
cargo install --git https://github.com/ntmathis/plans-tui
```

The package is `plans-tui`; the binary it installs is `plans`, because the
`plans` name on crates.io belongs to an unrelated crate. Needs Rust 1.87+.

## The plan format

A plan is a markdown file under a `plans/` or `docs/plans/` directory, usually
named `YYYY-MM-DD-<slug>.md` (the date prefix is optional):

```markdown
---
dateCreated: 2026-10-15
dateUpdated: 2026-10-15
tags: [plan, notes-app]
owner: unassigned
status: #WIP
---

# Add offline full-text search

## Handoff Prompt

```
Rewrite the search path to hit a local index first.
Read this plan, then follow it step by step. Commit after each step.
```
```

Status is one of `#TODO`, `#WIP`, `#UAT`, `#DONE`, `#DROPPED`. The
`## Handoff Prompt` section is optional: `Y` synthesizes one from the plan's
location when it is missing. The full convention — including what each status
means and how plans move between them — is in
[AGENTS.md](https://github.com/ntmathis/dotfiles/blob/main/AGENTS.md).

## Usage

```
plans                       the TUI, scanning every plans/ dir under ~/src
plans --here                the current repo only
plans --list                grouped plain text instead of the TUI
plans --json                every matching plan, for scripts and agents
plans --handoff <slug>      print one handoff prompt, never touching the clipboard
plans --copy <slug>         the same, and copy it
plans --set-status <slug> <status>
plans --stale [days]        TODO/WIP/UAT older than the cutoff
plans --repo notes --status wip --since 2026-09-01 --has-handoff
plans --history / --undo    the status journal, and rolling it back
```

`plans --help` lists every flag and its default.

Keys in the TUI:

| Key | Action |
|-----|--------|
| `j` / `k`, arrows | move |
| `Tab` | toggle the detail pane |
| `/` | fuzzy filter — Enter commits, Esc clears |
| `1`–`5` | toggle TODO / WIP / UAT / DONE / DROPPED |
| `a` | show every status |
| `s` | cycle grouping: status → repo → week → none |
| `y` / `Y` | copy the handoff prompt / a synthesized one |
| `g` | toggle git status markers |
| `x` | hide duplicate copies |
| `?` / `q` | help / quit |

## Config

Optional, read from `~/.config/plans/config.toml` (or `$PLANS_CONFIG`, or
`--config PATH`). Every key there is a default — a CLI flag always wins. See
[`config.example.toml`](config.example.toml).

## License

MIT — see [LICENSE](LICENSE).
