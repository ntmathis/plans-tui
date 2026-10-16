//! Ratatui rendering.

use std::time::Duration;

use chrono::Local;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, AppInit, DateFormat, GroupBy, RepoColumn, Row, contains, row_at};
use crate::clipboard;
use crate::git::GitStatus;
use crate::plan::{Plan, Status};

/// Lines the mouse wheel moves the list or the detail pane by.
const WHEEL_LINES: isize = 3;
/// Lines `PageUp`/`PageDown` move the detail pane by.
const PAGE_LINES: isize = 10;

// Column budget, in display cells.
const MARKER_W: usize = 2;
const STATUS_W: usize = 7;
const REPO_W: usize = 18;
const SLUG_W: usize = 32;
const GLYPH_W: usize = 2;
/// The one-character git marker column, shown only while git is on.
const GIT_W: usize = 1;
/// Above this width the title column appears; below it the repo column goes too.
const TITLE_MIN_WIDTH: usize = 70;
const REPO_MIN_WIDTH: usize = 58;

/// Launch the TUI. `init.mouse` toggles mouse reporting; `--no-mouse` turns it
/// off so the terminal keeps its own text selection.
pub fn run(init: AppInit) -> anyhow::Result<()> {
    let mouse = init.mouse;
    let mut terminal = ratatui::init();
    // ratatui::init() has already installed its restore-on-panic hook; ours
    // runs first and turns mouse reporting back off before it does.
    if mouse {
        install_mouse_panic_hook();
        enable_mouse()?;
    }

    let width = terminal.size()?.width;
    let mut app = App::new(init.width(width));
    let result = event_loop(&mut terminal, &mut app);

    if mouse {
        disable_mouse();
    }
    ratatui::restore();
    result
}

fn enable_mouse() -> anyhow::Result<()> {
    execute!(std::io::stdout(), EnableMouseCapture)?;
    Ok(())
}

fn disable_mouse() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
}

/// Leaving mouse reporting on after a panic makes the terminal unusable (and
/// breaks text selection), so switch it off before deferring.
fn install_mouse_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_mouse();
        previous(info);
    }));
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| draw(frame, app))?;
            dirty = false;
        }

        // Poll briefly while a toast is on screen so we can expire it; otherwise
        // sleep until the next input.
        let timeout = if app.toast.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(3600)
        };

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(app, key, terminal)? {
                        return Ok(());
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    if handle_mouse(app, mouse) {
                        dirty = true;
                    }
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }

        if app.expire_toast() {
            dirty = true;
        }
    }
}

/// Mouse reporting: wheel scrolls whatever is under the pointer, and a click
/// selects a row (or folds a group header). Returns true when a repaint is due.
fn handle_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollDown => {
            if contains(app.detail_area, mouse.column, mouse.row) {
                app.scroll_detail(WHEEL_LINES);
            } else {
                app.move_selection(WHEEL_LINES);
            }
            true
        }
        MouseEventKind::ScrollUp => {
            if contains(app.detail_area, mouse.column, mouse.row) {
                app.scroll_detail(-WHEEL_LINES);
            } else {
                app.move_selection(-WHEEL_LINES);
            }
            true
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(index) = row_at(app.list_area, app.list_offset, mouse.column, mouse.row)
            else {
                return false;
            };
            // Shift extends from the anchor, Ctrl/Cmd toggles one row, a plain
            // click selects (and folds a header). Terminals report Cmd as META
            // or SUPER inconsistently, so accept either.
            let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
            let toggle = mouse.modifiers.contains(KeyModifiers::CONTROL)
                || mouse.modifiers.contains(KeyModifiers::SUPER)
                || mouse.modifiers.contains(KeyModifiers::META);
            if shift {
                app.extend_selection_to_row(index);
            } else if toggle {
                app.toggle_plan_at_row(index);
            } else {
                app.select_row(index);
            }
            true
        }
        _ => false,
    }
}

/// Returns true when the app should quit.
fn handle_key(
    app: &mut App,
    key: KeyEvent,
    terminal: &mut DefaultTerminal,
) -> anyhow::Result<bool> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Ctrl-c quits from anywhere, including mid-filter.
    if ctrl && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    if app.help_open {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
        ) {
            app.help_open = false;
        }
        return Ok(false);
    }

    // The status modal owns the keyboard while it is open; nothing writes
    // until Enter is pressed inside it.
    if app.status_edit.is_some() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.cancel_status_edit(),
            KeyCode::Enter => app.confirm_status_edit(),
            KeyCode::Char('j') | KeyCode::Down => app.move_status_choice(1),
            KeyCode::Char('k') | KeyCode::Up => app.move_status_choice(-1),
            KeyCode::Char(c @ '1'..='5') => {
                app.set_status_choice(c as usize - '1' as usize);
            }
            _ => {}
        }
        return Ok(false);
    }

    if app.filtering {
        match key.code {
            KeyCode::Enter => app.filtering = false,
            KeyCode::Esc => {
                app.query.clear();
                app.filtering = false;
                app.rebuild();
            }
            KeyCode::Backspace => {
                app.query.pop();
                app.rebuild();
            }
            KeyCode::Char(c) => {
                app.query.push(c);
                app.rebuild();
            }
            _ => {}
        }
        return Ok(false);
    }

    if app.pending_z {
        app.pending_z = false;
        match key.code {
            KeyCode::Char('c') => app.set_collapsed(true),
            KeyCode::Char('o') => app.set_collapsed(false),
            KeyCode::Char('R') => app.expand_all(),
            KeyCode::Char('M') => app.collapse_all(),
            _ => {}
        }
        return Ok(false);
    }

    let page = terminal.size()?.height.saturating_sub(4);

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        // Esc clears a selection first and only quits when there is nothing to
        // clear. Ctrl-c still quits from anywhere (above).
        KeyCode::Esc => {
            if app.has_selection() {
                app.clear_selection();
            } else {
                return Ok(true);
            }
        }
        KeyCode::Char('v') => app.toggle_select_mode(),
        KeyCode::Char('J') | KeyCode::Down if shift => app.extend_selection_by(1),
        KeyCode::Char('K') | KeyCode::Up if shift => app.extend_selection_by(-1),
        KeyCode::Char('J') => app.extend_selection_by(1),
        KeyCode::Char('K') => app.extend_selection_by(-1),
        KeyCode::Char(' ') => app.toggle_cursor_plan(),
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('g') => app.toggle_git(),
        KeyCode::Home => {
            if app.select_mode {
                app.extend_selection_to_edge(false);
            } else {
                app.select_first();
            }
        }
        KeyCode::End => {
            if app.select_mode {
                app.extend_selection_to_edge(true);
            } else {
                app.select_last();
            }
        }
        KeyCode::Char('G') => {
            if app.select_mode {
                app.extend_selection_to_edge(true);
            } else {
                app.select_last();
            }
        }
        KeyCode::Char('d') if ctrl => app.half_page(page, true),
        KeyCode::Char('u') if ctrl => app.half_page(page, false),
        KeyCode::Char('u') => app.undo_last_batch(),
        KeyCode::Char('h') | KeyCode::Left => app.set_collapsed(true),
        KeyCode::Char('l') | KeyCode::Right => app.set_collapsed(false),
        // With the detail pane open, paging reads it; otherwise it pages the list.
        KeyCode::PageDown if app.detail_open => app.scroll_detail(PAGE_LINES),
        KeyCode::PageUp if app.detail_open => app.scroll_detail(-PAGE_LINES),
        KeyCode::PageDown => app.half_page(page, true),
        KeyCode::PageUp => app.half_page(page, false),
        KeyCode::Char('z') => app.pending_z = true,
        KeyCode::Tab => app.toggle_detail(),
        KeyCode::Char('/') => {
            // Start a new search rather than appending to the last one: there is
            // no visible caret, so refining an old query in place would confuse.
            app.query.clear();
            app.filtering = true;
            app.rebuild();
        }
        KeyCode::Char('1') => app.toggle_status(Status::Todo),
        KeyCode::Char('2') => app.toggle_status(Status::Wip),
        KeyCode::Char('3') => app.toggle_status(Status::Uat),
        KeyCode::Char('4') => app.toggle_status(Status::Done),
        KeyCode::Char('5') => app.toggle_status(Status::Dropped),
        KeyCode::Char('a') => app.show_all_statuses(),
        KeyCode::Char('s') => app.cycle_group(),
        KeyCode::Char('x') => app.toggle_duplicates(),
        KeyCode::Char('o') => app.cycle_sort(),
        KeyCode::Char('r') => app.reverse_sort(),
        KeyCode::Char('y') => copy_handoff(app),
        KeyCode::Char('Y') => copy_synthesized(app),
        KeyCode::Char('p') => copy_path(app),
        KeyCode::Char('e') => open_in_editor(app, terminal)?,
        KeyCode::Char('O') => open_externally(app),
        KeyCode::Char('R') => app.rescan(),
        // One-key "mark done": still confirm-gated, because it writes.
        KeyCode::Char('D') => app.begin_quick_done(),
        KeyCode::Char('S') => app.begin_status_edit(),
        KeyCode::Char('?') => app.help_open = true,
        _ => {}
    }
    Ok(false)
}

// ------------------------------------------------------------------ actions

/// `y`: the headline feature. A plan without a `## Handoff Prompt` toasts
/// rather than copying anything, because copying the whole body would be
/// garbage. With several plans selected, the prompts are concatenated behind a
/// `---` rule; plans with no prompt are reported by being absent from the
/// count.
fn copy_handoff(app: &mut App) {
    let targets = app.action_targets();
    if targets.is_empty() {
        app.toast("select a plan first");
        return;
    }
    let prompts: Vec<String> = targets.iter().filter_map(|i| app.handoff_for(*i)).collect();
    if prompts.is_empty() {
        app.toast(if targets.len() == 1 {
            "no Handoff Prompt in this plan — press Y to synthesize".to_string()
        } else {
            format!(
                "no Handoff Prompts in the {} selected plans — press Y to synthesize",
                targets.len()
            )
        });
        return;
    }
    let text = prompts.join("\n\n---\n\n");
    let noun = if prompts.len() == 1 {
        "handoff prompt".to_string()
    } else {
        format!("{} handoff prompts", prompts.len())
    };
    copy_text(app, &text, &noun);
}

/// `Y`: always available.
fn copy_synthesized(app: &mut App) {
    let targets = app.action_targets();
    if targets.is_empty() {
        app.toast("select a plan first");
        return;
    }
    let prompts: Vec<String> = targets
        .iter()
        .filter_map(|index| app.plans.get(*index))
        .map(Plan::synthesized_handoff)
        .collect();
    if prompts.is_empty() {
        return;
    }
    let text = prompts.join("\n\n---\n\n");
    let noun = if prompts.len() == 1 {
        "synthesized handoff prompt".to_string()
    } else {
        format!("{} synthesized handoff prompts", prompts.len())
    };
    copy_text(app, &text, &noun);
}

fn copy_path(app: &mut App) {
    let targets = app.action_targets();
    let paths: Vec<String> = targets
        .iter()
        .filter_map(|index| app.plans.get(*index))
        .map(|plan| plan.path.to_string_lossy().into_owned())
        .collect();
    if paths.is_empty() {
        app.toast("select a plan first");
        return;
    }
    let text = paths.join("\n");
    let noun = if paths.len() == 1 {
        "path".to_string()
    } else {
        format!("{} paths", paths.len())
    };
    copy_text(app, &text, &noun);
}

fn copy_text(app: &mut App, text: &str, noun: &str) {
    if clipboard::copy(text) {
        app.toast(format!(
            "copied {noun} ({} chars) → clipboard",
            format_count(text.chars().count())
        ));
    } else {
        // No pbcopy: the text still has to reach the user somehow.
        app.toast(format!("pbcopy unavailable — {noun} printed below"));
        eprintln!("{text}");
    }
}

/// `e`: suspend the TUI, run `$EDITOR`, then take the terminal back. Always
/// the cursor plan, never the whole selection — a bulk `e` would be a
/// footgun — and the toast says so when a selection was ignored.
fn open_in_editor(app: &mut App, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
    let Some(path) = app.selected_plan_path() else {
        app.toast("select a plan first");
        return Ok(());
    };
    let had_selection = !app.selection.is_empty();
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    // Hand the terminal back completely — including mouse reporting, which
    // would otherwise keep the editor from receiving clicks.
    if app.mouse {
        disable_mouse();
    }
    ratatui::restore();
    let status = std::process::Command::new(&editor).arg(&path).status();
    *terminal = enter_terminal(app.mouse)?;
    terminal.clear()?;
    // The file may have changed under us; drop the cached handoff prompt.
    app.invalidate(&path);

    let note = if had_selection {
        " · e is cursor-only"
    } else {
        ""
    };
    match status {
        Ok(status) if status.success() => app.toast(format!("edited with {editor}{note}")),
        Ok(status) => app.toast(format!("{editor} exited with {status}{note}")),
        Err(err) => app.toast(format!("could not run {editor}: {err}")),
    }
    Ok(())
}

/// `O`: hand every selected file to the macOS default app (Obsidian, if
/// registered).
fn open_externally(app: &mut App) {
    let paths: Vec<std::path::PathBuf> = app
        .action_targets()
        .iter()
        .filter_map(|index| app.plans.get(*index))
        .map(|plan| plan.path.clone())
        .collect();
    if paths.is_empty() {
        app.toast("select a plan first");
        return;
    }
    let mut opened = 0usize;
    let mut failure = None;
    for path in &paths {
        match std::process::Command::new("open").arg(path).spawn() {
            Ok(_) => opened += 1,
            Err(err) => failure = Some(err),
        }
    }
    match failure {
        None if opened == 1 => app.toast(format!("opened {}", paths[0].display())),
        None => app.toast(format!("opened {opened} plans")),
        Some(err) => app.toast(format!("open failed: {err}")),
    }
}

/// Re-enter raw mode + the alternate screen after an editor took it over.
/// Deliberately not `ratatui::init()`: that would stack another panic hook on
/// every editor launch.
fn enter_terminal(mouse: bool) -> anyhow::Result<DefaultTerminal> {
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    if mouse {
        enable_mouse()?;
    }
    Ok(Terminal::new(CrosstermBackend::new(std::io::stdout()))?)
}

fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(frame, chunks[0], app);

    let body = chunks[1];
    if app.detail_open && body.width >= 40 {
        let columns = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(body);
        draw_list(frame, columns[0], app);
        draw_detail(frame, columns[1], app);
    } else {
        // No detail pane: make sure a stale area cannot swallow wheel events.
        app.detail_area = Rect::new(0, 0, 0, 0);
        app.detail_lines = 0;
        app.reset_detail_scroll();
        draw_list(frame, body, app);
    }

    draw_footer(frame, chunks[2], app);

    if app.status_edit.is_some() {
        draw_status_edit(frame, area, app);
    }

    if app.help_open {
        draw_help(frame, area, app);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let summary = app
        .header_status_counts()
        .iter()
        .map(|(label, count)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(" · ");

    let left = format!(
        " plans · {} across {} repos · {summary}",
        app.plans.len(),
        app.repo_count()
    );

    let mut right = vec![
        format!("group {}", app.group_by.label()),
        format!(
            "sort {}{}",
            app.sort.label(),
            if app.sort_desc { "↓" } else { "↑" }
        ),
        app.status_filter_label(),
    ];
    if app.selection_len() > 0 {
        right.push(format!("{} selected", app.selection_len()));
        let hidden = app.hidden_selected_count();
        if hidden > 0 {
            right.push(format!("{hidden} hidden"));
        }
    }
    let stale = app.visible_stale_count();
    if stale > 0 {
        right.push(format!("{stale} stale"));
    }
    if app.git.is_some() {
        let dirty = app.visible_dirty_count();
        right.push(if dirty > 0 {
            format!("git · {dirty} dirty")
        } else {
            "git".to_string()
        });
    }
    if let Some(repo) = &app.repo_filter {
        right.push(format!("repo~{repo}"));
    }
    let right = format!("{} ", right.join(" · "));

    let width = area.width as usize;
    let right_width = UnicodeWidthStr::width(right.as_str()).min(width);
    // Reserve a gap so the two halves never collide.
    let left = truncate_right(&left, width.saturating_sub(right_width + 1));
    let pad = width
        .saturating_sub(UnicodeWidthStr::width(left.as_str()))
        .saturating_sub(right_width);

    let line = Line::from(vec![
        Span::styled(left, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.rows.is_empty() {
        // Never leave a blank pane: say why it is blank.
        let message = if app.query.trim().is_empty() {
            "  (no plans match the current filter — press a to include every status)".to_string()
        } else {
            format!(
                "  (nothing matches `{}` — esc clears the search, a shows every status)",
                app.query.trim()
            )
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::new().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            area,
        );
        app.list_area = area;
        app.list_offset = 0;
        return;
    }

    let width = area.width as usize;
    // Grouping by repo already says the repo in the header, so the column is
    // only worth its width when the setting asks for it.
    let hide_repo = match app.repo_column {
        RepoColumn::Never => true,
        RepoColumn::Always => false,
        RepoColumn::Auto => app.group_by == GroupBy::Repo,
    };
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| match row {
            Row::Group {
                label,
                count,
                collapsed,
                status,
                ..
            } => ListItem::new(group_line(
                label,
                *count,
                *collapsed,
                status.as_ref(),
                index == app.selected,
            )),
            Row::Plan(plan_index) => {
                let plan = &app.plans[*plan_index];
                let stale_age = if app.stale_days > 0 {
                    plan.age_days(app.today)
                        .filter(|age| *age >= app.stale_days)
                } else {
                    None
                };
                ListItem::new(plan_line(
                    plan,
                    RowOptions {
                        width,
                        cursor: index == app.selected,
                        in_selection: app.is_plan_selected(*plan_index),
                        stale_age,
                        date_format: app.date_format,
                        hide_repo,
                        strip_slug_date: app.strip_slug_date,
                        duplicate: plan.duplicate_of.is_some(),
                        git: app.git_status(plan),
                    },
                ))
            }
        })
        .collect();

    let list = List::new(items).highlight_style(Style::new().bg(Color::Indexed(237)));
    let mut state = ListState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.selected.min(app.rows.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
    // Recorded so mouse clicks can be mapped back to rows.
    app.list_area = area;
    app.list_offset = state.offset();
}

fn group_line(
    label: &str,
    count: usize,
    collapsed: bool,
    status: Option<&Status>,
    selected: bool,
) -> Line<'static> {
    let glyph = if collapsed { "▸" } else { "▾" };
    let color = match status {
        Some(status) => status_color(status),
        None => Color::Blue,
    };
    let mut style = Style::new().fg(color).add_modifier(Modifier::BOLD);
    if status.is_some_and(|s| matches!(s, Status::Done | Status::Dropped)) {
        style = style.add_modifier(Modifier::DIM);
    }
    if selected {
        style = style.bg(Color::Indexed(237));
    }
    Line::from(vec![
        Span::styled(
            format!("{} {glyph} ", if selected { "▶" } else { " " }),
            style,
        ),
        Span::styled(format!("{label} ({count})"), style),
    ])
}

/// Everything `plan_line` needs beyond the plan itself. A struct rather than
/// seven positional arguments, most of them booleans.
struct RowOptions {
    width: usize,
    cursor: bool,
    in_selection: bool,
    stale_age: Option<i64>,
    date_format: DateFormat,
    hide_repo: bool,
    strip_slug_date: bool,
    duplicate: bool,
    /// `None` when git was not consulted; `Some` reserves the marker column.
    git: Option<GitStatus>,
}

fn plan_line(plan: &Plan, options: RowOptions) -> Line<'static> {
    let RowOptions {
        width,
        cursor,
        in_selection,
        stale_age,
        date_format,
        hide_repo,
        strip_slug_date,
        duplicate,
        git,
    } = options;
    let columns = columns(width, hide_repo, date_format.width(), git.is_some());

    let status_style = {
        let mut style = Style::new().fg(status_color(&plan.status));
        if matches!(plan.status, Status::Done | Status::Dropped) {
            style = style.add_modifier(Modifier::DIM);
        }
        style
    };

    // A stale row shows its age instead of the date, in red with a `!`, so the
    // thing that made it stale is what the eye lands on.
    let (date, date_style) = match stale_age {
        Some(age) => (
            format!("!{age}d"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        None => {
            let date = plan
                .display_date()
                .map(|d| date_format.render(d))
                .unwrap_or_default();
            let style = match plan.display_date() {
                Some(date) if (Local::now().date_naive() - date).num_days() > 30 => {
                    Style::new().fg(Color::DarkGray)
                }
                _ => Style::new(),
            };
            (date, style)
        }
    };

    // `▶` is the cursor and `▌` the selection, so a row can be both. The pair
    // still fits the two-cell marker column.
    let mut spans = vec![
        Span::styled(
            format!(
                "{}{}",
                if cursor { '▶' } else { ' ' },
                if in_selection { '▌' } else { ' ' }
            ),
            Style::new().fg(Color::White),
        ),
        Span::styled("●", status_style),
        Span::raw(" "),
        Span::styled(
            pad_right(status_column_label(&plan.status), STATUS_W),
            status_style,
        ),
        Span::raw(" "),
        Span::styled(pad_right(&date, columns.date), date_style),
        Span::raw(" "),
    ];
    if columns.show_git {
        let marker = git.map(GitStatus::marker).unwrap_or(" ");
        let style = match git {
            Some(GitStatus::Modified) => Style::new().fg(Color::Yellow),
            Some(GitStatus::Untracked) => Style::new().fg(Color::Green),
            _ => Style::new(),
        };
        spans.push(Span::styled(marker, style));
        spans.push(Span::raw(" "));
    }

    if columns.show_repo {
        spans.push(Span::styled(
            pad_right(&truncate_repo(&plan.repo_name, REPO_W), REPO_W),
            Style::new().fg(Color::Indexed(110)),
        ));
        spans.push(Span::raw(" "));
    }

    let slug = if strip_slug_date {
        plan.slug_without_date()
    } else {
        plan.slug.as_str()
    };
    spans.push(Span::styled(
        pad_right(&truncate_right(slug, columns.slug), columns.slug),
        Style::new(),
    ));

    if columns.show_title {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            pad_right(&truncate_right(&plan.title, columns.title), columns.title),
            Style::new().fg(Color::Indexed(250)),
        ));
    }
    if duplicate {
        spans.push(Span::styled(
            " ⧉",
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        ));
    }
    if plan.has_handoff {
        spans.push(Span::styled(" ⏎", Style::new().fg(Color::Indexed(114))));
    }

    let line = Line::from(spans);
    // The cursor keeps the brighter background when a row is both.
    if cursor {
        line.style(Style::new().bg(Color::Indexed(237)))
    } else if in_selection {
        line.style(Style::new().bg(Color::Indexed(24)))
    } else {
        line
    }
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    app.detail_area = area;
    let index = app.selected_index();
    let block = Block::bordered()
        .title(" detail ")
        .border_style(Style::new().fg(Color::Indexed(240)));

    let Some(index) = index else {
        app.detail_lines = 0;
        app.reset_detail_scroll();
        frame.render_widget(
            Paragraph::new("select a plan to see its handoff prompt")
                .block(block)
                .style(Style::new().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    };

    let (title, repo, path, status, status_raw, owner, created, updated, tags, duplicate_of) = {
        let plan = &app.plans[index];
        (
            plan.title.clone(),
            plan.repo_name.clone(),
            plan.path.display().to_string(),
            plan.status.label().to_string(),
            plan.status_raw.clone(),
            plan.owner.clone(),
            plan.date_created.map(|d| d.format("%Y-%m-%d").to_string()),
            plan.date_updated.map(|d| d.format("%Y-%m-%d").to_string()),
            plan.tags.join(", "),
            plan.duplicate_of
                .as_ref()
                .map(|canonical| canonical.display().to_string()),
        )
    };
    let handoff = app.handoff_for(index);

    let mut lines = vec![
        Line::from(Span::styled(
            title,
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(repo, Style::new().fg(Color::Indexed(110))),
            Span::raw("  ·  "),
            Span::styled(status, Style::new().fg(Color::Magenta)),
            status_raw
                .filter(|raw| !raw.is_empty())
                .map(|raw| Span::styled(format!("  {raw}"), Style::new().fg(Color::DarkGray)))
                .unwrap_or_else(|| Span::raw("")),
        ]),
        Line::from(Span::styled(path, Style::new().fg(Color::DarkGray))),
    ];
    if let Some(canonical) = duplicate_of {
        lines.push(Line::from(Span::styled(
            format!("⧉ duplicate of {canonical}"),
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )));
    }

    let mut meta = Vec::new();
    if let Some(owner) = owner {
        meta.push(format!("owner: {owner}"));
    }
    if let Some(created) = created {
        meta.push(format!("created {created}"));
    }
    if let Some(updated) = updated {
        meta.push(format!("updated {updated}"));
    }
    if !meta.is_empty() {
        lines.push(Line::from(Span::styled(
            meta.join("  ·  "),
            Style::new().fg(Color::DarkGray),
        )));
    }
    if !tags.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("tags: {tags}"),
            Style::new().fg(Color::Indexed(108)),
        )));
    }
    lines.push(Line::raw(""));

    match handoff {
        Some(prompt) => {
            lines.push(Line::from(Span::styled(
                "## Handoff Prompt",
                Style::new()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Indexed(114)),
            )));
            lines.extend(
                prompt
                    .lines()
                    .take(40)
                    .map(|line| Line::raw(line.to_string())),
            );
            if prompt.lines().count() > 40 {
                lines.push(Line::from(Span::styled(
                    "…",
                    Style::new().fg(Color::DarkGray),
                )));
            }
        }
        None => {
            lines.push(Line::from(Span::styled(
                "no ## Handoff Prompt — press Y to synthesize one",
                Style::new().fg(Color::Yellow),
            )));
        }
    }

    app.detail_lines = lines.len();
    let scroll = app.detail_scroll;
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let line = match &app.toast {
        Some((message, _)) => Line::from(Span::styled(
            format!(" {message}"),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Indexed(114))
                .add_modifier(Modifier::BOLD),
        )),
        None if app.filtering => Line::from(vec![
            Span::styled(" /", Style::new().fg(Color::Yellow)),
            Span::raw(app.query.clone()),
            Span::styled(
                "   enter commit · esc clear",
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        None => Line::from(Span::styled(
            " j/k move · v select · J/K extend · space toggle · h/l fold · tab detail · \
             / search · 1-5 status · D done · u undo · a all · s group · x dupes · \
             g git · o sort · r reverse · y copy · Y synth · ? help · q quit",
            Style::new().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = [
        ("j / k, ↓ / ↑", "move selection"),
        ("v", "toggle select mode (selects the cursor row)"),
        ("J / K, ⇧↓ / ⇧↑", "extend the selection one row"),
        ("Space", "toggle the plan under the cursor"),
        (
            "Home / G",
            "top / bottom (in select mode: extend to the edge)",
        ),
        ("Esc", "clear the selection; quits when there is none"),
        (
            "D / S / y / Y / p / O",
            "act on the whole selection (one confirm)",
        ),
        ("⇧click / ^click", "extend from the anchor / toggle one row"),
        ("Ctrl-d / Ctrl-u", "half page"),
        ("h / l, ← / →", "collapse / expand group"),
        (
            "zc / zo / zR / zM",
            "fold under cursor / all open / all closed",
        ),
        ("Tab", "toggle the detail pane"),
        ("/", "fuzzy filter · Enter commits · Esc clears"),
        ("1–5", "toggle TODO / WIP / UAT / DONE / DROPPED"),
        ("D", "mark the selection #DONE (asks to confirm)"),
        ("a", "show every status"),
        ("s", "cycle grouping: status → repo → week → none"),
        ("x", "hide / show duplicate copies"),
        ("g", "toggle git status markers (M modified · ? untracked)"),
        ("o / r", "cycle sort · reverse it"),
        (
            "y / Y",
            "copy the selected Handoff Prompts / synthesized ones",
        ),
        ("p", "copy the selected plans' absolute paths"),
        (
            "e / O",
            "open the cursor plan in $EDITOR (cursor only) / open the selection",
        ),
        ("S", "set status (asks to confirm; the only write)"),
        ("u", "undo the most recent status batch"),
        ("R", "rescan from disk (clears the selection)"),
        ("? / q", "close this help / quit"),
        ("mouse", "wheel scrolls · click selects · header folds"),
        ("PgUp / PgDn", "page the list, or the detail pane when open"),
    ]
    .iter()
    .map(|(key, what)| {
        Line::from(vec![
            Span::styled(format!("  {key:<18}"), Style::new().fg(Color::Yellow)),
            Span::raw(what.to_string()),
        ])
    })
    .collect();

    if let Some(path) = &app.config_path {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  config            ", Style::new().fg(Color::Yellow)),
            Span::styled(path.display().to_string(), Style::new().fg(Color::DarkGray)),
        ]));
    }

    // Size the box to its longest line instead of a fixed 62: the copy/status
    // rows run past that, and a clipped key map is worse than a wider box.
    let content_width = lines.iter().map(|line| line.width()).max().unwrap_or(0);
    let width = (content_width as u16 + 2).min(area.width);
    let height = (lines.len() as u16 + 4).min(area.height);
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" plans — help ")
                .border_style(Style::new().fg(Color::Yellow)),
        ),
        popup,
    );
}

/// The `S` modal. Deliberately shows exactly what will be written — and, when
/// several plans are selected, names them (up to a handful) and says how many
/// will be skipped for having no frontmatter.
fn draw_status_edit(frame: &mut Frame, area: Rect, app: &App) {
    let Some(edit) = &app.status_edit else {
        return;
    };

    let today = Local::now().date_naive().format("%Y-%m-%d");
    let skipped = edit
        .targets
        .iter()
        .filter(|index| app.plans.get(**index).is_none_or(|p| !p.has_frontmatter))
        .count();
    let mut lines = vec![
        Line::from(Span::styled(
            target_summary(app, edit),
            Style::new().fg(Color::Indexed(110)),
        )),
        Line::raw(""),
    ];
    for (index, status) in Status::lifecycle().iter().enumerate() {
        let selected = index == app.status_choice_index();
        let mut style = Style::new().fg(status_color(status));
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(vec![
            Span::styled(if selected { "  ▶ " } else { "    " }, style),
            Span::styled(status.label().to_string(), style),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        if edit.targets.len() == 1 {
            format!(
                "  enter: write status: #{} · dateUpdated: {today}",
                edit.choice.label()
            )
        } else {
            format!(
                "  enter: write status: #{} to {} plans · dateUpdated: {today}",
                edit.choice.label(),
                edit.targets.len() - skipped
            )
        },
        Style::new().fg(Color::Indexed(114)),
    )));
    if skipped > 0 {
        lines.push(Line::from(Span::styled(
            format!("  {skipped} selected plan(s) have no frontmatter and will be skipped"),
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  esc: cancel — filenames are never touched",
        Style::new().fg(Color::DarkGray),
    )));

    let width = 64.min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(if edit.targets.len() == 1 {
                    " set status ".to_string()
                } else {
                    format!(" mark {} plans ", edit.targets.len())
                })
                .border_style(Style::new().fg(Color::Yellow)),
        ),
        popup,
    );
}

/// The first line of the status modal: the one path, or up to six slugs.
fn target_summary(app: &App, edit: &crate::app::StatusEdit) -> String {
    if edit.targets.len() == 1 {
        return app
            .plans
            .get(edit.targets[0])
            .map(|plan| plan.rel_path.clone())
            .unwrap_or_default();
    }
    let mut names: Vec<String> = edit
        .targets
        .iter()
        .take(6)
        .filter_map(|index| app.plans.get(*index))
        .map(|plan| plan.slug.clone())
        .collect();
    if edit.targets.len() > names.len() {
        names.push(format!("… and {} more", edit.targets.len() - names.len()));
    }
    names.join("\n")
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

// ------------------------------------------------------------------- helpers

/// Which columns fit, and how wide the flexible ones are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Columns {
    show_repo: bool,
    show_title: bool,
    show_git: bool,
    date: usize,
    slug: usize,
    title: usize,
}

/// Columns are dropped in order of usefulness: the title goes first, then the
/// repo, so a 60-column terminal still shows status, date and slug.
///
/// `hide_repo` is the one thing width cannot decide — when the list is already
/// grouped by repo, repeating the name on every row is noise.
fn columns(width: usize, hide_repo: bool, date_width: usize, show_git: bool) -> Columns {
    let show_repo = !hide_repo && width >= REPO_MIN_WIDTH;
    let show_title = width >= TITLE_MIN_WIDTH;

    let mut fixed = MARKER_W + 1 + STATUS_W + 1 + date_width + 1 + GLYPH_W;
    if show_git {
        fixed += GIT_W + 1;
    }
    if show_repo {
        fixed += REPO_W + 1;
    }
    let available = width.saturating_sub(fixed);

    // Give the human-readable title a little more room than the slug, but keep
    // the slug long enough to stay recognisable.
    let (slug, title) = if show_title && available >= 26 {
        let slug = ((available * 45) / 100).clamp(16, SLUG_W);
        (slug, available - slug - 1)
    } else {
        (available, 0)
    };

    Columns {
        show_repo,
        show_title,
        show_git,
        date: date_width,
        slug,
        title,
    }
}

fn status_color(status: &Status) -> Color {
    match status {
        Status::Todo => Color::Cyan,
        Status::Wip => Color::Yellow,
        Status::Uat => Color::Magenta,
        Status::Done => Color::Green,
        Status::Dropped => Color::Red,
        Status::Unknown => Color::DarkGray,
        Status::Other(_) => Color::White,
    }
}

/// The status column is fixed-width, so unknown one-offs collapse to OTHER;
/// the detail pane still shows the raw value.
fn status_column_label(status: &Status) -> &str {
    match status {
        Status::Other(_) => "OTHER",
        other => other.label(),
    }
}

fn pad_right(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

/// Truncate from the right, keeping the beginning.
fn truncate_right(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Truncate from the left, keeping the end.
fn truncate_left(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        kept.push(ch);
        used += w;
    }
    kept.reverse();
    format!("…{}", kept.into_iter().collect::<String>())
}

/// Repo names truncate at a `/` so the leaf always survives: at 15 cells
/// `apps/music-library` becomes `…/music-library`.
fn truncate_repo(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if let Some(leaf) = text.rsplit('/').next() {
        let candidate = format!("…/{leaf}");
        if UnicodeWidthStr::width(candidate.as_str()) <= width {
            return candidate;
        }
    }
    truncate_left(text, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_display_width() {
        assert_eq!(truncate_right("abcdef", 4), "abc…");
        assert_eq!(truncate_right("abc", 4), "abc");
        assert_eq!(truncate_right("abcdef", 1), "…");
        assert_eq!(truncate_left("abcdef", 4), "…def");
        assert_eq!(truncate_repo("apps/music-library", 15), "…/music-library");
        assert_eq!(truncate_repo("dotfiles", 15), "dotfiles");
    }

    #[test]
    fn padding_counts_cells_not_chars() {
        assert_eq!(pad_right("ab", 5), "ab   ");
        assert_eq!(pad_right("abcdef", 3), "abcdef");
    }

    #[test]
    fn unknown_statuses_collapse_in_the_column_only() {
        assert_eq!(
            status_column_label(&Status::Other("READY".to_string())),
            "OTHER"
        );
        assert_eq!(status_column_label(&Status::Uat), "UAT");
    }

    #[test]
    fn narrow_terminals_drop_the_title_then_the_repo() {
        let iso = DateFormat::Iso.width();
        // Plenty of room: both extra columns, title gets the bigger share.
        assert_eq!(
            columns(140, false, iso, false),
            Columns {
                show_repo: true,
                show_title: true,
                show_git: false,
                date: 10,
                slug: 32,
                title: 64
            }
        );
        // 77 columns is the list area at 140 with the detail pane open.
        assert_eq!(
            columns(77, false, iso, false),
            Columns {
                show_repo: true,
                show_title: true,
                show_git: false,
                date: 10,
                slug: 16,
                title: 17
            }
        );
        // 60 columns: title gone, repo still there.
        assert_eq!(
            columns(60, false, iso, false),
            Columns {
                show_repo: true,
                show_title: false,
                show_git: false,
                date: 10,
                slug: 17,
                title: 0
            }
        );
        // Very narrow: repo gone too, slug takes what is left.
        assert_eq!(
            columns(50, false, iso, false),
            Columns {
                show_repo: false,
                show_title: false,
                show_git: false,
                date: 10,
                slug: 26,
                title: 0
            }
        );
        // Degenerate widths must not underflow.
        for width in 0..24usize {
            let c = columns(width, false, iso, false);
            assert_eq!(c.slug, 0);
            assert_eq!(c.title, 0);
        }
    }

    #[test]
    fn short_dates_hand_the_width_back_to_the_title() {
        // A short date is 5 cells instead of 10, so the slug/title get 5 more.
        let short = columns(140, false, DateFormat::Short.width(), false);
        let iso = columns(140, false, DateFormat::Iso.width(), false);
        assert_eq!(short.date, 5);
        assert_eq!(short.title, iso.title + 5);
    }

    #[test]
    fn grouping_by_repo_drops_the_redundant_repo_column() {
        let grouped = columns(140, true, DateFormat::Iso.width(), false);
        assert!(!grouped.show_repo);
        // The width it frees goes to the title.
        let ungrouped = columns(140, false, DateFormat::Iso.width(), false);
        assert_eq!(grouped.title, ungrouped.title + REPO_W + 1);
    }

    #[test]
    fn the_git_column_only_costs_width_when_git_is_on() {
        let iso = DateFormat::Iso.width();
        let without = columns(140, false, iso, false);
        let with = columns(140, false, iso, true);
        assert!(!without.show_git);
        assert!(with.show_git);
        // The git marker and its separating space come out of the title.
        assert_eq!(with.title, without.title - GIT_W - 1);
    }

    #[test]
    fn date_formats_render_as_advertised() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        assert_eq!(DateFormat::Iso.render(date), "2026-09-04");
        assert_eq!(DateFormat::Short.render(date), "9/4");
        assert_eq!(DateFormat::Short.width(), 5);
    }

    #[test]
    fn centering_never_leaves_the_area() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        let popup = centered(area, 62, 30);
        assert!(popup.width <= area.width && popup.height <= area.height);
    }
}
