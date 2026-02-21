use std::cmp::Reverse;
use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use tokio::sync::mpsc;
use tokio::time;

use crate::config::{
    add_persistent_forward_rule, add_persistent_serve_rule, remove_persistent_forward_rule,
};
use crate::control::{self, ActiveConnection, ForwardRoute, ServeRoute, Status};
use crate::remote_path::RemotePath;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Services,
    Forwards,
    Connections,
}

impl Pane {
    fn next(self) -> Self {
        match self {
            Self::Services => Self::Forwards,
            Self::Forwards => Self::Connections,
            Self::Connections => Self::Services,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Services => Self::Connections,
            Self::Forwards => Self::Services,
            Self::Connections => Self::Forwards,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Full,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactFocus {
    Routes,
    Connections,
}

impl CompactFocus {
    fn toggle(self) -> Self {
        match self {
            Self::Routes => Self::Connections,
            Self::Connections => Self::Routes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoutesView {
    Services,
    Forwards,
}

impl RoutesView {
    fn toggle(self) -> Self {
        match self {
            Self::Services => Self::Forwards,
            Self::Forwards => Self::Services,
        }
    }

    fn as_pane(self) -> Pane {
        match self {
            Self::Services => Pane::Services,
            Self::Forwards => Pane::Forwards,
        }
    }
}

#[derive(Debug, Clone)]
struct AddServiceDialog {
    name: String,
    target: String,
    persist: bool,
    field: usize,
}

#[derive(Debug, Clone)]
struct AddForwardDialog {
    listen: String,
    remote: String,
    persist: bool,
    field: usize,
}

#[derive(Debug, Clone)]
struct RemoveForwardDialog {
    listen: Box<str>,
    remote: Box<str>,
    remove_persistently: bool,
}

#[derive(Debug, Clone)]
enum Modal {
    AddService(AddServiceDialog),
    AddForward(AddForwardDialog),
    RemoveService { name: Box<str> },
    RemoveForward(RemoveForwardDialog),
    Message { title: Box<str>, body: Box<str> },
}

#[derive(Debug, Default)]
struct Snapshot {
    status: Option<Status>,
    services: Vec<ServeRoute>,
    forwards: Vec<ForwardRoute>,
    connections: Vec<ActiveConnection>,
}

#[derive(Debug)]
struct App {
    status: Option<Status>,
    services: Vec<ServeRoute>,
    forwards: Vec<ForwardRoute>,
    connections: Vec<ActiveConnection>,
    focus: Pane,
    compact_focus: CompactFocus,
    compact_routes: RoutesView,
    selected_service: Option<usize>,
    selected_forward: Option<usize>,
    selected_connection: Option<usize>,
    modal: Option<Modal>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            status: None,
            services: Vec::new(),
            forwards: Vec::new(),
            connections: Vec::new(),
            focus: Pane::Services,
            compact_focus: CompactFocus::Routes,
            compact_routes: RoutesView::Services,
            selected_service: None,
            selected_forward: None,
            selected_connection: None,
            modal: None,
        }
    }
}

impl App {
    fn apply_snapshot(&mut self, mut snapshot: Snapshot) {
        snapshot.services.sort_by(|a, b| a.name.cmp(&b.name));
        snapshot.forwards.sort_by(|a, b| a.listen.cmp(&b.listen));
        snapshot.connections.sort_by_key(|conn| Reverse(conn.id));

        self.status = snapshot.status;
        self.services = snapshot.services;
        self.forwards = snapshot.forwards;
        self.connections = snapshot.connections;

        self.selected_service = clamp_selection(self.selected_service, self.services.len());
        self.selected_forward = clamp_selection(self.selected_forward, self.forwards.len());
        self.selected_connection =
            clamp_selection(self.selected_connection, self.connections.len());
    }

    fn set_error(&mut self, err: impl std::fmt::Display) {
        self.modal = Some(Modal::Message {
            title: "Error".into(),
            body: err.to_string().into(),
        });
    }

    fn set_info(&mut self, msg: impl Into<String>) {
        self.modal = Some(Modal::Message {
            title: "Info".into(),
            body: msg.into().into(),
        });
    }

    fn active_pane(&self, layout_mode: LayoutMode) -> Pane {
        match layout_mode {
            LayoutMode::Full => self.focus,
            LayoutMode::Compact => match self.compact_focus {
                CompactFocus::Routes => self.compact_routes.as_pane(),
                CompactFocus::Connections => Pane::Connections,
            },
        }
    }

    fn sync_compact_from_focus(&mut self) {
        match self.focus {
            Pane::Services => {
                self.compact_focus = CompactFocus::Routes;
                self.compact_routes = RoutesView::Services;
            }
            Pane::Forwards => {
                self.compact_focus = CompactFocus::Routes;
                self.compact_routes = RoutesView::Forwards;
            }
            Pane::Connections => {
                self.compact_focus = CompactFocus::Connections;
            }
        }
    }

    fn sync_focus_from_compact(&mut self) {
        self.focus = self.active_pane(LayoutMode::Compact);
    }

    fn open_add_dialog(&mut self, pane: Pane) {
        match pane {
            Pane::Services => {
                self.modal = Some(Modal::AddService(AddServiceDialog {
                    name: String::new(),
                    target: String::new(),
                    persist: false,
                    field: 0,
                }));
            }
            Pane::Forwards => {
                self.modal = Some(Modal::AddForward(AddForwardDialog {
                    listen: String::new(),
                    remote: String::new(),
                    persist: false,
                    field: 0,
                }));
            }
            Pane::Connections => {}
        }
    }

    fn open_remove_dialog(&mut self, pane: Pane) {
        match pane {
            Pane::Services => {
                if let Some(idx) = self.selected_service
                    && let Some(service) = self.services.get(idx)
                {
                    self.modal = Some(Modal::RemoveService {
                        name: service.name.clone(),
                    });
                }
            }
            Pane::Forwards => {
                if let Some(idx) = self.selected_forward
                    && let Some(forward) = self.forwards.get(idx)
                {
                    self.modal = Some(Modal::RemoveForward(RemoveForwardDialog {
                        listen: forward.listen.clone(),
                        remote: forward.remote.clone(),
                        remove_persistently: forward.persisted,
                    }));
                }
            }
            Pane::Connections => {}
        }
    }

    fn move_selection_up(&mut self, pane: Pane) {
        match pane {
            Pane::Services => move_selection_prev(&mut self.selected_service, self.services.len()),
            Pane::Forwards => move_selection_prev(&mut self.selected_forward, self.forwards.len()),
            Pane::Connections => {
                move_selection_prev(&mut self.selected_connection, self.connections.len())
            }
        }
    }

    fn move_selection_down(&mut self, pane: Pane) {
        match pane {
            Pane::Services => move_selection_next(&mut self.selected_service, self.services.len()),
            Pane::Forwards => move_selection_next(&mut self.selected_forward, self.forwards.len()),
            Pane::Connections => {
                move_selection_next(&mut self.selected_connection, self.connections.len())
            }
        }
    }
}

fn clamp_selection(sel: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(sel.unwrap_or(0).min(len - 1))
    }
}

fn move_selection_prev(selection: &mut Option<usize>, len: usize) {
    if len == 0 {
        *selection = None;
        return;
    }

    *selection = Some(match selection {
        Some(current) if *current > 0 => *current - 1,
        _ => 0,
    });
}

fn move_selection_next(selection: &mut Option<usize>, len: usize) {
    if len == 0 {
        *selection = None;
        return;
    }

    *selection = Some(match selection {
        Some(current) if *current + 1 < len => *current + 1,
        Some(current) => *current,
        None => 0,
    });
}

const COMPACT_WIDTH_THRESHOLD: u16 = 160;
const COMPACT_HEIGHT_THRESHOLD: u16 = 36;

fn layout_mode_for_area(area: Rect) -> LayoutMode {
    layout_mode_for_dimensions(area.width, area.height)
}

fn layout_mode_for_dimensions(width: u16, height: u16) -> LayoutMode {
    if width < COMPACT_WIDTH_THRESHOLD || height < COMPACT_HEIGHT_THRESHOLD {
        LayoutMode::Compact
    } else {
        LayoutMode::Full
    }
}

fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let keep = max_chars - 3;
    let mut out = chars.into_iter().take(keep).collect::<String>();
    out.push_str("...");
    out
}

fn start_input_thread() -> mpsc::UnboundedReceiver<KeyEvent> {
    let (tx, rx) = mpsc::unbounded_channel::<KeyEvent>();
    std::thread::spawn(move || {
        while let Ok(polled) = event::poll(Duration::from_millis(200)) {
            if !polled {
                continue;
            }

            let evt = match event::read() {
                Ok(evt) => evt,
                Err(_) => break,
            };
            if let CEvent::Key(key) = evt
                && key.kind == KeyEventKind::Press
                && tx.send(key).is_err()
            {
                break;
            }
        }
    });
    rx
}

async fn fetch_snapshot() -> Result<Snapshot> {
    let status = control::status().await?;
    if status.is_none() {
        return Ok(Snapshot::default());
    }

    let services = control::list_serves().await?;
    let forwards = control::list_forwards().await?;
    let connections = control::list_connections().await?;
    Ok(Snapshot {
        status,
        services,
        forwards,
        connections,
    })
}

pub async fn run_tui(config_path: &Path) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let result = run_tui_loop(&mut terminal, config_path).await;

    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to restore cursor")?;

    result
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config_path: &Path,
) -> Result<()> {
    let mut app = App::default();
    app.apply_snapshot(fetch_snapshot().await?);

    let mut input_rx = start_input_thread();
    let mut state_rx = control::watch_state_changes();
    let mut fallback_sync = time::interval(Duration::from_secs(5));

    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        tokio::select! {
            _ = fallback_sync.tick() => {
                if let Err(err) = refresh(&mut app).await {
                    app.set_error(err);
                }
            }
            maybe_reason = state_rx.recv() => {
                if maybe_reason.is_none() {
                    state_rx = control::watch_state_changes();
                    continue;
                }
                if let Err(err) = refresh(&mut app).await {
                    app.set_error(err);
                }
            }
            maybe_key = input_rx.recv() => {
                let Some(key) = maybe_key else {
                    return Ok(());
                };
                let area = terminal.size().context("failed to query terminal size")?;
                let layout_mode = layout_mode_for_dimensions(area.width, area.height);
                if handle_key(&mut app, key, config_path, layout_mode).await? {
                    return Ok(());
                }
            }
        }
    }
}

async fn refresh(app: &mut App) -> Result<()> {
    app.apply_snapshot(fetch_snapshot().await?);
    Ok(())
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    config_path: &Path,
    layout_mode: LayoutMode,
) -> Result<bool> {
    if let Some(mut modal) = app.modal.take() {
        match handle_modal_key(&mut modal, key, config_path).await {
            Ok(ModalOutcome::Stay) => {
                app.modal = Some(modal);
            }
            Ok(ModalOutcome::Close) => {
                refresh(app).await?;
            }
            Ok(ModalOutcome::ShowInfo(msg)) => {
                refresh(app).await?;
                app.set_info(msg);
            }
            Ok(ModalOutcome::ShowError(err)) => {
                app.set_error(err);
            }
            Err(err) => {
                app.set_error(err);
            }
        }
        return Ok(false);
    }

    match handle_non_modal_key(app, key, layout_mode) {
        KeyAction::Quit => return Ok(true),
        KeyAction::Refresh => refresh(app).await?,
        KeyAction::Continue => {}
    }

    Ok(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Continue,
    Quit,
    Refresh,
}

fn handle_non_modal_key(app: &mut App, key: KeyEvent, layout_mode: LayoutMode) -> KeyAction {
    match layout_mode {
        LayoutMode::Full => match key.code {
            KeyCode::Char('q') => KeyAction::Quit,
            KeyCode::Tab => {
                app.focus = app.focus.next();
                app.sync_compact_from_focus();
                KeyAction::Continue
            }
            KeyCode::BackTab => {
                app.focus = app.focus.prev();
                app.sync_compact_from_focus();
                KeyAction::Continue
            }
            KeyCode::Up => {
                app.move_selection_up(app.focus);
                KeyAction::Continue
            }
            KeyCode::Down => {
                app.move_selection_down(app.focus);
                KeyAction::Continue
            }
            KeyCode::Char('a') => {
                app.open_add_dialog(app.focus);
                KeyAction::Continue
            }
            KeyCode::Char('d') => {
                app.open_remove_dialog(app.focus);
                KeyAction::Continue
            }
            KeyCode::Char('r') => KeyAction::Refresh,
            _ => KeyAction::Continue,
        },
        LayoutMode::Compact => match key.code {
            KeyCode::Char('q') => KeyAction::Quit,
            KeyCode::Tab | KeyCode::BackTab => {
                app.compact_focus = app.compact_focus.toggle();
                app.sync_focus_from_compact();
                KeyAction::Continue
            }
            KeyCode::Left | KeyCode::Right if app.compact_focus == CompactFocus::Routes => {
                app.compact_routes = app.compact_routes.toggle();
                app.sync_focus_from_compact();
                KeyAction::Continue
            }
            KeyCode::Up => {
                let pane = app.active_pane(LayoutMode::Compact);
                app.move_selection_up(pane);
                KeyAction::Continue
            }
            KeyCode::Down => {
                let pane = app.active_pane(LayoutMode::Compact);
                app.move_selection_down(pane);
                KeyAction::Continue
            }
            KeyCode::Char('a') => {
                let pane = app.active_pane(LayoutMode::Compact);
                app.open_add_dialog(pane);
                KeyAction::Continue
            }
            KeyCode::Char('d') => {
                let pane = app.active_pane(LayoutMode::Compact);
                app.open_remove_dialog(pane);
                KeyAction::Continue
            }
            KeyCode::Char('r') => KeyAction::Refresh,
            _ => KeyAction::Continue,
        },
    }
}

enum ModalOutcome {
    Stay,
    Close,
    ShowInfo(String),
    ShowError(anyhow::Error),
}

async fn handle_modal_key(
    modal: &mut Modal,
    key: KeyEvent,
    config_path: &Path,
) -> Result<ModalOutcome> {
    match modal {
        Modal::Message { .. } => match key.code {
            KeyCode::Enter | KeyCode::Esc => Ok(ModalOutcome::Close),
            _ => Ok(ModalOutcome::Stay),
        },
        Modal::AddService(dialog) => handle_add_service_key(dialog, key, config_path).await,
        Modal::AddForward(dialog) => handle_add_forward_key(dialog, key, config_path).await,
        Modal::RemoveService { name } => match key.code {
            KeyCode::Esc => Ok(ModalOutcome::Close),
            KeyCode::Enter => {
                control::del_serve(name)
                    .await
                    .with_context(|| format!("failed to remove service '{}'", name))?;
                Ok(ModalOutcome::ShowInfo(format!("Removed service: {name}")))
            }
            _ => Ok(ModalOutcome::Stay),
        },
        Modal::RemoveForward(dialog) => handle_remove_forward_key(dialog, key, config_path).await,
    }
}

async fn handle_add_service_key(
    dialog: &mut AddServiceDialog,
    key: KeyEvent,
    config_path: &Path,
) -> Result<ModalOutcome> {
    match key.code {
        KeyCode::Esc => Ok(ModalOutcome::Close),
        KeyCode::Tab => {
            dialog.field = (dialog.field + 1) % 3;
            Ok(ModalOutcome::Stay)
        }
        KeyCode::BackTab => {
            dialog.field = (dialog.field + 2) % 3;
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Backspace => {
            if dialog.field == 0 {
                dialog.name.pop();
            } else if dialog.field == 1 {
                dialog.target.pop();
            }
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Char(' ') if dialog.field == 2 => {
            dialog.persist = !dialog.persist;
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Char(c) if dialog.field < 2 && !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if dialog.field == 0 {
                dialog.name.push(c);
            } else {
                dialog.target.push(c);
            }
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Enter => {
            let name = dialog.name.trim();
            let target = dialog.target.trim();
            if name.is_empty() || target.is_empty() {
                return Ok(ModalOutcome::ShowError(anyhow!(
                    "service name and target are required"
                )));
            }

            control::add_serve(name, target)
                .await
                .with_context(|| format!("failed to add service '{} -> {}'", name, target))?;
            if dialog.persist {
                add_persistent_serve_rule(config_path, name, target).with_context(|| {
                    format!(
                        "failed to persist service '{}' in {}",
                        name,
                        config_path.display()
                    )
                })?;
            }
            let mut msg = format!("Added service: {name} -> {target}");
            if dialog.persist {
                msg.push_str(" (persisted)");
            }
            Ok(ModalOutcome::ShowInfo(msg))
        }
        _ => Ok(ModalOutcome::Stay),
    }
}

async fn handle_add_forward_key(
    dialog: &mut AddForwardDialog,
    key: KeyEvent,
    config_path: &Path,
) -> Result<ModalOutcome> {
    match key.code {
        KeyCode::Esc => Ok(ModalOutcome::Close),
        KeyCode::Tab => {
            dialog.field = (dialog.field + 1) % 3;
            Ok(ModalOutcome::Stay)
        }
        KeyCode::BackTab => {
            dialog.field = (dialog.field + 2) % 3;
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Backspace => {
            if dialog.field == 0 {
                dialog.listen.pop();
            } else if dialog.field == 1 {
                dialog.remote.pop();
            }
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Char(' ') if dialog.field == 2 => {
            dialog.persist = !dialog.persist;
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Char(c) if dialog.field < 2 && !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if dialog.field == 0 {
                dialog.listen.push(c);
            } else {
                dialog.remote.push(c);
            }
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Enter => {
            let listen = dialog.listen.trim();
            let remote = dialog.remote.trim();
            if listen.is_empty() || remote.is_empty() {
                return Ok(ModalOutcome::ShowError(anyhow!(
                    "listen and remote are required"
                )));
            }

            remote
                .parse::<RemotePath>()
                .with_context(|| format!("invalid remote path '{}'", remote))?;

            control::add_forward(listen, remote, dialog.persist)
                .await
                .with_context(|| format!("failed to add forward '{} -> {}'", listen, remote))?;
            if dialog.persist {
                add_persistent_forward_rule(config_path, listen, remote).with_context(|| {
                    format!(
                        "failed to persist forward '{}' in {}",
                        listen,
                        config_path.display()
                    )
                })?;
            }
            let mut msg = format!("Added forward: {listen} -> {remote}");
            if dialog.persist {
                msg.push_str(" (persisted)");
            }
            Ok(ModalOutcome::ShowInfo(msg))
        }
        _ => Ok(ModalOutcome::Stay),
    }
}

async fn handle_remove_forward_key(
    dialog: &mut RemoveForwardDialog,
    key: KeyEvent,
    config_path: &Path,
) -> Result<ModalOutcome> {
    match key.code {
        KeyCode::Esc => Ok(ModalOutcome::Close),
        KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
            dialog.remove_persistently = !dialog.remove_persistently;
            Ok(ModalOutcome::Stay)
        }
        KeyCode::Enter => {
            control::del_forward(&dialog.listen)
                .await
                .with_context(|| format!("failed to remove forward '{}'", dialog.listen))?;

            if dialog.remove_persistently {
                let _ = remove_persistent_forward_rule(config_path, &dialog.listen, &dialog.remote)
                    .with_context(|| {
                        format!(
                            "failed to update persisted forwards in {}",
                            config_path.display()
                        )
                    })?;
                Ok(ModalOutcome::ShowInfo(format!(
                    "Removed forward: {} -> {} (runtime + config)",
                    dialog.listen, dialog.remote
                )))
            } else {
                Ok(ModalOutcome::ShowInfo(format!(
                    "Removed forward: {} -> {} (runtime only)",
                    dialog.listen, dialog.remote
                )))
            }
        }
        _ => Ok(ModalOutcome::Stay),
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let layout_mode = layout_mode_for_area(frame.area());
    match layout_mode {
        LayoutMode::Full => draw_full(frame, app),
        LayoutMode::Compact => draw_compact(frame, app),
    }

    if let Some(modal) = &app.modal {
        draw_modal(frame, modal);
    }
}

fn draw_full(frame: &mut ratatui::Frame<'_>, app: &App) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(12),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let status_lines = if let Some(status) = &app.status {
        vec![
            Line::from(vec![
                Span::styled("running", Style::default().fg(Color::Green)),
                Span::raw(format!(
                    "  served: {}  forwards: {}  connections: {}",
                    status.served, status.forwards, status.connections
                )),
            ]),
            Line::from(format!("endpoint: {}", status.endpoint_id)),
        ]
    } else {
        vec![Line::from(vec![
            Span::styled("disconnected", Style::default().fg(Color::Red)),
            Span::raw("  waiting for iroh-proxy server on DBus"),
        ])]
    };

    let status = Paragraph::new(status_lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(status, vertical[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(33),
            Constraint::Percentage(34),
        ])
        .split(vertical[1]);

    draw_services(
        frame,
        body[0],
        app,
        "Services",
        app.focus == Pane::Services,
        false,
    );
    draw_forwards(
        frame,
        body[1],
        app,
        "Forwards",
        app.focus == Pane::Forwards,
        false,
    );
    draw_connections(
        frame,
        body[2],
        app,
        "Active Connections",
        app.focus == Pane::Connections,
        false,
    );

    let help =
        Paragraph::new("Tab/Shift-Tab focus  Up/Down move  a add  d remove  r refresh  q quit")
            .block(Block::default().borders(Borders::ALL).title("Keys"));
    frame.render_widget(help, vertical[2]);
}

fn draw_compact(frame: &mut ratatui::Frame<'_>, app: &App) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let status_lines = if let Some(status) = &app.status {
        let endpoint_limit = usize::from(vertical[0].width.saturating_sub(4));
        vec![
            Line::from(vec![
                Span::styled("up", Style::default().fg(Color::Green)),
                Span::raw(format!(
                    " sv:{} fw:{} cn:{}",
                    status.served, status.forwards, status.connections
                )),
            ]),
            Line::from(format!(
                "ep: {}",
                truncate_with_ellipsis(&status.endpoint_id, endpoint_limit)
            )),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled("down", Style::default().fg(Color::Red)),
                Span::raw(" waiting for server"),
            ]),
            Line::from(""),
        ]
    };
    let status = Paragraph::new(status_lines).wrap(Wrap { trim: true });
    frame.render_widget(status, vertical[0]);

    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(vertical[1]);

    let routes_title = match app.compact_routes {
        RoutesView::Services => "Routes: Services",
        RoutesView::Forwards => "Routes: Forwards",
    };
    let routes_focused = app.compact_focus == CompactFocus::Routes;
    match app.compact_routes {
        RoutesView::Services => {
            draw_services(frame, body[0], app, routes_title, routes_focused, true)
        }
        RoutesView::Forwards => {
            draw_forwards(frame, body[0], app, routes_title, routes_focused, true)
        }
    }

    draw_connections(
        frame,
        body[1],
        app,
        "Conns",
        app.compact_focus == CompactFocus::Connections,
        true,
    );

    let help_text = "Tab row  Left/Right view  Up/Down select  a add  d del  r sync  q quit";
    let help = Paragraph::new(truncate_with_ellipsis(
        help_text,
        usize::from(vertical[2].width),
    ));
    frame.render_widget(help, vertical[2]);
}

fn draw_services(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    title: &'static str,
    focused: bool,
    compact: bool,
) {
    let mut block = Block::default().borders(Borders::ALL).title(title);
    if focused {
        block = block.border_style(Style::default().fg(Color::Yellow));
    }

    let rows = app
        .services
        .iter()
        .map(|service| {
            Row::new(vec![
                Cell::from(service.name.to_string()),
                Cell::from(service.target.to_string()),
            ])
        })
        .collect::<Vec<_>>();

    let constraints = if compact {
        vec![Constraint::Length(14), Constraint::Min(8)]
    } else {
        vec![Constraint::Length(18), Constraint::Min(10)]
    };

    let table = Table::new(rows, constraints)
        .header(
            Row::new(vec!["name", "target"]).style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(block)
        .row_highlight_style(Style::default().bg(Color::Blue))
        .highlight_symbol(if compact { "> " } else { "-> " });

    let mut state = TableState::default();
    state.select(app.selected_service);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_forwards(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    title: &'static str,
    focused: bool,
    compact: bool,
) {
    let mut block = Block::default().borders(Borders::ALL).title(title);
    if focused {
        block = block.border_style(Style::default().fg(Color::Yellow));
    }

    let rows = app
        .forwards
        .iter()
        .map(|forward| {
            let persisted = if compact {
                if forward.persisted { "y" } else { "n" }
            } else if forward.persisted {
                "yes"
            } else {
                "no"
            };
            Row::new(vec![
                Cell::from(forward.listen.to_string()),
                Cell::from(forward.remote.to_string()),
                Cell::from(persisted),
            ])
        })
        .collect::<Vec<_>>();

    let constraints = if compact {
        vec![
            Constraint::Length(16),
            Constraint::Min(12),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(16),
            Constraint::Min(16),
            Constraint::Length(4),
        ]
    };

    let table = Table::new(rows, constraints)
        .header(
            Row::new(vec!["listen", "remote", "p"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(block)
        .row_highlight_style(Style::default().bg(Color::Blue))
        .highlight_symbol(if compact { "> " } else { "-> " });

    let mut state = TableState::default();
    state.select(app.selected_forward);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_connections(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    title: &'static str,
    focused: bool,
    compact: bool,
) {
    let mut block = Block::default().borders(Borders::ALL).title(title);
    if focused {
        block = block.border_style(Style::default().fg(Color::Yellow));
    }

    let rows = app
        .connections
        .iter()
        .map(|conn| {
            Row::new(vec![
                Cell::from(conn.kind.to_string()),
                Cell::from(conn.src.to_string()),
                Cell::from(conn.dst.to_string()),
            ])
        })
        .collect::<Vec<_>>();

    let constraints = if compact {
        vec![
            Constraint::Length(6),
            Constraint::Length(20),
            Constraint::Min(8),
        ]
    } else {
        vec![
            Constraint::Length(8),
            Constraint::Length(24),
            Constraint::Min(12),
        ]
    };

    let table = Table::new(rows, constraints)
        .header(
            Row::new(vec!["type", "src", "dst"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(block)
        .row_highlight_style(Style::default().bg(Color::Blue))
        .highlight_symbol(if compact { "> " } else { "-> " });

    let mut state = TableState::default();
    state.select(app.selected_connection);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_modal(frame: &mut ratatui::Frame<'_>, modal: &Modal) {
    let popup = centered_rect(70, 55, frame.area());
    frame.render_widget(Clear, popup);

    match modal {
        Modal::AddService(dialog) => {
            let body = vec![
                Line::from(vec![
                    Span::styled(
                        if dialog.field == 0 {
                            "> name: "
                        } else {
                            "  name: "
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(&dialog.name),
                ]),
                Line::from(vec![
                    Span::styled(
                        if dialog.field == 1 {
                            "> target: "
                        } else {
                            "  target: "
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(&dialog.target),
                ]),
                Line::from(vec![
                    Span::styled(
                        if dialog.field == 2 {
                            "> persist: "
                        } else {
                            "  persist: "
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(if dialog.persist { "yes" } else { "no" }),
                ]),
                Line::raw(""),
                Line::raw("Tab switch field, Space toggle, Enter submit, Esc cancel"),
            ];
            let widget = Paragraph::new(body)
                .block(Block::default().borders(Borders::ALL).title("Add Service"));
            frame.render_widget(widget, popup);
        }
        Modal::AddForward(dialog) => {
            let body = vec![
                Line::from(vec![
                    Span::styled(
                        if dialog.field == 0 {
                            "> listen: "
                        } else {
                            "  listen: "
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(&dialog.listen),
                ]),
                Line::from(vec![
                    Span::styled(
                        if dialog.field == 1 {
                            "> remote: "
                        } else {
                            "  remote: "
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(&dialog.remote),
                ]),
                Line::from(vec![
                    Span::styled(
                        if dialog.field == 2 {
                            "> persist: "
                        } else {
                            "  persist: "
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(if dialog.persist { "yes" } else { "no" }),
                ]),
                Line::raw(""),
                Line::raw("Tab switch field, Space toggle, Enter submit, Esc cancel"),
            ];
            let widget = Paragraph::new(body)
                .block(Block::default().borders(Borders::ALL).title("Add Forward"));
            frame.render_widget(widget, popup);
        }
        Modal::RemoveService { name } => {
            let body = vec![
                Line::raw(format!("Remove service '{name}'?")),
                Line::raw(""),
                Line::raw("Enter confirm, Esc cancel"),
            ];
            let widget = Paragraph::new(body).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Remove Service"),
            );
            frame.render_widget(widget, popup);
        }
        Modal::RemoveForward(dialog) => {
            let mode = if dialog.remove_persistently {
                "runtime + config"
            } else {
                "runtime only"
            };
            let body = vec![
                Line::raw(format!(
                    "Remove forward '{} -> {}'?",
                    dialog.listen, dialog.remote
                )),
                Line::raw(""),
                Line::raw(format!("mode: {mode}")),
                Line::raw("Tab/Left/Right/Space toggle mode"),
                Line::raw("Enter confirm, Esc cancel"),
            ];
            let widget = Paragraph::new(body).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Remove Forward"),
            );
            frame.render_widget(widget, popup);
        }
        Modal::Message { title, body } => {
            let widget = Paragraph::new(vec![
                Line::raw(body.as_ref()),
                Line::raw(""),
                Line::raw("Enter/Esc close"),
            ])
            .block(Block::default().borders(Borders::ALL).title(title.as_ref()));
            frame.render_widget(widget, popup);
        }
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn layout_mode_thresholds_match_expected_policy() {
        assert_eq!(
            layout_mode_for_area(Rect::new(0, 0, 159, 40)),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode_for_area(Rect::new(0, 0, 160, 35)),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode_for_area(Rect::new(0, 0, 160, 36)),
            LayoutMode::Full
        );
    }

    #[test]
    fn compact_navigation_switches_focus_and_routes_view() {
        let mut app = App::default();
        assert_eq!(app.active_pane(LayoutMode::Compact), Pane::Services);

        handle_non_modal_key(&mut app, key(KeyCode::Tab), LayoutMode::Compact);
        assert_eq!(app.compact_focus, CompactFocus::Connections);
        assert_eq!(app.active_pane(LayoutMode::Compact), Pane::Connections);

        handle_non_modal_key(&mut app, key(KeyCode::BackTab), LayoutMode::Compact);
        assert_eq!(app.compact_focus, CompactFocus::Routes);

        handle_non_modal_key(&mut app, key(KeyCode::Right), LayoutMode::Compact);
        assert_eq!(app.compact_routes, RoutesView::Forwards);
        assert_eq!(app.active_pane(LayoutMode::Compact), Pane::Forwards);
    }

    #[test]
    fn compact_selection_moves_in_active_table_only() {
        let mut app = App {
            services: vec![
                ServeRoute {
                    name: "svc-a".into(),
                    target: "127.0.0.1:1".into(),
                },
                ServeRoute {
                    name: "svc-b".into(),
                    target: "127.0.0.1:2".into(),
                },
            ],
            forwards: vec![
                ForwardRoute {
                    listen: "127.0.0.1:7001".into(),
                    remote: "node/tcp/a".into(),
                    persisted: false,
                },
                ForwardRoute {
                    listen: "127.0.0.1:7002".into(),
                    remote: "node/tcp/b".into(),
                    persisted: false,
                },
            ],
            connections: vec![
                ActiveConnection {
                    id: 1,
                    src: "src-a".into(),
                    kind: "tcp".into(),
                    dst: "dst-a".into(),
                },
                ActiveConnection {
                    id: 2,
                    src: "src-b".into(),
                    kind: "tcp".into(),
                    dst: "dst-b".into(),
                },
            ],
            selected_service: Some(0),
            selected_forward: Some(0),
            selected_connection: Some(0),
            ..App::default()
        };

        handle_non_modal_key(&mut app, key(KeyCode::Down), LayoutMode::Compact);
        assert_eq!(app.selected_service, Some(1));
        assert_eq!(app.selected_forward, Some(0));
        assert_eq!(app.selected_connection, Some(0));

        handle_non_modal_key(&mut app, key(KeyCode::Right), LayoutMode::Compact);
        handle_non_modal_key(&mut app, key(KeyCode::Down), LayoutMode::Compact);
        assert_eq!(app.selected_service, Some(1));
        assert_eq!(app.selected_forward, Some(1));
        assert_eq!(app.selected_connection, Some(0));

        handle_non_modal_key(&mut app, key(KeyCode::Tab), LayoutMode::Compact);
        handle_non_modal_key(&mut app, key(KeyCode::Down), LayoutMode::Compact);
        assert_eq!(app.selected_connection, Some(1));
    }
}
