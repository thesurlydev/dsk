//! Interactive browser for a scanned tree.

use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::artifact::{self, ArtifactKind};
use crate::format::{
    bytes, display_path, percent, plural, right, shell_path, truncate_left, truncate_right,
};
use crate::report::SortKey;
use crate::scan::{Node, NodeKind, ScanOptions};

pub fn run(root: Node, root_path: &Path, scan: ScanOptions) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("dsk tui requires an interactive terminal; use `dsk report` for a text summary");
    }
    let mut terminal = ratatui::try_init()?;
    let result = App::new(root, root_path, scan).run(&mut terminal);
    ratatui::restore();
    result
}

struct App {
    root: Node,
    root_path: PathBuf,
    /// Child indices from the root to the directory being viewed.
    stack: Vec<usize>,
    /// Indices into the current directory's children, in display order.
    order: Vec<usize>,
    selected: usize,
    sort: SortKey,
    show_files: bool,
    theme: Theme,
    /// Scan settings used when the user opens a different directory with `g`.
    scan: ScanOptions,
    state: TableState,
    quit: bool,
    modal: Option<Modal>,
    /// A confirmed action, executed by the event loop before the next frame so
    /// the screen can show that work is in progress.
    pending: Option<Action>,
    status: Option<String>,
}

enum Modal {
    Confirm(Confirmation),
    Go(GoPrompt),
    Notice {
        title: String,
        lines: Vec<String>,
        ok: bool,
    },
}

enum Confirmation {
    Clean(CleanTarget),
    Delete(DeleteTarget),
}

enum Action {
    Confirm(Confirmation),
    Go(String),
}

struct GoPrompt {
    input: Vec<char>,
    cursor: usize,
    error: Option<String>,
}

impl GoPrompt {
    fn new() -> Self {
        Self {
            input: Vec::new(),
            cursor: 0,
            error: None,
        }
    }

    fn with_error(input: &str, error: String) -> Self {
        let input: Vec<char> = input.chars().collect();
        Self {
            cursor: input.len(),
            input,
            error: Some(error),
        }
    }

    fn text(&self) -> String {
        self.input.iter().collect()
    }

    fn insert(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += 1;
        self.error = None;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
            self.error = None;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
            self.error = None;
        }
    }

    /// One editable line, with a reverse-video block at the cursor. Longer
    /// inputs keep the cursor in view by showing a window around it.
    fn line(&self, width: usize) -> Line<'static> {
        let mut spans = vec![Span::raw("> ")];
        let total = self.input.len() + 1;
        let start = if total <= width {
            0
        } else {
            (self.cursor + 1).saturating_sub(width)
        };
        let end = (start + width).min(total);
        for index in start..end {
            if index == self.cursor {
                let cell = self.input.get(index).copied().unwrap_or(' ');
                spans.push(Span::styled(
                    cell.to_string(),
                    Style::new().add_modifier(Modifier::REVERSED),
                ));
            } else if let Some(c) = self.input.get(index) {
                spans.push(Span::raw(c.to_string()));
            }
        }
        Line::from(spans)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Theme {
    Dark,
    Light,
}

impl Theme {
    fn toggle(&mut self) {
        *self = match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        };
    }

    fn label(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    /// Sizes and key hints. ANSI cyan washes out on a light background, so use
    /// blue there instead.
    fn accent(self) -> Color {
        match self {
            Theme::Dark => Color::Cyan,
            Theme::Light => Color::Blue,
        }
    }

    fn muted(self) -> Color {
        Color::DarkGray
    }

    /// Artifact tags and warning text.
    fn tag(self) -> Color {
        match self {
            Theme::Dark => Color::Yellow,
            Theme::Light => Color::Magenta,
        }
    }
}

struct CleanTarget {
    /// The selected directory, shown in the confirmation.
    path: PathBuf,
    manifest: PathBuf,
    /// Space the scan attributed to this directory before cleaning.
    estimate: u64,
}

struct DeleteTarget {
    path: PathBuf,
    kind: NodeKind,
    /// Space the scan attributed to this entry before deleting it.
    estimate: u64,
}

#[derive(Default)]
struct Removed {
    size: u64,
    files: u64,
    dirs: u64,
    reclaimable: u64,
}

impl Removed {
    fn add(&mut self, other: Self) {
        self.size += other.size;
        self.files += other.files;
        self.dirs += other.dirs;
        self.reclaimable += other.reclaimable;
    }
}

impl App {
    fn new(root: Node, root_path: &Path, scan: ScanOptions) -> Self {
        let mut app = Self {
            root,
            root_path: root_path.to_path_buf(),
            stack: Vec::new(),
            order: Vec::new(),
            selected: 0,
            sort: SortKey::Size,
            show_files: true,
            theme: Theme::Dark,
            scan,
            state: TableState::default(),
            quit: false,
            modal: None,
            pending: None,
            status: None,
        };
        app.refresh();
        app
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key.code);
            }
            if self.quit {
                return Ok(());
            }
            match self.pending.take() {
                Some(Action::Confirm(Confirmation::Clean(target))) => {
                    self.run_clean(terminal, target)?
                }
                Some(Action::Confirm(Confirmation::Delete(target))) => {
                    self.run_delete(terminal, target)?
                }
                Some(Action::Go(input)) => self.run_go(terminal, input)?,
                None => {}
            }
        }
    }

    fn current(&self) -> &Node {
        let mut node = &self.root;
        for index in &self.stack {
            node = &node.children[*index];
        }
        node
    }

    fn current_path(&self) -> PathBuf {
        let mut path = self.root_path.clone();
        let mut node = &self.root;
        for index in &self.stack {
            node = &node.children[*index];
            path.push(&*node.name);
        }
        path
    }

    fn selected_node(&self) -> Option<&Node> {
        self.order
            .get(self.selected)
            .map(|index| &self.current().children[*index])
    }

    fn selected_path(&self) -> Option<PathBuf> {
        let node = self.selected_node()?;
        let mut path = self.current_path();
        path.push(&*node.name);
        Some(path)
    }

    fn refresh(&mut self) {
        let order = {
            let current = self.current();
            let mut order: Vec<usize> = current
                .children
                .iter()
                .enumerate()
                .filter(|(_, node)| self.show_files || node.is_dir())
                .map(|(index, _)| index)
                .collect();
            let children = &current.children;
            match self.sort {
                SortKey::Size => order.sort_unstable_by(|a, b| {
                    children[*b]
                        .size
                        .cmp(&children[*a].size)
                        .then_with(|| children[*a].name.cmp(&children[*b].name))
                }),
                SortKey::Count => order.sort_unstable_by(|a, b| {
                    children[*b]
                        .files
                        .cmp(&children[*a].files)
                        .then_with(|| children[*b].size.cmp(&children[*a].size))
                        .then_with(|| children[*a].name.cmp(&children[*b].name))
                }),
                SortKey::Name => {
                    order.sort_unstable_by(|a, b| children[*a].name.cmp(&children[*b].name))
                }
            }
            order
        };

        self.order = order;
        if self.order.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.order.len() - 1);
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        if self.modal.is_some() {
            self.handle_modal_key(code);
            return;
        }
        self.status = None;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.order.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + 10).min(self.order.len().saturating_sub(1));
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(10);
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.order.len().saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(index) = self.order.get(self.selected).copied()
                    && self.current().children[index].is_dir()
                    && !self.current().children[index].children.is_empty()
                {
                    self.stack.push(index);
                    self.selected = 0;
                    self.refresh();
                }
            }
            KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => {
                if let Some(previous) = self.stack.pop() {
                    self.refresh();
                    if let Some(position) = self.order.iter().position(|index| *index == previous) {
                        self.selected = position;
                    }
                }
            }
            KeyCode::Char('s') => {
                self.sort = match self.sort {
                    SortKey::Size => SortKey::Count,
                    SortKey::Count => SortKey::Name,
                    SortKey::Name => SortKey::Size,
                };
                self.refresh();
            }
            KeyCode::Char('f') => {
                self.show_files = !self.show_files;
                self.refresh();
            }
            KeyCode::Char('t') => self.theme.toggle(),
            KeyCode::Char('g') => self.request_go(),
            KeyCode::Char('c') => self.request_clean(),
            KeyCode::Char('d') => self.request_delete(),
            _ => {}
        }
    }

    fn handle_modal_key(&mut self, code: KeyCode) {
        match self.modal.take() {
            Some(Modal::Confirm(action)) => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.pending = Some(Action::Confirm(action));
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.modal = Some(Modal::Confirm(action)),
            },
            // A path prompt captures every key, including the letters that
            // would otherwise navigate, so paths can contain anything.
            Some(Modal::Go(mut prompt)) => {
                match code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        if prompt.text().trim().is_empty() {
                            prompt.error = Some("enter a path".to_string());
                        } else {
                            self.pending = Some(Action::Go(prompt.text()));
                        }
                    }
                    KeyCode::Char(c) => prompt.insert(c),
                    KeyCode::Backspace => prompt.backspace(),
                    KeyCode::Delete => prompt.delete(),
                    KeyCode::Left => prompt.cursor = prompt.cursor.saturating_sub(1),
                    KeyCode::Right => {
                        prompt.cursor = (prompt.cursor + 1).min(prompt.input.len());
                    }
                    KeyCode::Home => prompt.cursor = 0,
                    KeyCode::End => prompt.cursor = prompt.input.len(),
                    _ => {}
                }
                if self.pending.is_none() {
                    self.modal = Some(Modal::Go(prompt));
                }
            }
            // Any key dismisses a result notice; it never falls through to the
            // navigation bindings.
            Some(Modal::Notice { .. }) | None => {}
        }
    }

    /// The Cargo clean this selection would run, if it is a Cargo project
    /// directory or a detected Cargo target directory.
    fn clean_target(&self) -> Option<CleanTarget> {
        let node = self.selected_node()?;
        if !node.is_dir() {
            return None;
        }
        let path = self.selected_path()?;

        if node.artifact == Some(ArtifactKind::RustTarget) {
            // Re-check the marker rather than trusting a possibly old scan.
            if artifact::detect(&path, node.name.as_ref()) != Some(ArtifactKind::RustTarget) {
                return None;
            }
            let manifest = path.parent()?.join("Cargo.toml");
            if !manifest.is_file() {
                return None;
            }
            return Some(CleanTarget {
                path,
                manifest,
                estimate: node.size,
            });
        }

        let manifest = path.join("Cargo.toml");
        manifest.is_file().then_some(CleanTarget {
            path,
            manifest,
            estimate: node.reclaimable,
        })
    }

    fn request_go(&mut self) {
        self.modal = Some(Modal::Go(GoPrompt::new()));
    }

    fn run_go(&mut self, terminal: &mut DefaultTerminal, input: String) -> Result<()> {
        self.modal = Some(Modal::Notice {
            title: "go".to_string(),
            lines: vec![
                format!("scanning {input}"),
                "this can take a moment".to_string(),
            ],
            ok: true,
        });
        terminal.draw(|frame| self.draw(frame))?;
        self.perform_go(&input);
        Ok(())
    }

    /// Resolve the entered path, rescan it, and make it the new root. Bad paths
    /// reopen the prompt with the error so the user can correct the input.
    fn perform_go(&mut self, input: &str) {
        let path = match resolve_go_path(&self.current_path(), input) {
            Ok(path) => path,
            Err(error) => {
                self.modal = Some(Modal::Go(GoPrompt::with_error(input, format!("{error:#}"))));
                return;
            }
        };

        let result = crate::scan::scan(&path, self.scan);
        let errors = result.stats.errors;

        self.root = result.root;
        self.root_path = path;
        self.stack.clear();
        self.order.clear();
        self.selected = 0;
        self.state = TableState::default();
        self.refresh();
        self.status = (errors > 0).then(|| {
            format!(
                "{} could not be read; totals exclude them",
                plural(errors, "path", "paths")
            )
        });
        self.modal = None;
    }

    fn request_clean(&mut self) {
        match self.clean_target() {
            Some(target) => self.modal = Some(Modal::Confirm(Confirmation::Clean(target))),
            None => {
                self.status = Some(
                    "select a Cargo project (Cargo.toml) or a cargo target/ to clean".to_string(),
                )
            }
        }
    }

    fn request_delete(&mut self) {
        let Some(node) = self.selected_node() else {
            self.status = Some("nothing selected to delete".to_string());
            return;
        };
        let kind = node.kind;
        let estimate = node.size;
        let Some(path) = self.selected_path() else {
            return;
        };
        self.modal = Some(Modal::Confirm(Confirmation::Delete(DeleteTarget {
            path,
            kind,
            estimate,
        })));
    }

    fn run_clean(&mut self, terminal: &mut DefaultTerminal, target: CleanTarget) -> Result<()> {
        self.modal = Some(Modal::Notice {
            title: "cargo clean".to_string(),
            lines: vec![
                format!("running cargo clean for {}", display_path(&target.path)),
                "this can take a moment".to_string(),
            ],
            ok: true,
        });
        terminal.draw(|frame| self.draw(frame))?;

        let output = Command::new("cargo")
            .arg("clean")
            .arg("--manifest-path")
            .arg(&target.manifest)
            .current_dir(target.manifest.parent().unwrap_or_else(|| Path::new(".")))
            .output();

        let (title, ok, mut lines) = match output {
            Ok(output) if output.status.success() => {
                let mut lines = vec![format!("cleaned {}", display_path(&target.path))];
                if target.estimate > 0 {
                    lines.push(format!("about {} reclaimed", bytes(target.estimate)));
                }
                push_output(&mut lines, &output.stderr);
                push_output(&mut lines, &output.stdout);
                ("cargo clean finished", true, lines)
            }
            Ok(output) => {
                let mut lines = vec![format!("exit status: {}", output.status)];
                push_output(&mut lines, &output.stderr);
                push_output(&mut lines, &output.stdout);
                ("cargo clean failed", false, lines)
            }
            Err(error) => (
                "cargo clean failed",
                false,
                vec![format!("could not run cargo: {error}")],
            ),
        };
        lines.push(String::new());
        lines.push("press any key to dismiss".to_string());

        self.update_after_clean();
        self.modal = Some(Modal::Notice {
            title: title.to_string(),
            lines,
            ok,
        });
        Ok(())
    }

    fn run_delete(&mut self, terminal: &mut DefaultTerminal, target: DeleteTarget) -> Result<()> {
        self.modal = Some(Modal::Notice {
            title: "delete".to_string(),
            lines: vec![
                format!("deleting {}", display_path(&target.path)),
                "this can take a moment".to_string(),
            ],
            ok: true,
        });
        terminal.draw(|frame| self.draw(frame))?;
        self.perform_delete(target);
        Ok(())
    }

    /// Delete the confirmed selection and drop it from the in-memory tree. Split
    /// from `run_delete` so tests can exercise the whole path without a terminal.
    fn perform_delete(&mut self, target: DeleteTarget) {
        let (title, ok, mut lines) = match delete_path(&self.root_path, &target.path, target.kind) {
            Ok(()) => {
                let mut lines = vec![format!("deleted {}", display_path(&target.path))];
                if target.estimate > 0 {
                    lines.push(format!("about {} freed", bytes(target.estimate)));
                }
                ("delete finished", true, lines)
            }
            Err(error) => ("delete failed", false, vec![format!("{error:#}")]),
        };
        lines.push(String::new());
        lines.push("press any key to dismiss".to_string());

        if ok {
            self.update_after_removal(&target.path);
        }
        self.modal = Some(Modal::Notice {
            title: title.to_string(),
            lines,
            ok,
        });
    }

    /// Drop the subtree at `target` from the in-memory tree, adjusting sizes,
    /// file counts, and reclaimable totals along the path to it. Only the
    /// ancestors are visited; the rest of the tree is left untouched.
    fn update_after_removal(&mut self, target: &Path) {
        let names = self.stack_names();
        let selected = self.selected_node().map(|node| node.name.to_string());

        let mut path = self.root_path.clone();
        Self::prune_path(&mut self.root, &mut path, target);

        self.restore_navigation(names, selected);
        self.status = None;
    }

    fn prune_path(node: &mut Node, path: &mut PathBuf, target: &Path) -> Removed {
        let children = std::mem::take(&mut node.children);
        let mut kept = Vec::with_capacity(children.len());
        let mut removed = Removed::default();
        for mut child in children.into_vec() {
            path.push(&*child.name);
            if path == target {
                removed.size += child.size;
                removed.files += child.files;
                if child.is_dir() {
                    removed.dirs += child.dirs + 1;
                }
                removed.reclaimable += child.reclaimable;
            } else {
                if target.starts_with(&*path) {
                    removed.add(Self::prune_path(&mut child, path, target));
                }
                kept.push(child);
            }
            path.pop();
        }
        node.children = kept.into_boxed_slice();
        node.size = node.size.saturating_sub(removed.size);
        node.files = node.files.saturating_sub(removed.files);
        node.dirs = node.dirs.saturating_sub(removed.dirs);
        node.reclaimable = node.reclaimable.saturating_sub(removed.reclaimable);
        removed
    }

    /// Drop cargo target subtrees that no longer exist on disk, without
    /// rescanning. Sizes, file counts, and reclaimable totals are adjusted
    /// along the path to each removed directory.
    fn update_after_clean(&mut self) {
        let names = self.stack_names();
        let selected = self.selected_node().map(|node| node.name.to_string());

        let mut path = self.root_path.clone();
        Self::prune_node(&mut self.root, &mut path);

        self.restore_navigation(names, selected);
        self.status = None;
    }

    fn prune_node(node: &mut Node, path: &mut PathBuf) -> Removed {
        let children = std::mem::take(&mut node.children);
        let mut kept = Vec::with_capacity(children.len());
        let mut removed = Removed::default();
        for mut child in children.into_vec() {
            path.push(&*child.name);
            if child.artifact == Some(ArtifactKind::RustTarget) && !directory_exists(path) {
                removed.size += child.size;
                removed.files += child.files;
                removed.dirs += child.dirs + 1;
                removed.reclaimable += child.reclaimable;
            } else {
                let nested = Self::prune_node(&mut child, path);
                removed.add(nested);
                kept.push(child);
            }
            path.pop();
        }
        node.children = kept.into_boxed_slice();
        node.size = node.size.saturating_sub(removed.size);
        node.files = node.files.saturating_sub(removed.files);
        node.dirs = node.dirs.saturating_sub(removed.dirs);
        node.reclaimable = node.reclaimable.saturating_sub(removed.reclaimable);
        removed
    }

    fn stack_names(&self) -> Vec<String> {
        let mut names = Vec::with_capacity(self.stack.len());
        let mut node = &self.root;
        for index in &self.stack {
            node = &node.children[*index];
            names.push(node.name.to_string());
        }
        names
    }

    /// Rebuild the navigation stack by name after child indices may have
    /// shifted. Stops at the nearest ancestor that still exists, which moves
    /// the view out of a directory that was just deleted.
    fn restore_navigation(&mut self, names: Vec<String>, selected: Option<String>) {
        let mut indices = Vec::with_capacity(names.len());
        {
            let mut node = &self.root;
            for name in names {
                let Some(index) = node
                    .children
                    .iter()
                    .position(|child| child.is_dir() && child.name.as_ref() == name.as_str())
                else {
                    break;
                };
                indices.push(index);
                node = &node.children[index];
            }
        }
        self.stack = indices;
        self.refresh();
        if let Some(name) = selected
            && let Some(position) = self
                .order
                .iter()
                .position(|index| self.current().children[*index].name.as_ref() == name.as_str())
        {
            self.selected = position;
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(frame.area());

        self.draw_header(frame, chunks[0]);
        self.draw_table(frame, chunks[1]);
        self.draw_footer(frame, chunks[2]);
        self.draw_modal(frame);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let current = self.current();
        let path = self.current_path();
        let title = format!(
            " dsk | {} ",
            truncate_left(
                &path.display().to_string(),
                area.width.saturating_sub(9) as usize
            )
        );
        let line = Line::from(vec![
            Span::styled(
                bytes(current.size),
                Style::new()
                    .fg(self.theme.accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " in {}, {}",
                plural(current.files, "file", "files"),
                plural(current.dirs, "dir", "dirs")
            )),
            Span::styled(
                format!("  |  {} reclaimable here", bytes(current.reclaimable)),
                Style::new().fg(Color::Green),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line).block(Block::bordered().title(title)),
            area,
        );
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let current = self.current();
        let total = current.size;
        let rows: Vec<Row> = self
            .order
            .iter()
            .map(|index| {
                let node = &current.children[*index];
                let label = if node.is_dir() {
                    format!("{}/", node.name)
                } else {
                    node.name.to_string()
                };
                let tag = match (node.artifact, node.shared) {
                    (Some(kind), _) => format!("[{}]", kind.short()),
                    (None, true) => "[link]".to_string(),
                    (None, false) => String::new(),
                };
                let bar = bar(node.size, total, 10);
                Row::new(vec![
                    Cell::from(Span::styled(
                        right(&bytes(node.size), 7),
                        Style::new().fg(self.theme.accent()),
                    )),
                    Cell::from(right(&percent(node.size, total), 6)),
                    Cell::from(bar),
                    Cell::from(label),
                    Cell::from(Span::styled(tag, Style::new().fg(self.theme.tag()))),
                ])
            })
            .collect();

        let header = Row::new(vec![
            Cell::from(right("size", 7)),
            Cell::from(right("share", 6)),
            Cell::from("usage"),
            Cell::from("name"),
            Cell::from("kind"),
        ])
        .style(Style::new().fg(self.theme.muted()));

        let table = Table::new(
            rows,
            [
                Constraint::Length(7),
                Constraint::Length(6),
                Constraint::Length(10),
                Constraint::Min(10),
                Constraint::Length(9),
            ],
        )
        .header(header)
        .block(Block::bordered())
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));

        if self.order.is_empty() {
            self.state.select(None);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  empty directory",
                    Style::new().fg(self.theme.muted()),
                ))
                .block(Block::bordered().title(" size    share  usage       name")),
                area,
            );
            return;
        }
        self.state.select(Some(self.selected));
        frame.render_stateful_widget(table, area, &mut self.state);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let accent = self.theme.accent();
        let keys = Line::from(vec![
            Span::styled("enter/l", Style::new().fg(accent)),
            Span::raw(" open  "),
            Span::styled("g", Style::new().fg(accent)),
            Span::raw(" go  "),
            Span::styled("backspace/h", Style::new().fg(accent)),
            Span::raw(" up  "),
            Span::styled("s", Style::new().fg(accent)),
            Span::raw(format!(" sort: {}  ", sort_label(self.sort))),
            Span::styled("f", Style::new().fg(accent)),
            Span::raw(format!(
                " files: {}  ",
                if self.show_files { "shown" } else { "hidden" }
            )),
            Span::styled("c", Style::new().fg(accent)),
            Span::raw(" cargo clean  "),
            Span::styled("d", Style::new().fg(accent)),
            Span::raw(" delete  "),
            Span::styled("t", Style::new().fg(accent)),
            Span::raw(format!(" theme: {}  ", self.theme.label())),
            Span::styled("q", Style::new().fg(accent)),
            Span::raw(" quit"),
        ]);

        let width = area.width.saturating_sub(4) as usize;
        let (detail, detail_style) = match &self.status {
            Some(status) => (status.clone(), Style::new().fg(self.theme.tag())),
            None => (self.detail_text(), Style::new().fg(self.theme.muted())),
        };

        let paragraph = Paragraph::new(vec![
            keys,
            Line::from(Span::styled(truncate_right(&detail, width), detail_style)),
        ])
        .block(Block::bordered());
        frame.render_widget(paragraph, area);
    }

    fn detail_text(&self) -> String {
        match self.selected_node() {
            Some(node) => {
                let mut path = self.current_path();
                path.push(&*node.name);
                let mut text = format!(
                    "{}  {}  {} of this directory",
                    path.display(),
                    bytes(node.size),
                    percent(node.size, self.current().size),
                );
                if let Some(kind) = node.artifact {
                    text.push_str(&format!("  [{}]", kind.short()));
                    if let Some(command) = kind.clean_command(&path) {
                        text.push_str(&format!("  {command}"));
                    }
                }
                if node.shared {
                    text.push_str("  [link] blocks counted under another path");
                }
                text
            }
            None => "nothing selected".to_string(),
        }
    }

    fn draw_modal(&self, frame: &mut Frame) {
        let Some(modal) = &self.modal else {
            return;
        };

        let area = frame.area();
        let width = area.width.saturating_sub(8).clamp(24, 90).min(area.width);
        let text_width = width.saturating_sub(4) as usize;

        let (title, color, text) = match modal {
            Modal::Confirm(Confirmation::Clean(target)) => {
                let mut lines = vec![
                    format!(
                        "cargo clean --manifest-path {}",
                        shell_path(&target.manifest)
                    ),
                    format!("directory: {}", display_path(&target.path)),
                ];
                if target.estimate > 0 {
                    lines.push(format!("estimated reclaimable: {}", bytes(target.estimate)));
                }
                lines.push(String::new());
                lines.push("y / enter  run     n / esc  cancel".to_string());
                (
                    "confirm cargo clean",
                    self.theme.tag(),
                    plain_lines(lines, text_width),
                )
            }
            Modal::Confirm(Confirmation::Delete(target)) => {
                let lines = vec![
                    format!("delete {}", display_path(&target.path)),
                    format!(
                        "{} of {}",
                        delete_kind_label(target.kind),
                        bytes(target.estimate)
                    ),
                    "this cannot be undone".to_string(),
                    String::new(),
                    "y / enter  delete     n / esc  cancel".to_string(),
                ];
                ("confirm delete", Color::Red, plain_lines(lines, text_width))
            }
            Modal::Notice { title, lines, ok } => (
                title.as_str(),
                if *ok { Color::Green } else { Color::Red },
                plain_lines(lines.clone(), text_width),
            ),
            Modal::Go(prompt) => {
                let mut lines = vec![
                    Line::from(Span::styled(
                        truncate_right(
                            &format!("from {}", display_path(&self.current_path())),
                            text_width,
                        ),
                        Style::new().fg(self.theme.muted()),
                    )),
                    prompt.line(text_width.saturating_sub(2)),
                ];
                if let Some(error) = &prompt.error {
                    lines.push(Line::from(Span::styled(
                        truncate_right(error, text_width),
                        Style::new().fg(Color::Red),
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    truncate_right(
                        "enter  go     esc  cancel     ~ expands to your home directory",
                        text_width,
                    ),
                    Style::new().fg(self.theme.muted()),
                )));
                ("go to directory", self.theme.accent(), lines)
            }
        };

        let height = (text.len() as u16 + 2).min(area.height);
        let popup = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(text).block(
                Block::bordered()
                    .title(format!(" {title} "))
                    .border_style(Style::new().fg(color)),
            ),
            popup,
        );
    }
}

fn plain_lines(lines: Vec<String>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| Line::from(truncate_right(&line, width)))
        .collect()
}

/// Resolve a path typed in the `go` prompt. Relative paths are joined to the
/// directory currently on screen, and a leading `~` means the home directory.
fn resolve_go_path(base: &Path, input: &str) -> Result<PathBuf> {
    if input.trim().is_empty() {
        bail!("enter a path");
    }
    let expanded = expand_tilde(input);
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    let canonical = fs::canonicalize(&candidate)
        .with_context(|| format!("cannot access {}", candidate.display()))?;
    ensure!(
        canonical.is_dir(),
        "{} is not a directory",
        canonical.display()
    );
    Ok(canonical)
}

fn expand_tilde(input: &str) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
    };
    if input == "~" {
        return home().unwrap_or_else(|| PathBuf::from(input));
    }
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = home()
    {
        return home.join(rest);
    }
    PathBuf::from(input)
}

fn push_output(lines: &mut Vec<String>, output: &[u8]) {
    let text = String::from_utf8_lossy(output);
    for line in text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(3)
    {
        lines.push(line.to_string());
    }
}

fn directory_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Delete one selected entry after re-checking that it still exists, still has
/// the same directory-ness the scan recorded, and resolves inside the scan root.
/// Symlinks are unlinked, never followed.
fn delete_path(root: &Path, path: &Path, expected: NodeKind) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let file_type = metadata.file_type();
    let current = if file_type.is_symlink() {
        NodeKind::Symlink
    } else if file_type.is_dir() {
        NodeKind::Dir
    } else if file_type.is_file() {
        NodeKind::File
    } else {
        NodeKind::Other
    };
    ensure!(
        (current == NodeKind::Dir) == (expected == NodeKind::Dir),
        "{} changed type since it was scanned",
        path.display()
    );

    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let canonical = if current == NodeKind::Symlink {
        // Canonicalizing a symlink resolves its target, which may be outside the
        // scan root. Check the containing directory instead and unlink the link.
        let parent = path
            .parent()
            .with_context(|| format!("{} has no parent", path.display()))?;
        let canonical_parent =
            fs::canonicalize(parent).with_context(|| format!("resolving {}", parent.display()))?;
        ensure!(
            canonical_parent.starts_with(&canonical_root),
            "refusing to delete outside the scan root"
        );
        canonical_parent.join(
            path.file_name()
                .with_context(|| format!("{} has no file name", path.display()))?,
        )
    } else {
        fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?
    };
    ensure!(
        canonical != canonical_root,
        "refusing to delete the scan root itself"
    );
    ensure!(
        canonical.starts_with(&canonical_root),
        "refusing to delete outside the scan root"
    );
    ensure!(
        canonical.parent().is_some_and(|parent| parent != canonical),
        "refusing to delete a filesystem root"
    );

    if current == NodeKind::Dir {
        fs::remove_dir_all(&canonical)
            .with_context(|| format!("removing {}", canonical.display()))?;
    } else {
        fs::remove_file(&canonical).with_context(|| format!("removing {}", canonical.display()))?;
    }
    Ok(())
}

fn delete_kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Dir => "directory",
        NodeKind::File => "file",
        NodeKind::Symlink => "symlink",
        NodeKind::Other => "special file",
    }
}

fn sort_label(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Size => "size",
        SortKey::Count => "count",
        SortKey::Name => "name",
    }
}

fn bar(part: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((part as f64 / total as f64) * width as f64).round() as usize
    }
    .min(width);
    format!("{}{}", "#".repeat(filled), ".".repeat(width - filled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    fn tree() -> (tempfile::TempDir, crate::scan::ScanResult) {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\nname=\"p\"\n").unwrap();
        fs::write(project.join("target/blob"), vec![0u8; 128 * 1024]).unwrap();
        fs::write(project.join("main.rs"), b"fn main() {}").unwrap();
        let result = scan(tmp.path(), ScanOptions::default());
        (tmp, result)
    }

    fn select(app: &mut App, name: &str) {
        app.selected = app
            .order
            .iter()
            .position(|index| app.current().children[*index].name.as_ref() == name)
            .unwrap_or_else(|| panic!("{name} is not in the current directory"));
    }

    #[test]
    fn navigation_walks_in_and_out() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        assert_eq!(app.order.len(), 1, "only the project dir at the top level");
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.stack.len(), 1);
        // Inside project: target/ and main.rs, size order puts target first.
        assert_eq!(app.current().children[app.order[0]].name.as_ref(), "target");
        app.handle_key(KeyCode::Down);
        app.handle_key(KeyCode::Backspace);
        assert!(app.stack.is_empty());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn toggling_files_hides_them() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);
        let dirs = app
            .current()
            .children
            .iter()
            .filter(|node| node.is_dir())
            .count();
        app.handle_key(KeyCode::Char('f'));
        assert_eq!(app.order.len(), dirs);
        assert!(
            app.order
                .iter()
                .all(|index| app.current().children[*index].is_dir())
        );
    }

    #[test]
    fn t_toggles_between_dark_and_light() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        assert_eq!(app.theme, Theme::Dark);
        assert_ne!(Theme::Dark.accent(), Theme::Light.accent());

        app.handle_key(KeyCode::Char('t'));
        assert_eq!(app.theme, Theme::Light);
        app.handle_key(KeyCode::Char('t'));
        assert_eq!(app.theme, Theme::Dark);
    }

    #[test]
    fn c_on_a_cargo_project_opens_confirmation() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Char('c'));

        let Some(Modal::Confirm(Confirmation::Clean(target))) = &app.modal else {
            panic!("expected a confirmation modal");
        };
        assert_eq!(
            target.manifest,
            tmp.path().join("project/Cargo.toml"),
            "confirms against the project manifest"
        );
        assert_eq!(target.path, tmp.path().join("project"));
        assert!(target.estimate > 0);
    }

    #[test]
    fn c_on_a_target_opens_confirmation_and_only_queues_on_confirm() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);

        app.handle_key(KeyCode::Char('c'));
        let Some(Modal::Confirm(Confirmation::Clean(target))) = &app.modal else {
            panic!("expected a confirmation modal");
        };
        assert_eq!(target.path, tmp.path().join("project/target"));
        assert_eq!(target.manifest, tmp.path().join("project/Cargo.toml"));

        app.handle_key(KeyCode::Char('y'));
        assert!(app.modal.is_none());
        let Some(Action::Confirm(Confirmation::Clean(pending))) = app.pending.as_ref() else {
            panic!("clean queued");
        };
        assert_eq!(pending.manifest, tmp.path().join("project/Cargo.toml"));
        assert!(
            tmp.path().join("project/target").exists(),
            "confirming must not delete anything by itself"
        );
    }

    #[test]
    fn g_opens_the_go_prompt_and_editing_works() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());

        app.handle_key(KeyCode::Char('g'));
        let Some(Modal::Go(prompt)) = &app.modal else {
            panic!("expected a go prompt");
        };
        assert!(prompt.input.is_empty());

        for c in "/tmp/abc".chars() {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Backspace);
        app.handle_key(KeyCode::Left);
        app.handle_key(KeyCode::Char('x'));

        let Some(Modal::Go(prompt)) = &app.modal else {
            panic!("the prompt stays open while editing");
        };
        assert_eq!(prompt.text(), "/tmp/axb");
        assert_eq!(prompt.cursor, 7);

        app.handle_key(KeyCode::Esc);
        assert!(app.modal.is_none());
    }

    #[test]
    fn go_paths_are_relative_to_the_given_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let target = tmp.path().join("target");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(tmp.path().join("file"), b"x").unwrap();

        assert_eq!(
            resolve_go_path(&base, "../target").unwrap(),
            fs::canonicalize(&target).unwrap()
        );
        assert_eq!(
            resolve_go_path(&base, target.to_str().unwrap()).unwrap(),
            fs::canonicalize(&target).unwrap()
        );
        assert!(resolve_go_path(&base, "").is_err());

        let error = resolve_go_path(&base, "../file").unwrap_err();
        assert!(error.to_string().contains("not a directory"));
    }

    #[test]
    fn tilde_expands_to_the_home_directory() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        assert_eq!(expand_tilde("~/dsk-test"), home.join("dsk-test"));
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~user"), PathBuf::from("~user"));
    }

    #[test]
    fn go_switches_the_root_and_resets_navigation() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        fs::create_dir_all(first.join("nested")).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("blob"), vec![0u8; 64 * 1024]).unwrap();

        let result = scan(tmp.path(), ScanOptions::default());
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.stack.len(), 1);

        app.request_go();
        for c in second.to_string_lossy().chars() {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Enter);
        let Some(Action::Go(input)) = app.pending.take() else {
            panic!("go queued");
        };
        app.perform_go(&input);

        assert_eq!(app.root_path, fs::canonicalize(&second).unwrap());
        assert!(app.stack.is_empty());
        assert_eq!(app.selected, 0);
        assert!(app.modal.is_none());
        assert_eq!(app.current().children[app.order[0]].name.as_ref(), "blob");
    }

    #[test]
    fn shared_hard_link_rows_are_tagged_and_explained() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("alpha")).unwrap();
        fs::create_dir_all(tmp.path().join("beta")).unwrap();
        let original = tmp.path().join("alpha/one");
        fs::write(&original, vec![1u8; 64 * 1024]).unwrap();
        fs::hard_link(&original, tmp.path().join("beta/two")).unwrap();

        let result = scan(tmp.path(), ScanOptions::default());
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.selected = app
            .order
            .iter()
            .position(|index| app.current().children[*index].name.as_ref() == "beta")
            .unwrap();
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.current().name.as_ref(), "beta");
        app.selected = 0;
        assert!(app.selected_node().unwrap().shared);

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("[link]"), "{screen}");
        assert!(screen.contains("counted under another path"), "{screen}");
    }

    #[test]
    fn go_with_a_bad_path_keeps_the_prompt_open() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        let missing = tmp.path().join("missing");

        app.request_go();
        for c in missing.to_string_lossy().chars() {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Enter);
        let Some(Action::Go(input)) = app.pending.take() else {
            panic!("go queued");
        };
        app.perform_go(&input);

        let Some(Modal::Go(prompt)) = &app.modal else {
            panic!("the prompt stays open after a bad path");
        };
        assert_eq!(prompt.text(), input);
        assert!(
            prompt
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cannot access"))
        );
        assert_eq!(app.root_path, tmp.path());
    }

    #[test]
    fn escape_cancels_without_quitting() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Char('c'));
        assert!(app.modal.is_some());
        app.handle_key(KeyCode::Esc);
        assert!(app.modal.is_none());
        assert!(app.pending.is_none());
        assert!(!app.quit);
    }

    #[test]
    fn c_on_something_uncleanable_explains_why() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);
        app.handle_key(KeyCode::Down); // main.rs, a file
        app.handle_key(KeyCode::Char('c'));
        assert!(app.modal.is_none());
        assert!(app.status.is_some());
    }

    #[test]
    fn update_after_clean_drops_the_removed_target_from_the_tree() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);
        let before = app.current().size;
        assert!(app.current().reclaimable > 0);

        fs::remove_dir_all(tmp.path().join("project/target")).unwrap();
        app.update_after_clean();

        assert_eq!(app.stack.len(), 1, "still viewing the project");
        assert!(
            app.current()
                .children
                .iter()
                .all(|child| child.name.as_ref() != "target")
        );
        assert!(app.current().size < before);
        assert_eq!(app.current().reclaimable, 0);
        assert_eq!(app.root.reclaimable, 0);
    }

    #[test]
    fn update_after_clean_moves_out_of_a_deleted_target() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);
        app.handle_key(KeyCode::Enter); // into target/
        assert_eq!(app.stack.len(), 2);

        fs::remove_dir_all(tmp.path().join("project/target")).unwrap();
        app.update_after_clean();

        assert_eq!(app.stack.len(), 1, "back at the nearest surviving parent");
        assert_eq!(app.current().name.as_ref(), "project");
    }

    #[test]
    fn d_on_a_file_confirms_and_only_deletes_after_confirming() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);
        select(&mut app, "main.rs");

        let file = tmp.path().join("project/main.rs");
        app.handle_key(KeyCode::Char('d'));
        let Some(Modal::Confirm(Confirmation::Delete(target))) = &app.modal else {
            panic!("expected a confirmation modal");
        };
        assert_eq!(target.path, file);
        assert_eq!(target.kind, NodeKind::File);

        app.handle_key(KeyCode::Char('y'));
        let Some(Action::Confirm(Confirmation::Delete(target))) = app.pending.take() else {
            panic!("delete queued");
        };
        assert!(
            file.exists(),
            "confirming must not delete anything by itself"
        );

        app.perform_delete(target);
        assert!(!file.exists());
        assert!(
            app.current()
                .children
                .iter()
                .all(|child| child.name.as_ref() != "main.rs")
        );
    }

    #[test]
    fn d_on_a_directory_deletes_the_subtree_and_updates_totals() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);
        select(&mut app, "target");
        let before = app.current().size;
        assert!(app.current().reclaimable > 0);

        app.handle_key(KeyCode::Char('d'));
        let Some(Modal::Confirm(Confirmation::Delete(target))) = app.modal.take() else {
            panic!("expected a confirmation modal");
        };
        app.perform_delete(target);

        assert!(!tmp.path().join("project/target").exists());
        assert!(
            app.current()
                .children
                .iter()
                .all(|child| child.name.as_ref() != "target")
        );
        assert!(app.current().size < before);
        assert_eq!(app.current().reclaimable, 0);
        assert_eq!(app.root.reclaimable, 0);
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_symlink_keeps_its_target() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("keep.txt");
        fs::write(&target, b"keep").unwrap();
        fs::create_dir_all(tmp.path().join("dir")).unwrap();
        symlink(&target, tmp.path().join("dir/link")).unwrap();

        let result = scan(tmp.path(), ScanOptions::default());
        let mut app = App::new(result.root, tmp.path(), ScanOptions::default());
        app.handle_key(KeyCode::Enter);
        select(&mut app, "link");
        app.handle_key(KeyCode::Char('d'));
        let Some(Modal::Confirm(Confirmation::Delete(delete))) = app.modal.take() else {
            panic!("expected a confirmation modal");
        };
        app.perform_delete(delete);

        assert!(fs::symlink_metadata(tmp.path().join("dir/link")).is_err());
        assert!(target.exists(), "the link target must survive");
    }

    #[test]
    fn delete_refuses_the_scan_root() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("file"), b"x").unwrap();

        let error = delete_path(tmp.path(), tmp.path(), NodeKind::Dir).unwrap_err();
        assert!(error.to_string().contains("scan root"));
        assert!(tmp.path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn delete_refuses_a_symlink_where_a_directory_was_scanned() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep.txt"), b"keep").unwrap();
        let link = root.path().join("target");
        symlink(outside.path(), &link).unwrap();

        let error = delete_path(root.path(), &link, NodeKind::Dir).unwrap_err();
        assert!(error.to_string().contains("changed type"));
        assert!(link.exists());
        assert!(outside.path().join("keep.txt").exists());
    }
}
