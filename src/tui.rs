use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use humansize::{format_size, BINARY};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;

use crate::{calculate_wasted_bytes, DuplicateGroup, ScanReport};

const PREVIEW_BYTES: usize = 64;

/// Active pane focus in the split-screen duplicate explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPane {
    Groups,
    Files,
}

/// Active popup modal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modal {
    None,
    ConfirmDelete,
    DryRunPreview,
    Help,
}

/// Interactive Ratatui state for exploring and cleaning duplicate files.
pub(crate) struct TuiApp {
    report: ScanReport,
    dry_run: bool,
    focus: FocusPane,
    group_state: ListState,
    file_state: ListState,
    marked_paths: HashSet<PathBuf>,
    modal: Modal,
    status_message: String,
    status_is_warning: bool,
    should_quit: bool,
    preview_path: Option<PathBuf>,
    preview_lines: Vec<String>,
}

/// RAII guard that always restores raw mode and alternate screen on exit.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

/// Launches the interactive full-screen TUI for `report`.
pub(crate) fn run_tui(report: ScanReport, dry_run: bool) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("TUI mode requires an interactive terminal (TTY)");
    }

    let mut terminal = ratatui::try_init().context("Failed to initialize terminal UI")?;
    let _guard = TerminalGuard;

    let mut app = TuiApp::new(report, dry_run);
    while !app.should_quit {
        terminal
            .draw(|frame| app.render(frame))
            .context("Failed to draw TUI frame")?;

        match event::read().context("Failed to read terminal event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    Ok(())
}

impl TuiApp {
    /// Initializes TUI state from a completed scan report.
    fn new(report: ScanReport, dry_run: bool) -> Self {
        let mut group_state = ListState::default();
        let mut file_state = ListState::default();
        if !report.groups.is_empty() {
            group_state.select(Some(0));
            if let Some(first) = report.groups.first() {
                if !first.files.is_empty() {
                    file_state.select(Some(0));
                }
            }
        }

        let status_message = if dry_run {
            "Dry-run mode active: Space marks copies, 'd' previews deletions without modifying disk."
                .to_string()
        } else {
            "Use Tab/arrows to browse, Space to mark copies, 'd' to delete marked, '?' for help."
                .to_string()
        };

        let mut app = Self {
            report,
            dry_run,
            focus: FocusPane::Groups,
            group_state,
            file_state,
            marked_paths: HashSet::new(),
            modal: Modal::None,
            status_message,
            status_is_warning: false,
            should_quit: false,
            preview_path: None,
            preview_lines: Vec::new(),
        };
        app.refresh_preview();
        app
    }

    /// Returns the currently selected duplicate group, if any.
    fn current_group(&self) -> Option<&DuplicateGroup> {
        let idx = self.group_state.selected()?;
        self.report.groups.get(idx)
    }

    /// Returns the currently selected file path inside the active group, if any.
    fn current_file(&self) -> Option<&PathBuf> {
        let group = self.current_group()?;
        let file_idx = self.file_state.selected()?;
        group.files.get(file_idx)
    }

    /// Returns `(marked_count, marked_bytes)` across all duplicate groups.
    fn marked_summary(&self) -> (usize, u64) {
        let mut count = 0;
        let mut bytes = 0_u64;
        for group in &self.report.groups {
            for path in &group.files {
                if self.marked_paths.contains(path) {
                    count += 1;
                    bytes = bytes.saturating_add(group.size);
                }
            }
        }
        (count, bytes)
    }

    /// Counts marked files within `group`.
    fn marked_in_group(&self, group: &DuplicateGroup) -> usize {
        group
            .files
            .iter()
            .filter(|path| self.marked_paths.contains(*path))
            .count()
    }

    /// Dispatches a key press based on modal and pane focus.
    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match self.modal {
            Modal::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter
                ) {
                    self.modal = Modal::None;
                }
                return;
            }
            Modal::DryRunPreview => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('n')
                ) {
                    self.modal = Modal::None;
                }
                return;
            }
            Modal::ConfirmDelete => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        self.execute_marked_deletions();
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                        self.modal = Modal::None;
                        self.set_status("Deletion cancelled.", false);
                    }
                    _ => {}
                }
                return;
            }
            Modal::None => {}
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('?') => self.modal = Modal::Help,
            KeyCode::Tab | KeyCode::BackTab => self.toggle_focus(),
            KeyCode::Left | KeyCode::Char('h') => self.focus = FocusPane::Groups,
            KeyCode::Right | KeyCode::Char('l') => self.focus = FocusPane::Files,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home | KeyCode::Char('g') => self.jump_start(),
            KeyCode::End | KeyCode::Char('G') => self.jump_end(),
            KeyCode::Char(' ') => self.toggle_mark_current(),
            KeyCode::Enter if self.focus == FocusPane::Files => self.toggle_mark_current(),
            KeyCode::Enter if self.focus == FocusPane::Groups => self.focus = FocusPane::Files,
            KeyCode::Char('a') => self.auto_mark_current_group(),
            KeyCode::Char('A') => self.auto_mark_all_groups(),
            KeyCode::Char('u') => self.unmark_current_group(),
            KeyCode::Char('U') => self.unmark_all_groups(),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => self.open_delete_modal(),
            _ => {}
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            FocusPane::Groups => FocusPane::Files,
            FocusPane::Files => FocusPane::Groups,
        };
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            FocusPane::Groups => {
                let len = self.report.groups.len();
                if len == 0 {
                    return;
                }
                let current = self.group_state.selected().unwrap_or(0);
                let next = clamp_move(current, delta, len);
                if next != current {
                    self.group_state.select(Some(next));
                    self.reset_file_selection();
                    self.refresh_preview();
                }
            }
            FocusPane::Files => {
                let len = self.current_group().map_or(0, |group| group.files.len());
                if len == 0 {
                    return;
                }
                let current = self.file_state.selected().unwrap_or(0);
                let next = clamp_move(current, delta, len);
                if next != current {
                    self.file_state.select(Some(next));
                    self.refresh_preview();
                }
            }
        }
    }

    fn jump_start(&mut self) {
        match self.focus {
            FocusPane::Groups => {
                if !self.report.groups.is_empty() {
                    self.group_state.select(Some(0));
                    self.reset_file_selection();
                    self.refresh_preview();
                }
            }
            FocusPane::Files => {
                if self.current_group().is_some_and(|g| !g.files.is_empty()) {
                    self.file_state.select(Some(0));
                    self.refresh_preview();
                }
            }
        }
    }

    fn jump_end(&mut self) {
        match self.focus {
            FocusPane::Groups => {
                if let Some(last) = self.report.groups.len().checked_sub(1) {
                    self.group_state.select(Some(last));
                    self.reset_file_selection();
                    self.refresh_preview();
                }
            }
            FocusPane::Files => {
                if let Some(last) = self
                    .current_group()
                    .and_then(|group| group.files.len().checked_sub(1))
                {
                    self.file_state.select(Some(last));
                    self.refresh_preview();
                }
            }
        }
    }

    fn reset_file_selection(&mut self) {
        let has_files = self.current_group().is_some_and(|g| !g.files.is_empty());
        self.file_state
            .select(if has_files { Some(0) } else { None });
    }

    /// Toggles deletion mark on the selected file, refusing to mark the final remaining copy.
    fn toggle_mark_current(&mut self) {
        let Some(group_idx) = self.group_state.selected() else {
            return;
        };
        let Some(file_idx) = self.file_state.selected() else {
            return;
        };
        let Some(group) = self.report.groups.get(group_idx) else {
            return;
        };
        let Some(path) = group.files.get(file_idx).cloned() else {
            return;
        };

        if self.marked_paths.contains(&path) {
            self.marked_paths.remove(&path);
            self.set_status(format!("Unmarked {}", path.display()), false);
            return;
        }

        let already_marked = self.marked_in_group(group);
        if already_marked + 1 >= group.files.len() {
            self.set_status(
                "Cannot mark every copy in a group — at least one file must remain.",
                true,
            );
            return;
        }

        self.marked_paths.insert(path.clone());
        self.set_status(format!("Marked {} for deletion", path.display()), false);
    }

    /// Marks every copy in the current group except the first sorted file.
    fn auto_mark_current_group(&mut self) {
        let Some(group_idx) = self.group_state.selected() else {
            return;
        };
        let Some(group) = self.report.groups.get(group_idx) else {
            return;
        };
        let marked_now = mark_all_except_first(group, &mut self.marked_paths);
        self.set_status(
            format!(
                "Kept first copy and marked {marked_now} duplicate(s) in group #{}.",
                group_idx + 1
            ),
            false,
        );
    }

    /// Marks every copy except the first across all duplicate groups.
    fn auto_mark_all_groups(&mut self) {
        if self.report.groups.is_empty() {
            return;
        }
        let mut newly_marked = 0;
        for group in &self.report.groups {
            newly_marked += mark_all_except_first(group, &mut self.marked_paths);
        }
        let (total_count, total_bytes) = self.marked_summary();
        self.set_status(
            format!(
                "Auto-marked {newly_marked} file(s) ({total_count} total marked, {} reclaimable).",
                format_size(total_bytes, BINARY)
            ),
            false,
        );
    }

    fn unmark_current_group(&mut self) {
        let Some(group_idx) = self.group_state.selected() else {
            return;
        };
        let Some(group) = self.report.groups.get(group_idx) else {
            return;
        };
        let mut removed = 0;
        for path in &group.files {
            if self.marked_paths.remove(path) {
                removed += 1;
            }
        }
        self.set_status(
            format!("Unmarked {removed} file(s) in group #{}.", group_idx + 1),
            false,
        );
    }

    fn unmark_all_groups(&mut self) {
        let count = self.marked_paths.len();
        self.marked_paths.clear();
        self.set_status(format!("Cleared {count} marked file(s)."), false);
    }

    fn open_delete_modal(&mut self) {
        let (count, _) = self.marked_summary();
        if count == 0 {
            self.set_status(
                "No files marked yet. Press Space or 'a'/'A' to mark copies.",
                true,
            );
            return;
        }
        self.modal = if self.dry_run {
            Modal::DryRunPreview
        } else {
            Modal::ConfirmDelete
        };
    }

    /// Deletes all marked files from disk and updates in-memory groups safely.
    fn execute_marked_deletions(&mut self) {
        let targets = self.collect_marked_targets();
        let mut deleted_paths = HashSet::new();
        let mut freed_bytes = 0_u64;
        let mut failed_count = 0_usize;

        for (path, size) in targets {
            match fs::remove_file(&path) {
                Ok(()) => {
                    self.marked_paths.remove(&path);
                    deleted_paths.insert(path);
                    freed_bytes = freed_bytes.saturating_add(size);
                }
                Err(_) => {
                    failed_count += 1;
                }
            }
        }

        let deleted_count = deleted_paths.len();
        apply_deletions_to_groups(&mut self.report.groups, &deleted_paths);
        self.clamp_selections();
        self.refresh_preview();
        self.modal = Modal::None;

        if failed_count > 0 {
            self.set_status(
                format!(
                    "Deleted {deleted_count} file(s) (freed {}), {failed_count} failed.",
                    format_size(freed_bytes, BINARY)
                ),
                true,
            );
        } else {
            self.set_status(
                format!(
                    "Deleted {deleted_count} file(s), freed {}.",
                    format_size(freed_bytes, BINARY)
                ),
                false,
            );
        }
    }

    fn collect_marked_targets(&self) -> Vec<(PathBuf, u64)> {
        let mut targets = Vec::new();
        for group in &self.report.groups {
            for path in &group.files {
                if self.marked_paths.contains(path) {
                    targets.push((path.clone(), group.size));
                }
            }
        }
        targets
    }

    fn clamp_selections(&mut self) {
        if self.report.groups.is_empty() {
            self.group_state.select(None);
            self.file_state.select(None);
            self.focus = FocusPane::Groups;
            return;
        }

        let max_group = self.report.groups.len() - 1;
        let next_group = self.group_state.selected().unwrap_or(0).min(max_group);
        self.group_state.select(Some(next_group));

        if let Some(group) = self.report.groups.get(next_group) {
            if group.files.is_empty() {
                self.file_state.select(None);
            } else {
                let max_file = group.files.len() - 1;
                let next_file = self.file_state.selected().unwrap_or(0).min(max_file);
                self.file_state.select(Some(next_file));
            }
        }
    }

    fn set_status(&mut self, message: impl Into<String>, is_warning: bool) {
        self.status_message = message.into();
        self.status_is_warning = is_warning;
    }

    fn refresh_preview(&mut self) {
        let current = self.current_file().cloned();
        if current == self.preview_path {
            return;
        }
        self.preview_path = current.clone();
        self.preview_lines = match current {
            Some(path) => build_file_preview(&path),
            None => vec!["No file selected.".to_string()],
        };
    }

    /// Renders the complete TUI dashboard and any active modal.
    fn render(&mut self, frame: &mut Frame) {
        let outer = Layout::vertical([
            Constraint::Length(5),
            Constraint::Min(10),
            Constraint::Length(3),
        ]);
        let [header_area, main_area, footer_area] = outer.areas(frame.area());

        self.render_header(frame, header_area);
        self.render_main_panes(frame, main_area);
        self.render_footer(frame, footer_area);

        match self.modal {
            Modal::None => {}
            Modal::ConfirmDelete => self.render_delete_modal(frame, false),
            Modal::DryRunPreview => self.render_delete_modal(frame, true),
            Modal::Help => render_help_modal(frame),
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Percentage(54), Constraint::Percentage(46)])
                .areas(area);

        let (marked_count, marked_bytes) = self.marked_summary();
        let total_wasted = calculate_wasted_bytes(&self.report.groups);
        let mode_badge = if self.dry_run {
            Span::styled(
                " DRY RUN ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " LIVE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        };

        let path_max = left_area.width.saturating_sub(18) as usize;
        let path_label = truncate_left(&self.report.scan_path.display().to_string(), path_max);
        let left_lines = vec![
            Line::from(vec![
                Span::styled("Path: ", Style::default().fg(Color::DarkGray)),
                Span::styled(path_label, Style::default().fg(Color::White)),
                Span::raw(" "),
                mode_badge,
            ]),
            Line::from(vec![
                Span::styled("Files:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.report.files_scanned.to_string(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled("Grps:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.report.duplicate_groups().to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled("Dups:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.report.duplicate_files().to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled("Skip:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.report.skipped_files.to_string(),
                    Style::default().fg(if self.report.skipped_files > 0 {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
            ]),
            Line::from(vec![
                Span::styled("Wasted:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format_size(total_wasted, BINARY),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" │ "),
                Span::styled("Marked:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{marked_count} ({})", format_size(marked_bytes, BINARY)),
                    Style::default()
                        .fg(if marked_count > 0 {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];

        let left_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " b3dup — Duplicate Space Analyzer ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        frame.render_widget(Paragraph::new(left_lines).block(left_block), left_area);

        let bar_width = right_area.width.saturating_sub(31).clamp(5, 16) as usize;
        let scanned_base = self.report.files_scanned.max(1);
        let s1_bar = render_ratio_bar(self.report.stage1.candidates, scanned_base, bar_width);
        let s2_bar = render_ratio_bar(self.report.stage2.candidates, scanned_base, bar_width);
        let s3_bar = render_ratio_bar(self.report.stage3.candidates, scanned_base, bar_width);

        let right_lines = vec![
            Line::from(vec![
                Span::styled("S1 Size ", Style::default().fg(Color::Cyan)),
                Span::styled(s1_bar, Style::default().fg(Color::Cyan)),
                Span::raw(format!(
                    " {:>4} ({}ms)",
                    self.report.stage1.candidates, self.report.stage1.elapsed_ms
                )),
            ]),
            Line::from(vec![
                Span::styled("S2 4KB  ", Style::default().fg(Color::Blue)),
                Span::styled(s2_bar, Style::default().fg(Color::Blue)),
                Span::raw(format!(
                    " {:>4} ({}ms)",
                    self.report.stage2.candidates, self.report.stage2.elapsed_ms
                )),
            ]),
            Line::from(vec![
                Span::styled("S3 Full ", Style::default().fg(Color::Yellow)),
                Span::styled(s3_bar, Style::default().fg(Color::Yellow)),
                Span::raw(format!(
                    " {:>4} ({}ms/{}ms)",
                    self.report.stage3.candidates,
                    self.report.stage3.elapsed_ms,
                    self.report.total_elapsed_ms
                )),
            ]),
        ];

        let right_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue))
            .title(Span::styled(
                " 3-Stage BLAKE3 Funnel ",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ));
        frame.render_widget(Paragraph::new(right_lines).block(right_block), right_area);
    }

    fn render_main_panes(&mut self, frame: &mut Frame, area: Rect) {
        let [groups_area, right_area] =
            Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
                .areas(area);

        self.render_groups_pane(frame, groups_area);
        self.render_right_pane(frame, right_area);
    }

    fn render_groups_pane(&mut self, frame: &mut Frame, area: Rect) {
        let border_color = if self.focus == FocusPane::Groups {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(
                format!(" Duplicate Groups ({}) ", self.report.groups.len()),
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            ));

        if self.report.groups.is_empty() {
            frame.render_widget(
                Paragraph::new("No duplicate files found.")
                    .style(Style::default().fg(Color::Green))
                    .block(block),
                area,
            );
            return;
        }

        let wide_groups = area.width >= 44;
        let items: Vec<ListItem> = self
            .report
            .groups
            .iter()
            .enumerate()
            .map(|(idx, group)| {
                let marked_here = self.marked_in_group(group);
                let wasted_text = format_size(group.wasted_bytes(), BINARY);
                let size_text = format_size(group.size, BINARY);
                let mut spans = vec![
                    Span::styled(
                        format!("{:>3}. ", idx + 1),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{wasted_text:>9}"),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                ];
                if wide_groups {
                    spans.push(Span::styled(
                        " wasted",
                        Style::default().fg(Color::DarkGray),
                    ));
                    spans.push(Span::raw(" │ "));
                } else {
                    spans.push(Span::raw(" │ "));
                }
                spans.push(Span::styled(
                    format!("{size_text}×{}", group.files.len()),
                    Style::default().fg(Color::Yellow),
                ));
                if marked_here > 0 {
                    spans.push(Span::styled(
                        format!(" [{marked_here}✗]"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, area, &mut self.group_state);

        let mut scrollbar_state = ScrollbarState::new(self.report.groups.len())
            .position(self.group_state.selected().unwrap_or(0));
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }

    fn render_right_pane(&mut self, frame: &mut Frame, area: Rect) {
        let preview_height = if area.height >= 15 { 5 } else { 0 };
        let [info_area, files_area, preview_area] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(preview_height),
        ])
        .areas(area);

        self.render_group_details(frame, info_area);
        self.render_files_pane(frame, files_area);
        if preview_height > 0 {
            self.render_preview_pane(frame, preview_area);
        }
    }

    fn render_group_details(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Group Details ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));

        let Some(group) = self.current_group() else {
            frame.render_widget(Paragraph::new("No group selected.").block(block), area);
            return;
        };

        let marked_here = self.marked_in_group(group);
        let hash_max = area.width.saturating_sub(10) as usize;
        let hash_display = truncate_right(&group.hash_hex(), hash_max);
        let lines = vec![
            Line::from(vec![
                Span::styled("BLAKE3: ", Style::default().fg(Color::DarkGray)),
                Span::styled(hash_display, Style::default().fg(Color::Cyan)),
            ]),
            Line::from(vec![
                Span::styled("Size:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}×{}", format_size(group.size, BINARY), group.files.len()),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(" │ "),
                Span::styled("Wasted:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format_size(group.wasted_bytes(), BINARY),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" │ "),
                Span::styled("Marked:", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{marked_here}/{}", group.files.len()),
                    Style::default().fg(if marked_here > 0 {
                        Color::Cyan
                    } else {
                        Color::White
                    }),
                ),
            ]),
        ];

        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_files_pane(&mut self, frame: &mut Frame, area: Rect) {
        let border_color = if self.focus == FocusPane::Files {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(
                " File Copies (Space: mark, a: auto, u: unmark) ",
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            ));

        let Some(group) = self.current_group() else {
            frame.render_widget(Paragraph::new("No files to inspect.").block(block), area);
            return;
        };

        let marked_here = self.marked_in_group(group);
        let only_one_kept = marked_here + 1 >= group.files.len();
        let reserved = if only_one_kept { 16 } else { 9 };
        let path_max = area.width.saturating_sub(reserved) as usize;

        let items: Vec<ListItem> = group
            .files
            .iter()
            .map(|path| {
                let is_marked = self.marked_paths.contains(path);
                let path_display = truncate_left(&path.display().to_string(), path_max);
                if is_marked {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            "[X] ",
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(path_display, Style::default().fg(Color::LightRed)),
                    ]))
                } else {
                    let mut spans = vec![
                        Span::styled("[ ] ", Style::default().fg(Color::Green)),
                        Span::styled(path_display, Style::default().fg(Color::White)),
                    ];
                    if only_one_kept {
                        spans.push(Span::styled(
                            " [KEEP]",
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        ));
                    }
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();

        let files_len = items.len();
        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, area, &mut self.file_state);

        let mut scrollbar_state =
            ScrollbarState::new(files_len).position(self.file_state.selected().unwrap_or(0));
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }

    fn render_preview_pane(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " File Preview (first 64 bytes) ",
                Style::default().fg(Color::DarkGray),
            ));

        let lines: Vec<Line> = self
            .preview_lines
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(Color::Gray))))
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let status_style = if self.status_is_warning {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };

        let chips = [
            ("Tab", "Pane "),
            ("↑↓", "Move "),
            ("Spc", "Mark "),
            ("a/A", "Auto "),
            ("u/U", "Clear "),
            ("d", "Del "),
            ("?", "Help "),
            ("q", "Quit"),
        ];
        let mut hint_spans = Vec::with_capacity(chips.len() * 2);
        for (key, desc) in chips {
            hint_spans.push(Span::styled(
                format!("[{key}]"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            hint_spans.push(Span::raw(desc));
        }

        let lines = vec![
            Line::from(Span::styled(self.status_message.clone(), status_style)),
            Line::from(hint_spans),
        ];

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_delete_modal(&self, frame: &mut Frame, dry_run_only: bool) {
        let targets = self.collect_marked_targets();
        let total_bytes: u64 = targets.iter().map(|(_, size)| *size).sum();

        let popup = centered_rect(72, 16, frame.area());
        frame.render_widget(Clear, popup);

        let title = if dry_run_only {
            " [DRY RUN] Deletion Preview "
        } else {
            " Confirm Permanent Deletion "
        };
        let accent = if dry_run_only {
            Color::Yellow
        } else {
            Color::Red
        };

        let mut lines = vec![
            Line::from(vec![
                Span::raw(if dry_run_only {
                    "Would delete "
                } else {
                    "Permanently delete "
                }),
                Span::styled(
                    format!("{} file(s)", targets.len()),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" and free "),
                Span::styled(
                    format_size(total_bytes, BINARY),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(":"),
            ]),
            Line::default(),
        ];

        let max_preview = popup.height.saturating_sub(7) as usize;
        for (path, size) in targets.iter().take(max_preview) {
            lines.push(Line::from(vec![
                Span::styled("  - ", Style::default().fg(accent)),
                Span::raw(format!(
                    "{} ({})",
                    path.display(),
                    format_size(*size, BINARY)
                )),
            ]));
        }
        if targets.len() > max_preview {
            lines.push(Line::from(Span::styled(
                format!("  ... and {} more file(s)", targets.len() - max_preview),
                Style::default().fg(Color::DarkGray),
            )));
        }

        lines.push(Line::default());
        lines.push(if dry_run_only {
            Line::from(Span::styled(
                "No files will be modified. Press [Esc] or [Enter] to close.",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(vec![
                Span::styled(
                    "[y / Enter] Delete Permanently",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw("    "),
                Span::styled(
                    "[n / Esc] Cancel",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        });

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent))
            .title(Span::styled(
                title,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

fn render_help_modal(frame: &mut Frame) {
    let popup = centered_rect(68, 17, frame.area());
    frame.render_widget(Clear, popup);

    let lines = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab / ← / → / h / l   Switch between Groups and File Copies panes"),
        Line::from("  ↑ / ↓ / j / k         Move selection up / down"),
        Line::from("  PgUp / PgDn / g / G   Page up/down or jump to start/end"),
        Line::default(),
        Line::from(Span::styled(
            "Selection & Cleanup (at least 1 copy is always preserved)",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  Space / Enter         Toggle deletion mark on highlighted file"),
        Line::from("  a                     Auto-mark all copies except first in current group"),
        Line::from("  A                     Auto-mark all copies except first across ALL groups"),
        Line::from("  u / U                 Unmark current group / unmark all groups"),
        Line::from("  d / Delete            Confirm & delete marked files (or dry-run preview)"),
        Line::from("  q / Esc               Close modal or exit TUI"),
        Line::default(),
        Line::from(Span::styled(
            "Press Esc, ?, or Enter to close.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " b3dup TUI Keybindings ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn clamp_move(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max_idx = (len - 1) as isize;
    (current as isize + delta).clamp(0, max_idx) as usize
}

fn truncate_left(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let tail: String = text.chars().skip(char_count - keep).collect();
    format!("…{tail}")
}

fn truncate_right(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let head: String = text.chars().take(keep).collect();
    format!("{head}…")
}

fn render_ratio_bar(value: usize, total: usize, width: usize) -> String {
    let clamped_total = total.max(1);
    let filled = ((value.min(clamped_total) * width) + clamped_total / 2) / clamped_total;
    let filled = filled.min(width);
    let empty = width.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let clamped_height = height.min(area.height.max(1));
    let top_pad = area.height.saturating_sub(clamped_height) / 2;
    let [_, mid, _] = Layout::vertical([
        Constraint::Length(top_pad),
        Constraint::Length(clamped_height),
        Constraint::Min(0),
    ])
    .areas(area);

    let side = 100_u16.saturating_sub(percent_x.min(100)) / 2;
    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage(side),
        Constraint::Percentage(percent_x.min(100)),
        Constraint::Percentage(side),
    ])
    .areas(mid);
    center
}

/// Marks every file in `group` except index 0, skipping any already marked.
fn mark_all_except_first(group: &DuplicateGroup, marked_paths: &mut HashSet<PathBuf>) -> usize {
    let mut newly_marked = 0;
    for path in group.files.iter().skip(1) {
        if marked_paths.insert(path.clone()) {
            newly_marked += 1;
        }
    }
    newly_marked
}

/// Removes deleted paths from `groups` and prunes any group left with fewer than 2 copies.
fn apply_deletions_to_groups(groups: &mut Vec<DuplicateGroup>, deleted_paths: &HashSet<PathBuf>) {
    for group in groups.iter_mut() {
        group.files.retain(|path| !deleted_paths.contains(path));
    }
    groups.retain(|group| group.files.len() >= 2);
}

/// Reads at most `PREVIEW_BYTES` from `path` for a compact hex + ASCII preview.
fn build_file_preview(path: &Path) -> Vec<String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) => return vec![format!("Cannot open file: {err}")],
    };

    let mut buffer = [0_u8; PREVIEW_BYTES];
    let mut total = 0;
    while total < buffer.len() {
        match file.read(&mut buffer[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return vec![format!("Read error: {err}")],
        }
    }

    if total == 0 {
        return vec!["(empty file)".to_string()];
    }

    let mut lines = Vec::new();
    for (row, chunk) in buffer[..total].chunks(8).enumerate() {
        let mut hex_part = String::new();
        let mut ascii_part = String::new();
        for &byte in chunk {
            hex_part.push_str(&format!("{byte:02x} "));
            let ch = if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '.'
            };
            ascii_part.push(ch);
        }
        lines.push(format!("{:04x}: {:<24} |{ascii_part}|", row * 8, hex_part));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StageMetrics;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample_report() -> ScanReport {
        ScanReport {
            scan_path: PathBuf::from("./test_data"),
            files_scanned: 10,
            skipped_files: 0,
            stage1: StageMetrics {
                candidates: 6,
                elapsed_ms: 2,
            },
            stage2: StageMetrics {
                candidates: 5,
                elapsed_ms: 3,
            },
            stage3: StageMetrics {
                candidates: 5,
                elapsed_ms: 4,
            },
            stage3_hashed_files: 5,
            total_elapsed_ms: 9,
            groups: vec![
                DuplicateGroup {
                    hash: [0xAA; 32],
                    size: 4096,
                    files: vec![
                        PathBuf::from("a/file1.bin"),
                        PathBuf::from("b/file1.bin"),
                        PathBuf::from("c/file1.bin"),
                    ],
                },
                DuplicateGroup {
                    hash: [0xBB; 32],
                    size: 1024,
                    files: vec![PathBuf::from("a/small.bin"), PathBuf::from("b/small.bin")],
                },
            ],
        }
    }

    #[test]
    fn tui_cannot_mark_all_copies_in_group() {
        let mut app = TuiApp::new(sample_report(), false);
        app.focus = FocusPane::Files;

        // Mark file 0 and file 1 in a 3-copy group -> allowed.
        app.toggle_mark_current();
        app.move_selection(1);
        app.toggle_mark_current();
        assert_eq!(app.marked_paths.len(), 2);

        // Attempt to mark the 3rd and final copy -> must be blocked.
        app.move_selection(1);
        app.toggle_mark_current();
        assert_eq!(app.marked_paths.len(), 2);
        assert!(app.status_is_warning);
    }

    #[test]
    fn auto_mark_and_deletion_cleanup_preserve_one_copy() {
        let mut app = TuiApp::new(sample_report(), false);
        app.auto_mark_all_groups();
        let (count, bytes) = app.marked_summary();
        assert_eq!(count, 3);
        assert_eq!(bytes, 2 * 4096 + 1024);

        // Simulate deleting the marked copies from group 1.
        let mut deleted = HashSet::new();
        deleted.insert(PathBuf::from("b/file1.bin"));
        deleted.insert(PathBuf::from("c/file1.bin"));
        apply_deletions_to_groups(&mut app.report.groups, &deleted);

        // Group 1 only has 1 copy left, so it is no longer a duplicate group.
        assert_eq!(app.report.groups.len(), 1);
        assert_eq!(app.report.groups[0].size, 1024);
    }

    #[test]
    fn tui_renders_cleanly_at_standard_and_compact_sizes() {
        let mut app = TuiApp::new(sample_report(), true);
        for (w, h) in [(110, 32), (60, 18)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
        }
    }
}
