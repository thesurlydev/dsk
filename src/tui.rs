//! Interactive browser for a scanned tree.

use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
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
use crate::scan::Node;

pub fn run(root: Node, root_path: &Path) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("dsk tui requires an interactive terminal; use plain `dsk` for reports");
    }
    let mut terminal = ratatui::try_init()?;
    let result = App::new(root, root_path).run(&mut terminal);
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
    state: TableState,
    quit: bool,
    modal: Option<Modal>,
    /// A confirmed clean request, executed by the event loop before the next
    /// frame so the screen can show that work is in progress.
    pending: Option<CleanTarget>,
    status: Option<String>,
}

enum Modal {
    Confirm(CleanTarget),
    Notice {
        title: String,
        lines: Vec<String>,
        ok: bool,
    },
}

struct CleanTarget {
    /// The selected directory, shown in the confirmation.
    path: PathBuf,
    manifest: PathBuf,
    /// Space the scan attributed to this directory before cleaning.
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
    fn new(root: Node, root_path: &Path) -> Self {
        let mut app = Self {
            root,
            root_path: root_path.to_path_buf(),
            stack: Vec::new(),
            order: Vec::new(),
            selected: 0,
            sort: SortKey::Size,
            show_files: true,
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
            if let Some(target) = self.pending.take() {
                self.run_clean(terminal, target)?;
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
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
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
            KeyCode::Char('c') => self.request_clean(),
            _ => {}
        }
    }

    fn handle_modal_key(&mut self, code: KeyCode) {
        match self.modal.take() {
            Some(Modal::Confirm(target)) => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.pending = Some(target);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.modal = Some(Modal::Confirm(target)),
            },
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

    fn request_clean(&mut self) {
        match self.clean_target() {
            Some(target) => self.modal = Some(Modal::Confirm(target)),
            None => {
                self.status = Some(
                    "select a Cargo project (Cargo.toml) or a cargo target/ to clean".to_string(),
                )
            }
        }
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
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
                let tag = node
                    .artifact
                    .map(|kind| format!("[{}]", kind.short()))
                    .unwrap_or_default();
                let bar = bar(node.size, total, 10);
                Row::new(vec![
                    Cell::from(Span::styled(
                        right(&bytes(node.size), 7),
                        Style::new().fg(Color::Cyan),
                    )),
                    Cell::from(right(&percent(node.size, total), 6)),
                    Cell::from(bar),
                    Cell::from(label),
                    Cell::from(Span::styled(tag, Style::new().fg(Color::Yellow))),
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
        .style(Style::new().fg(Color::DarkGray));

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
                    Style::new().fg(Color::DarkGray),
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
        let keys = Line::from(vec![
            Span::styled("enter/l", Style::new().fg(Color::Cyan)),
            Span::raw(" open  "),
            Span::styled("backspace/h", Style::new().fg(Color::Cyan)),
            Span::raw(" up  "),
            Span::styled("s", Style::new().fg(Color::Cyan)),
            Span::raw(format!(" sort: {}  ", sort_label(self.sort))),
            Span::styled("f", Style::new().fg(Color::Cyan)),
            Span::raw(format!(
                " files: {}  ",
                if self.show_files { "shown" } else { "hidden" }
            )),
            Span::styled("c", Style::new().fg(Color::Cyan)),
            Span::raw(" cargo clean  "),
            Span::styled("q", Style::new().fg(Color::Cyan)),
            Span::raw(" quit"),
        ]);

        let width = area.width.saturating_sub(4) as usize;
        let (detail, detail_style) = match &self.status {
            Some(status) => (status.clone(), Style::new().fg(Color::Yellow)),
            None => (self.detail_text(), Style::new().fg(Color::DarkGray)),
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
                text
            }
            None => "nothing selected".to_string(),
        }
    }

    fn draw_modal(&self, frame: &mut Frame) {
        let Some(modal) = &self.modal else {
            return;
        };
        let (title, lines, color) = match modal {
            Modal::Confirm(target) => {
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
                ("confirm cargo clean", lines, Color::Yellow)
            }
            Modal::Notice { title, lines, ok } => (
                title.as_str(),
                lines.clone(),
                if *ok { Color::Green } else { Color::Red },
            ),
        };

        let area = frame.area();
        let width = area.width.saturating_sub(8).clamp(24, 90).min(area.width);
        let height = (lines.len() as u16 + 2).min(area.height);
        let popup = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };

        let text_width = width.saturating_sub(4) as usize;
        let text: Vec<Line> = lines
            .into_iter()
            .map(|line| Line::from(truncate_right(&line, text_width)))
            .collect();

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

    #[test]
    fn navigation_walks_in_and_out() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path());
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
        let mut app = App::new(result.root, tmp.path());
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
    fn c_on_a_cargo_project_opens_confirmation() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path());
        app.handle_key(KeyCode::Char('c'));

        let Some(Modal::Confirm(target)) = &app.modal else {
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
        let mut app = App::new(result.root, tmp.path());
        app.handle_key(KeyCode::Enter);

        app.handle_key(KeyCode::Char('c'));
        let Some(Modal::Confirm(target)) = &app.modal else {
            panic!("expected a confirmation modal");
        };
        assert_eq!(target.path, tmp.path().join("project/target"));
        assert_eq!(target.manifest, tmp.path().join("project/Cargo.toml"));

        app.handle_key(KeyCode::Char('y'));
        assert!(app.modal.is_none());
        let pending = app.pending.as_ref().expect("clean queued");
        assert_eq!(pending.manifest, tmp.path().join("project/Cargo.toml"));
        assert!(
            tmp.path().join("project/target").exists(),
            "confirming must not delete anything by itself"
        );
    }

    #[test]
    fn escape_cancels_without_quitting() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path());
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
        let mut app = App::new(result.root, tmp.path());
        app.handle_key(KeyCode::Enter);
        app.handle_key(KeyCode::Down); // main.rs, a file
        app.handle_key(KeyCode::Char('c'));
        assert!(app.modal.is_none());
        assert!(app.status.is_some());
    }

    #[test]
    fn update_after_clean_drops_the_removed_target_from_the_tree() {
        let (tmp, result) = tree();
        let mut app = App::new(result.root, tmp.path());
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
        let mut app = App::new(result.root, tmp.path());
        app.handle_key(KeyCode::Enter);
        app.handle_key(KeyCode::Enter); // into target/
        assert_eq!(app.stack.len(), 2);

        fs::remove_dir_all(tmp.path().join("project/target")).unwrap();
        app.update_after_clean();

        assert_eq!(app.stack.len(), 1, "back at the nearest surviving parent");
        assert_eq!(app.current().name.as_ref(), "project");
    }
}
