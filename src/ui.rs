//! ratatui dashboard.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use crate::config::Config;
use crate::providers::copilot;
use crate::model::{fmt_money, fmt_tok, Snapshot, Status};
use crate::snapshot;

#[derive(Clone, Copy, PartialEq)]
enum LoginKind {
    Copilot,
    Grok,
    OpenRouter,
    OpenCodeGo,
}

struct LoginState {
    kind: LoginKind,
    lines: Arc<Mutex<Vec<String>>>,
    done: Arc<AtomicBool>,
    // set true only when the OAuth flow actually succeeded
    connected: Arc<AtomicBool>,
    // UI-side timer: when we first observed `connected`, so we can show the
    // confirmation for a beat before auto-closing.
    connected_at: Option<Instant>,
    /// manual API-key input (OpenRouter): type/paste, Enter to save
    input: String,
}

#[derive(Clone)]
struct ProviderItem {
    name: &'static str,
    status: String,
    kind: LoginKind,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum LogoutKind {
    Copilot,
    OpenRouter,
    OpenCodeGo,
    All,
}

impl LogoutKind {
    fn label(self) -> &'static str {
        match self {
            LogoutKind::Copilot => "Logout Copilot   clear OAuth token",
            LogoutKind::OpenRouter => "Logout OpenRouter   clear API key",
            LogoutKind::OpenCodeGo => "Logout OpenCode Go   clear key",
            LogoutKind::All => "Logout all   clear every saved token",
        }
    }
}

#[derive(Clone)]
enum MenuEntry {
    Provider(ProviderItem),
    /// clear saved tokens for one provider (or everything)
    Logout(LogoutKind),
}

struct ConnectMenu {
    items: Vec<MenuEntry>,
    selected: usize,
}

enum Modal {
    Menu(ConnectMenu),
    Login(LoginState),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Close,
    Refresh,
    Connect,
}

/// Refresh cadence shown in the footer: auto countdown + last refresh time,
/// plus a short-lived `✓ refreshed` flash after a manual refresh.
struct RefreshInfo {
    last: String,          // HH:MM:SS of the last snapshot
    next_in_secs: u64,     // seconds until the automatic refresh
    flash: Option<(Instant, String)>,
}

fn fb_clock() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

pub fn run(cfg: Config) -> std::io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let refresh = Duration::from_secs(cfg.refresh_seconds.max(5));
    let mut snap = snapshot::snapshot(&cfg);
    let mut snap_at = Instant::now();
    let mut next_refresh = snap_at + refresh;
    // refresh the per-model cache once at startup if it's missing or stale
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if snap.or_usage.as_ref().map(|u| crate::openrouter::is_stale(u, now_unix)).unwrap_or(true) {
        crate::openrouter::sync_async(&cfg);
    }
    let mut modal: Option<Modal> = None;
    let mut footer_rect = Rect::default();
    let mut modal_rect: Option<Rect> = None;
    // mouse capture starts on (we enable it at startup); login modals turn it
    // off so the user can mouse-select the verification URL/code to copy.
    let mut mouse_capture = true;
    // refresh feedback
    let mut last_refresh = fb_clock();
    let mut flash: Option<(Instant, String)> = None;

    let res = loop {
        // redraw on a timer so the countdown/refresh stays live
        if Instant::now() >= next_refresh {
            snap = snapshot::snapshot(&cfg);
            snap_at = Instant::now();
            next_refresh = snap_at + refresh;
            last_refresh = fb_clock();
        }
        term.draw(|f| {
            let rect = f.area();
            let info = RefreshInfo {
                last: last_refresh.clone(),
                next_in_secs: next_refresh.saturating_duration_since(Instant::now()).as_secs(),
                flash: flash.clone(),
            };
            footer_rect = draw_dashboard(f, rect, &cfg, &snap, &info);
            modal_rect = match &modal {
                Some(m) => draw_modal(f, rect, m),
                None => None,
            };
        })?;

        // While a login modal is open, release mouse capture so the terminal
        // handles selection and the user can copy the URL/code; re-enable it
        // as soon as we're back to the dashboard or the connect menu.
        let want_capture = !matches!(modal, Some(Modal::Login(_)));
        if want_capture != mouse_capture {
            if want_capture {
                execute!(term.backend_mut(), EnableMouseCapture)?;
            } else {
                execute!(term.backend_mut(), DisableMouseCapture)?;
            }
            mouse_capture = want_capture;
        }

        if event::poll(Duration::from_millis(250))? {
            let ev = event::read()?;
            // a manual refresh (R key, footer click, logout) reshoots the timer,
            // which we turn into a short visual flash in the footer.
            let prev_next = next_refresh;
            if let Event::Mouse(m) = &ev {
                if std::env::var("CODEBARX_MOUSE_DEBUG").is_ok() {
                    let path = crate::config::state_dir().join("mouse-events.log");
                    use std::io::Write as _;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true).append(true).open(&path)
                    {
                        let _ = writeln!(f, "kind={:?} col={} row={} mods={:?}",
                            m.kind, m.column, m.row, m.modifiers);
                    }
                }
            }
            match ev {
                Event::Key(k) => {
                    let quit = handle_key(k, &mut modal, &mut next_refresh, &cfg, &snap)?;
                    if quit {
                        break Ok(());
                    }
                }
                Event::Mouse(m) => {
                    let menu_len = match &modal {
                        Some(Modal::Menu(menu)) => Some(menu.items.len()),
                        _ => None,
                    };
                    match decide_mouse(&m, menu_len, modal_rect, &footer_rect) {
                        MouseDecision::None => {}
                        MouseDecision::Close => break Ok(()),
                        MouseDecision::Refresh => next_refresh = Instant::now(),
                        MouseDecision::OpenMenu => modal = Some(open_menu(&snap)),
                        MouseDecision::Dismiss => modal = None,
                        MouseDecision::ConnectProvider(idx) => {
                            match menu_entry_at(&modal, idx) {
                                Some(MenuEntry::Provider(p)) => modal = Some(start_login(p.kind, &cfg)),
                                Some(MenuEntry::Logout(k)) => do_logout(&cfg, &mut modal, &mut next_refresh, k),
                                None => {}
                            }
                        }
                    }
                }
                Event::Resize(_, _) => {}
                Event::Paste(s) => {
                    // bracketed paste (e.g. an API key) lands as one event
                    if let Some(Modal::Login(ls)) = &mut modal {
                        if matches!(ls.kind, LoginKind::OpenRouter | LoginKind::OpenCodeGo)
                            && !ls.done.load(Ordering::Relaxed)
                        {
                            ls.input.push_str(&s);
                        }
                    }
                }
                _ => {}
            }
            if next_refresh != prev_next {
                flash = Some((Instant::now() + Duration::from_secs(3), "✓ refreshed".to_string()));
            }
        }
        // fold a finished device login back into a fresh snapshot
        let mut close_login = false;
        if let Some(Modal::Login(ls)) = &mut modal {
            close_login = login_fold(ls, Instant::now());
        }
        if close_login {
            modal = None;
            snap = snapshot::snapshot(&cfg);
            snap_at = Instant::now();
            next_refresh = snap_at + refresh;
        }
    };

    execute!(
        term.backend_mut(),
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        crossterm::cursor::Show,
        DisableBracketedPaste
    )?;
    terminal::disable_raw_mode()?;
    res
}

fn handle_key(
    k: KeyEvent,
    modal: &mut Option<Modal>,
    next_refresh: &mut Instant,
    cfg: &Config,
    snap: &Snapshot,
) -> std::io::Result<bool> {
    let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
    if ctrl_c {
        return Ok(true);
    }
    if let Some(m) = modal {
        match m {
            Modal::Menu(menu) => {
                let n = menu.items.len();
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                        menu.selected = (menu.selected + n - 1) % n;
                    }
                    KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                        menu.selected = (menu.selected + 1) % n;
                    }
                    KeyCode::Enter => {
                        match menu.items.get(menu.selected).cloned() {
                            Some(MenuEntry::Provider(p)) => {
                                *modal = Some(start_login(p.kind, cfg));
                            }
                            Some(MenuEntry::Logout(k)) => {
                                do_logout(cfg, modal, next_refresh, k);
                            }
                            None => {}
                        }
                    }
                    KeyCode::Char('q') | KeyCode::Esc => *modal = None,
                    _ => {}
                }
            }
            Modal::Login(ls) => {
                // API-key entry (OpenRouter / OpenCode Go): type/paste the key,
                // Enter saves it and drops back to the refreshed dashboard.
                // Keys can contain any character, so `q` only cancels while
                // the field is empty.
                let inputting =
                    matches!(ls.kind, LoginKind::OpenRouter | LoginKind::OpenCodeGo)
                        && !ls.done.load(Ordering::Relaxed);
                if inputting {
                    match k.code {
                        KeyCode::Esc => *modal = None,
                        KeyCode::Char('q') if ls.input.is_empty() => *modal = None,
                        KeyCode::Backspace => {
                            ls.input.pop();
                        }
                        KeyCode::Char(c) => {
                            // 'o' only opens the keys page while the field is
                            // still empty; the 'o' in "sk-…" must always be
                            // typed into the key, never swallowed as a command.
                            if c == 'o' && ls.input.is_empty() {
                                open_key_page(ls.kind);
                            } else {
                                ls.input.push(c);
                            }
                        }
                        KeyCode::Enter => submit_api_key(ls, cfg),
                        _ => {}
                    }
                } else {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => *modal = None,
                        KeyCode::Enter if ls.kind != LoginKind::Copilot => *modal = None,
                        _ => {}
                    }
                }
            }
        }
        return Ok(false); // swallow keys while a modal is open
    }
    match k.code {
        KeyCode::Char('q') | KeyCode::Char('x') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('c') | KeyCode::Char('C') => *modal = Some(open_menu(snap)),
        KeyCode::Char('l') | KeyCode::Char('L') => *modal = Some(start_login(LoginKind::Copilot, cfg)),
        KeyCode::Char('g') | KeyCode::Char('G') => *modal = Some(start_login(LoginKind::Grok, cfg)),
        KeyCode::Char('r') | KeyCode::Char('R') => {
            *next_refresh = Instant::now();
            crate::openrouter::sync_async(cfg); // also refresh the model-usage cache
        }
        _ => {}
    }
    Ok(false)
}

/// Build the connect menu with each provider's current status.
fn open_menu(snap: &Snapshot) -> Modal {
    let copilot_status = match &snap.copilot {
        Status::Ok(cp) => match cp.quotas.iter().find(|q| q.used_pct.is_some()) {
            Some(q0) => format!("{} · {} {}% used", cp.plan, q0.name, q0.used_pct.unwrap()),
            None => format!("{} · connected", cp.plan),
        },
        Status::NeedsLogin { .. } => "needs login (device flow)".into(),
        Status::Err { msg } => format!("error: {msg}"),
    };
    let grok_status = if snap.grok.needs_login {
        "needs login".into()
    } else if let Some(e) = &snap.grok.error {
        format!("error ({e})")
    } else if let Some(p) = snap.grok.used_pct {
        format!("credits {p:.0}% used")
    } else {
        "ready".into()
    };
    let or_status = if snap.openrouter.needs_key {
        "needs API key".into()
    } else if let Some(e) = &snap.openrouter.error {
        format!("error ({e})")
    } else {
        format!("credits {}", crate::model::fmt_money(snap.openrouter.balance_usd))
    };
    let ogo_status = if snap.opencode_go.needs_key {
        "needs key (opencode auth.json)".into()
    } else if let Some(e) = &snap.opencode_go.error {
        format!("error ({e})")
    } else if let Some(w) = &snap.opencode_go.rolling {
        format!("rolling {}% used", w.percent)
    } else {
        "connected".into()
    };
    Modal::Menu(ConnectMenu {
        selected: 0,
        items: vec![
            MenuEntry::Provider(ProviderItem { name: "GitHub Copilot", status: copilot_status, kind: LoginKind::Copilot }),
            MenuEntry::Provider(ProviderItem { name: "Grok (xAI)", status: grok_status, kind: LoginKind::Grok }),
            MenuEntry::Provider(ProviderItem { name: "OpenRouter", status: or_status, kind: LoginKind::OpenRouter }),
            MenuEntry::Provider(ProviderItem { name: "OpenCode Go", status: ogo_status, kind: LoginKind::OpenCodeGo }),
            MenuEntry::Logout(LogoutKind::Copilot),
            MenuEntry::Logout(LogoutKind::OpenRouter),
            MenuEntry::Logout(LogoutKind::OpenCodeGo),
            MenuEntry::Logout(LogoutKind::All),
        ],
    })
}

fn start_login(kind: LoginKind, cfg: &Config) -> Modal {
    match kind {
        LoginKind::Copilot => Modal::Login(start_copilot_login(cfg)),
        LoginKind::Grok => Modal::Login(LoginState {
            kind: LoginKind::Grok,
            lines: Arc::new(Mutex::new(vec![
                "1) Install the Grok Build CLI and run:   grok login".into(),
                "2) Or set GROK_OAUTH_TOKEN=<bearer> for the panel process.".into(),
                "".into(),
                "press q or Esc to return to the dashboard".into(),
            ])),
            done: Arc::new(AtomicBool::new(false)),
            connected: Arc::new(AtomicBool::new(false)),
            connected_at: None,
            input: String::new(),
        }),
        LoginKind::OpenRouter => Modal::Login(LoginState {
            kind: LoginKind::OpenRouter,
            lines: Arc::new(Mutex::new(vec![
                "1) press [o] to open your keys page".into(),
                "   https://openrouter.ai/settings/keys".into(),
                "".into(),
                "2) (optional) set a key spending limit at".into(),
                "   https://openrouter.ai/settings/limits  to show the meter".into(),
                "".into(),
                "3) paste the key below, then press Enter:".into(),
            ])),
            done: Arc::new(AtomicBool::new(false)),
            connected: Arc::new(AtomicBool::new(false)),
            connected_at: None,
            input: String::new(),
        }),
        LoginKind::OpenCodeGo => Modal::Login(LoginState {
            kind: LoginKind::OpenCodeGo,
            lines: Arc::new(Mutex::new(vec![
                "1) find the key in the opencode console".into(),
                "   https://opencode.ai/console  →  Keys (or press [o])".into(),
                "".into(),
                "   when using opencode-go from pi, it is already stored at".into(),
                "   ~/.pi/agent/auth.json — nothing to do, auto-detected.".into(),
                "".into(),
                "2) otherwise paste the key below (sk-...), then press Enter:".into(),
            ])),
            done: Arc::new(AtomicBool::new(false)),
            connected: Arc::new(AtomicBool::new(false)),
            connected_at: None,
            input: String::new(),
        }),
    }
}

fn start_copilot_login(cfg: &Config) -> LoginState {
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let done = Arc::new(AtomicBool::new(false));
    let connected = Arc::new(AtomicBool::new(false));
    let l2 = lines.clone();
    let d2 = done.clone();
    let c2 = connected.clone();
    let cfg2 = cfg.clone();
    std::thread::spawn(move || {
        let ok = copilot::device_login(&cfg2, |line| {
            if let Ok(mut l) = l2.lock() {
                l.push(line.to_string());
            }
        });
        if ok.is_some() {
            c2.store(true, Ordering::Relaxed);
        }
        d2.store(true, Ordering::Relaxed);
    });
    LoginState { kind: LoginKind::Copilot, lines, done, connected, connected_at: None, input: String::new() }
}

/// Decide whether a finished login modal should close & trigger a refresh.
/// On success (`connected`) it keeps the confirmation visible for ~1.5 s
/// before closing, so the user actually sees "✓ Connected". On failure it
/// returns false forever, leaving the modal open for the user to read and
/// close manually with q/Esc.
fn login_fold(ls: &mut LoginState, now: Instant) -> bool {
    if !ls.done.load(Ordering::Relaxed) {
        return false;
    }
    if !ls.connected.load(Ordering::Relaxed) {
        return false; // failed / timed-out — stay open until the user closes
    }
    let t0 = match ls.connected_at {
        Some(t) => t,
        None => {
            ls.connected_at = Some(now);
            now
        }
    };
    now.duration_since(t0) >= Duration::from_millis(1500)
}

/// Look up a connect-menu entry by index (for keyboard Enter + mouse clicks).
/// Returns an owned clone so callers can mutate `modal` without a borrow fight.
fn menu_entry_at(modal: &Option<Modal>, idx: usize) -> Option<MenuEntry> {
    match modal {
        Some(Modal::Menu(menu)) => menu.items.get(idx).cloned(),
        _ => None,
    }
}

fn submit_api_key(ls: &mut LoginState, cfg: &Config) {
    let key = ls.input.trim().to_string();
    if key.is_empty() {
        return;
    }
    match ls.kind {
        LoginKind::OpenRouter => crate::openrouter::save_key(cfg, &key),
        LoginKind::OpenCodeGo => crate::providers::opencode_go::save_key(cfg, &key),
        LoginKind::Copilot | LoginKind::Grok => {}
    }
    ls.input.clear();
    // mark as connected so the modal shows the confirmation for a beat and
    // the run loop folds it → immediate refresh validates the key.
    ls.done.store(true, Ordering::Relaxed);
    ls.connected.store(true, Ordering::Relaxed);
}

/// Clear every saved provider token (Copilot OAuth + OpenRouter/OpenCode-go
/// keys), close the menu and refresh so the panel reflects the logged-out state.
fn do_logout(cfg: &Config, modal: &mut Option<Modal>, next_refresh: &mut Instant, kind: LogoutKind) {
    match kind {
        LogoutKind::Copilot => crate::providers::copilot::clear_token(cfg),
        LogoutKind::OpenRouter => crate::openrouter::clear_key(cfg),
        LogoutKind::OpenCodeGo => crate::providers::opencode_go::clear_key(cfg),
        LogoutKind::All => {
            crate::providers::copilot::clear_token(cfg);
            crate::openrouter::clear_key(cfg);
            crate::providers::opencode_go::clear_key(cfg);
        }
    }
    *modal = None;
    *next_refresh = Instant::now();
}

/// Open the right keys page for a key-input login kind (empty-field `o` key).
fn open_key_page(kind: LoginKind) {
    match kind {
        LoginKind::OpenRouter => open_url("https://openrouter.ai/settings/keys"),
        LoginKind::OpenCodeGo => open_url("https://opencode.ai/console"),
        LoginKind::Copilot | LoginKind::Grok => {}
    }
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/* ------------------------------------------------------------------ */
/* drawing                                                             */
/* ------------------------------------------------------------------ */

fn draw_dashboard(f: &mut Frame, area: Rect, cfg: &Config, snap: &Snapshot, refresh: &RefreshInfo) -> Rect {
    let constraints = [Constraint::Length(2), Constraint::Min(0), Constraint::Length(2)];
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    // header: title only — the right-hand live reset countdown was dropped.
    let header = Paragraph::new(Line::from(vec![
        Span::styled("UsageBar", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]));
    f.render_widget(header, Rect::new(chunks[0].x, chunks[0].y, chunks[0].width, 1));
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(chunks[0].width as usize),
            Style::default().fg(Color::DarkGray),
        ))),
        Rect::new(chunks[0].x, chunks[0].y + 1, chunks[0].width, 1),
    );

    draw_body(f, chunks[1], cfg, snap);

    // footer: buttons row + separator row
    let mut spans = Vec::<Span<'static>>::new();
    for (i, item) in footer_layout().into_iter().enumerate() {
        let label = item.1;
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            label[..3].to_string(),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(label[3..].to_string()));
    }
    // refresh feedback, right-aligned: `⟳ auto 30s · last 14:05:22`, or a
    // short yellow `✓ refreshed` flash right after a manual refresh.
    let buttons_w = footer_layout().iter().map(|r| r.3).max().unwrap_or(0) as usize;
    let now = Instant::now();
    let flashing = refresh.flash.as_ref().map(|(u, _)| now < *u).unwrap_or(false);
    let status = if flashing {
        "✓ refreshed".to_string()
    } else {
        format!("⟳ auto {}s · last {}", refresh.next_in_secs, refresh.last)
    };
    let pad = (chunks[2].width as usize)
        .saturating_sub(buttons_w + status.chars().count() + 1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(
        status,
        Style::default()
            .fg(if flashing { Color::Yellow } else { Color::DarkGray })
            .add_modifier(if flashing { Modifier::BOLD } else { Modifier::empty() }),
    ));
    let mut footer_rows = vec![Line::from(spans)];
    footer_rows.push(Line::from(Span::styled(
        "─".repeat(chunks[2].width as usize),
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(footer_rows), chunks[2]);
    chunks[2]
}

/// (action, label, start_x, end_x_exclusive) measured from the footer's left edge.
fn footer_layout() -> Vec<(Action, &'static str, u16, u16)> {
    let labels: [(&str, Action); 3] = [
        ("[C] Connect", Action::Connect),
        ("[R] Refresh", Action::Refresh),
        ("[q] Close", Action::Close),
    ];
    let mut out = Vec::with_capacity(labels.len());
    let mut x: u16 = 0;
    for (i, (label, act)) in labels.iter().enumerate() {
        let start = x;
        x += label.chars().count() as u16;
        out.push((*act, *label, start, x));
        if i + 1 < labels.len() {
            x += 3; // "   " separator
        }
    }
    out
}

fn mouse_to_action(m: &crossterm::event::MouseEvent, footer: &Rect) -> Option<Action> {
    if !matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    if m.row != footer.y {
        return None;
    }
    let col = m.column;
    for (act, _, x0, x1) in footer_layout() {
        if col >= x0 && col < x1 {
            return Some(act);
        }
    }
    None
}

#[derive(Debug, PartialEq)]
enum MouseDecision {
    /// ignore (Up / Drag / scroll / hover — never drives the UI)
    None,
    Close,
    Refresh,
    OpenMenu,
    /// a real left-click landed on a provider row in the open menu
    ConnectProvider(usize),
    /// a real left-click landed outside the open modal
    Dismiss,
}

/// Decide what a mouse event does with an open modal / footer.
/// Only a left-button *press* (Down) drives the UI; releasing or dragging
/// must never dismiss a modal, otherwise the modal flashes while the button
/// is held and disappears on release.
///
/// `menu_items` = Some(len) when the Connect menu is open;
/// `modal_rect`  = Some(rect) when *any* modal is open.
fn decide_mouse(
    m: &crossterm::event::MouseEvent,
    menu_items: Option<usize>,
    modal_rect: Option<Rect>,
    footer: &Rect,
) -> MouseDecision {
    if !matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        return MouseDecision::None;
    }
    if let Some(n) = menu_items {
        // Connect menu is open: a click on a row connects, anything else dismisses.
        if let Some(r) = modal_rect {
            if let Some(idx) = menu_hit(m, r, n) {
                return MouseDecision::ConnectProvider(idx);
            }
        }
        return MouseDecision::Dismiss;
    }
    if modal_rect.is_some() {
        // A login modal is open — ignore every mouse action so the user can
        // select/copy the verification URL & code without dismissing it.
        // (Mouse capture is also off in this state.) Close it with q/Esc.
        return MouseDecision::None;
    }
    match mouse_to_action(m, footer) {
        Some(Action::Close) => MouseDecision::Close,
        Some(Action::Refresh) => MouseDecision::Refresh,
        Some(Action::Connect) => MouseDecision::OpenMenu,
        None => MouseDecision::None,
    }
}

/// Find which provider row a click landed on in the connect menu.
/// Item rows are laid out at modal.y + 3 + i (title + blank line above).
fn menu_hit(m: &crossterm::event::MouseEvent, mrect: Rect, n: usize) -> Option<usize> {
    if !matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) || n == 0 {
        return None;
    }
    if m.column < mrect.x || m.column >= mrect.x + mrect.width {
        return None;
    }
    let first = mrect.y.saturating_add(3);
    if m.row < first {
        return None;
    }
    let idx = (m.row - first) as usize;
    if idx < n {
        Some(idx)
    } else {
        None
    }
}

fn centered_rect(area: Rect, x_pct: u16, y_pct: u16) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - y_pct) / 2),
            Constraint::Percentage(y_pct),
            Constraint::Percentage((100 - y_pct) / 2),
        ])
        .split(area);
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - x_pct) / 2),
            Constraint::Percentage(x_pct),
            Constraint::Percentage((100 - x_pct) / 2),
        ])
        .split(vert[1]);
    horiz[1]
}

fn draw_modal(f: &mut Frame, area: Rect, modal: &Modal) -> Option<Rect> {
    match modal {
        Modal::Menu(menu) => draw_connect_menu(f, area, menu),
        Modal::Login(ls) => draw_login_modal(f, area, ls),
    }
}

fn draw_connect_menu(f: &mut Frame, area: Rect, menu: &ConnectMenu) -> Option<Rect> {
    let rect = centered_rect(area, 62, 48);
    f.render_widget(Clear, rect);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(" Select a provider:"));
    lines.push(Line::raw(""));
    // item rows MUST stay contiguous (one row each) so mouse `menu_hit`
    // (row = rect.y + 3 + idx) keeps lining up with the selection.
    for (i, item) in menu.items.iter().enumerate() {
        let sel = i == menu.selected;
        let mark = if sel { "▸ " } else { "  " };
        match item {
            MenuEntry::Provider(p) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{mark}{:<16}", p.name),
                        Style::default()
                            .fg(if sel { Color::Yellow } else { Color::White })
                            .add_modifier(if sel { Modifier::BOLD } else { Modifier::empty() }),
                    ),
                    Span::styled(p.status.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
            MenuEntry::Logout(k) => {
                let all = *k == LogoutKind::All;
                let color = if all { Color::Red } else { Color::Yellow };
                lines.push(Line::from(vec![Span::styled(
                    format!("{mark}{}", k.label()),
                    Style::default()
                        .fg(if sel { color } else { Color::DarkGray })
                        .add_modifier(if sel { Modifier::BOLD } else { Modifier::empty() }),
                )]));
            }
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(" ↑/↓ move · Enter select · click a row", Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" Esc / q  close", Style::default().fg(Color::DarkGray)),
    ]));

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Connect — pick a provider ")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .style(Style::default().fg(Color::White)),
        rect,
    );
    Some(rect)
}

fn draw_login_modal(f: &mut Frame, area: Rect, ls: &LoginState) -> Option<Rect> {
    let rect = centered_rect(area, 76, 60);
    let text = {
        let lines = ls.lines.lock().unwrap_or_else(|p| p.into_inner());
        lines.join("\n")
    };
    let mut out: Vec<Line> = text.split('\n').map(Line::raw).collect();
    out.push(Line::raw(""));
    if ls.connected.load(Ordering::Relaxed) {
        let msg = match ls.kind {
            LoginKind::OpenRouter => "✓ API key saved — checking credits…",
            LoginKind::OpenCodeGo => "✓ API key saved — checking quota…",
            LoginKind::Copilot | LoginKind::Grok => {
                "✓ Connected — OAuth token saved, closing…"
            }
        };
        out.push(Line::from(vec![Span::styled(
            msg,
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        )]));
    } else if ls.done.load(Ordering::Relaxed) {
        out.push(Line::from(vec![Span::styled(
            "login failed — press q/Esc to return",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )]));
    } else if matches!(ls.kind, LoginKind::OpenRouter | LoginKind::OpenCodeGo) {
        // key input field rendered below
    } else {
        out.push(Line::from(vec![Span::styled(
            "drag-select with the mouse to copy · q/Esc to close",
            Style::default().fg(Color::DarkGray),
        )]));
    }
    // inline API-key entry (OpenRouter / OpenCode Go): masked input + hints
    if matches!(ls.kind, LoginKind::OpenRouter | LoginKind::OpenCodeGo)
        && !ls.done.load(Ordering::Relaxed)
    {
        let mut masked = String::from("   key  ");
        for (i, c) in ls.input.chars().enumerate() {
            if i < 12 {
                masked.push(c);
            } else {
                masked.push('·');
            }
        }
        out.push(Line::from(vec![
            Span::styled(masked, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled("▏", Style::default().fg(Color::DarkGray)),
        ]));
        out.push(Line::from(vec![Span::styled(
            "  Enter — save & validate   o — open keys page   Esc — cancel",
            Style::default().fg(Color::DarkGray),
        )]));
    }
    let title = match ls.kind {
        LoginKind::Copilot => " GitHub Copilot — device login ",
        LoginKind::Grok => " Grok (xAI) — login ",
        LoginKind::OpenRouter => " OpenRouter — API key ",
        LoginKind::OpenCodeGo => " OpenCode Go — API key ",
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(out)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .style(Style::default().fg(Color::White)),
        rect,
    );
    Some(rect)
}

fn draw_body(f: &mut Frame, area: Rect, cfg: &Config, snap: &Snapshot) {
    let width = area.width.max(1) as usize;
    let mut y = area.y;
    let max_y = area.y + area.height;

    fn next_slot(y: &mut u16, h: u16, max: u16) -> Option<Rect> {
        if *y + h > max {
            return None;
        }
        let r = Rect::new(0, *y, 0, h); // width fixed at the call site
        *y += h;
        Some(r)
    }
    fn put(f: &mut Frame, area: Rect, line: Line<'static>) {
        f.render_widget(Paragraph::new(line), area);
    }
    fn divider_slot(y: &mut u16, max: u16, drew: bool) -> Option<Rect> {
        // separator between providers: only when something was drawn before,
        // and leave room for at least one provider row after it.
        if drew && *y + 2 <= max {
            next_slot(y, 1, max)
        } else {
            None
        }
    }
    fn draw_divider(f: &mut Frame, r: Rect, width: usize) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(width),
                Style::default().fg(Color::DarkGray),
            ))),
            r,
        );
    }
    let mut drew = false;
    // Show only providers that are actually connected / have data this window.
    // Set `show_no_data_providers: true` in config.json to opt back into the
    // always-show-everything layout.
    let show_all = cfg.show_no_data_providers;
    let c_active = snap.claude.msgs > 0 || snap.claude.has_token_data;
    let x_active = snap.codex.sessions > 0 || snap.codex.turns > 0 || snap.codex.has_token_data;
    let o_active = snap.opencode.as_ref().map(|o| o.sessions > 0 || o.has_token_data).unwrap_or(false);
    let cp_active = matches!(&snap.copilot, Status::Ok(_));
    let g_active = !snap.grok.needs_login || snap.grok.local_sessions > 0;
    let ogo_active = !snap.opencode_go.needs_key;
    let or_active = !snap.openrouter.needs_key;
    let any_active = show_all || c_active || x_active || o_active || cp_active || g_active || ogo_active || or_active;

    if !any_active {
        if let Some(r) = next_slot(&mut y, 1, max_y) {
            let r = Rect::new(area.x, r.y, area.width, r.height);
            f.render_widget(
                Paragraph::new("no active providers — press [C] Connect or run a CLI first")
                    .style(Style::default().fg(Color::DarkGray)),
                r,
            );
        }
        return;
    }

    // ---- Claude Code
    if show_all || c_active {
        if let Some(r) = divider_slot(&mut y, max_y, drew) {
            draw_divider(f, Rect::new(area.x, r.y, area.width, r.height), width);
        }
        let c = &snap.claude;
        let mut stats = format!(
            "in {} · out {} · cache {}",
            fmt_tok(c.tokens.input),
            fmt_tok(c.tokens.output),
            fmt_tok(c.tokens.cache_read + c.tokens.cache_write),
        );
        if c.msgs > 0 {
            stats.push_str(&format!(" · {} msg{}", c.msgs, if c.msgs == 1 { "" } else { "s" }));
        }
        if let Some(r) = next_slot(&mut y, 1, max_y) {
            let r = Rect::new(area.x, r.y, area.width, r.height);
            put(f, r, title_line("Claude Code", &stats, &fmt_money(c.cost), width));
        }
        if let Some(r) = next_slot(&mut y, 1, max_y) {
            let r = Rect::new(area.x, r.y, area.width, r.height);
            put(f, r, budget_line("budget", c.tokens.total(), cfg.claude_budget, width));
        }
        for item in c.items.iter().take(2) {
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                put(f, r, item_line(&item.name, item.tokens.total(), width));
            }
        }
        drew = true;
    }

    // ---- Codex
    if show_all || x_active {
        if let Some(r) = divider_slot(&mut y, max_y, drew) {
            draw_divider(f, Rect::new(area.x, r.y, area.width, r.height), width);
        }
        let x = &snap.codex;
        let token_s = if x.has_token_data {
            format!(
                "in {} · out {} · cache {}",
                fmt_tok(x.tokens.input),
                fmt_tok(x.tokens.output),
                fmt_tok(x.tokens.cache_read + x.tokens.cache_write),
            )
        } else {
            format!("{} sessions · {} turns (no token data)", x.sessions, x.turns)
        };
        if let Some(r) = next_slot(&mut y, 1, max_y) {
            let r = Rect::new(area.x, r.y, area.width, r.height);
            put(f, r, title_line("Codex", &token_s, &fmt_money(x.cost), width));
        }
        drew = true;
    }

    // ---- OpenCode
    if let Some(o) = &snap.opencode {
        if show_all || o_active {
            if let Some(r) = divider_slot(&mut y, max_y, drew) {
                draw_divider(f, Rect::new(area.x, r.y, area.width, r.height), width);
            }
            let mut stats = format!(
                "in {} · out {} · cache {}",
                fmt_tok(o.tokens.input),
                fmt_tok(o.tokens.output),
                fmt_tok(o.tokens.cache_read + o.tokens.cache_write),
            );
            if o.sessions > 0 {
                stats.push_str(&format!(" · {} session{}", o.sessions, if o.sessions == 1 { "" } else { "s" }));
            }
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                put(f, r, title_line("OpenCode", &stats, &fmt_money(o.cost), width));
            }
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                put(f, r, budget_line("budget", o.tokens.total(), cfg.opencode_budget, width));
            }
            for item in o.items.iter().take(1) {
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    put(f, r, item_line(&item.name, item.tokens.total(), width));
                }
            }
            drew = true;
        }
    }

    // ---- OpenCode Go (official quota API)
    if show_all || ogo_active {
        if let Some(r) = divider_slot(&mut y, max_y, drew) {
            draw_divider(f, Rect::new(area.x, r.y, area.width, r.height), width);
        }
        let ogo = &snap.opencode_go;
        if ogo.needs_key {
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                f.render_widget(
                    Paragraph::new("OpenCode Go — no key (run `opencode login`, or set OPENCODE_API_KEY)")
                        .style(Style::default().fg(Color::DarkGray)),
                    r,
                );
            }
        } else if let Some(e) = &ogo.error {
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                f.render_widget(
                    Paragraph::new(format!("OpenCode Go — {e}")).style(Style::default().fg(Color::Red)),
                    r,
                );
            }
        } else {
            let mut tail = String::new();
            if let Some(w) = &ogo.rolling {
                if w.rate_limited() {
                    tail = "limit reached".into();
                } else {
                    tail = format!("rolling {}%", w.percent);
                }
            }
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                put(f, r, title_line("OpenCode Go", "official quota", &tail, width));
            }
            for (label, w) in [
                ("rolling", &ogo.rolling),
                ("weekly", &ogo.weekly),
                ("monthly", &ogo.monthly),
            ] {
                let Some(w) = w else { continue };
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    let frac = w.percent as f64 / 100.0;
                    let resets = crate::providers::opencode_go::format_resets_at(w.resets_at.as_deref());
                    let right = if w.rate_limited() {
                        format!("LIMIT · resets {resets}")
                    } else {
                        format!("{}% · resets {resets}", w.percent)
                    };
                    put(f, r, meter_line(label, frac, right, width));
                }
            }
        }
        drew = true;
    }

    // ---- Copilot
    if show_all || cp_active {
        if let Some(r) = divider_slot(&mut y, max_y, drew) {
            draw_divider(f, Rect::new(area.x, r.y, area.width, r.height), width);
        }
        match &snap.copilot {
            Status::Ok(cp) => {
                let meta = format!("{} · reset {}", cp.plan, cp.reset);
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    put(f, r, title_line("Copilot", "", &meta, width));
                }
                // only the quotas that actually run out (AI credits on
                // token-based plans); unlimited chat/completions are skipped.
                for q in cp.quotas.iter().filter(|q| !q.unlimited && q.used_pct.is_some()) {
                    if let Some(r) = next_slot(&mut y, 1, max_y) {
                        let r = Rect::new(area.x, r.y, area.width, r.height);
                        let frac = q.used_pct.unwrap() as f64 / 100.0;
                        let right = if q.entitlement > 0 && (q.used > 0 || q.entitlement > 0) {
                            format!("{:.0}% · {} / {}", frac * 100.0, crate::model::fmt_int(q.used), crate::model::fmt_int(q.entitlement))
                        } else {
                            format!("{:.0}%", frac * 100.0)
                        };
                        put(f, r, meter_line(&q.name, frac, right, width));
                    }
                }
            }
            other => {
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    f.render_widget(
                        Paragraph::new(format!("Copilot — {other}")).style(Style::default().fg(Color::DarkGray)),
                        r,
                    );
                }
            }
        }
        drew = true;
    }

    // ---- Grok
    if show_all || g_active {
        if let Some(r) = divider_slot(&mut y, max_y, drew) {
            draw_divider(f, Rect::new(area.x, r.y, area.width, r.height), width);
        }
        if snap.grok.needs_login {
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                f.render_widget(
                    Paragraph::new("Grok — needs 'grok login' or GROK_OAUTH_TOKEN")
                        .style(Style::default().fg(Color::DarkGray)),
                    r,
                );
            }
        } else if let Some(e) = &snap.grok.error {
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                f.render_widget(
                    Paragraph::new(format!("Grok — error ({e})")).style(Style::default().fg(Color::Red)),
                    r,
                );
            }
        } else if let Some(p) = snap.grok.used_pct {
            let resets = snap.grok.resets_at.as_deref().unwrap_or("—");
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                put(f, r, title_line("Grok", &format!("credits {p:.0}% used · resets {resets}"), "", width));
            }
        }
        if snap.grok.local_sessions > 0 {
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                put(
                    f,
                    r,
                    item_line(
                        &format!("grok local: {} sessions", snap.grok.local_sessions),
                        snap.grok.local_tokens,
                        width,
                    ),
                );
            }
        }
        drew = true;
    }

    // ---- OpenRouter
    if show_all || or_active {
        if let Some(r) = divider_slot(&mut y, max_y, drew) {
            draw_divider(f, Rect::new(area.x, r.y, area.width, r.height), width);
        }
        let or = &snap.openrouter;
        if let Some(e) = &or.error {
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                f.render_widget(
                    Paragraph::new(format!("OpenRouter — error ({e})")).style(Style::default().fg(Color::Red)),
                    r,
                );
            }
        } else {
            let bal = fmt_money(or.balance_usd);
            let tail = match or.key_limit_usd {
                Some(l) => format!("limit {}", fmt_money(l)),
                None => String::new(),
            };
            if let Some(r) = next_slot(&mut y, 1, max_y) {
                let r = Rect::new(area.x, r.y, area.width, r.height);
                put(f, r, title_line("OpenRouter", &format!("credits {bal}"), &tail, width));
            }
            // Credits usage bar (how much of the purchased credits is spent)
            if or.total_credits_usd > 0.0 {
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    let frac = (or.total_usage_usd / or.total_credits_usd).clamp(0.0, 1.0);
                    put(
                        f,
                        r,
                        meter_line(
                            "credits",
                            frac,
                            format!(
                                "{:.0}% · {} used · {} left",
                                frac * 100.0,
                                fmt_money(or.total_usage_usd),
                                fmt_money(or.balance_usd),
                            ),
                            width,
                        ),
                    );
                }
            }
            // API-key spending meter when a key limit is configured
            if let (Some(p), Some(limit)) = (or.used_pct, or.key_limit_usd) {
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    let used = or.key_used_usd.unwrap_or(0.0);
                    put(
                        f,
                        r,
                        meter_line(
                            "key budget",
                            p as f64 / 100.0,
                            format!("{:.0}% · {} / {}", p as f64, fmt_money(used), fmt_money(limit)),
                            width,
                        ),
                    );
                }
            }
            // daily / weekly / monthly key spend (whenever the API reports it)
            if or.usage_today > 0.0 || or.usage_week > 0.0 || or.usage_month > 0.0 {
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            format!(
                                "  spend  today {} · week {} · month {}",
                                fmt_money(or.usage_today),
                                fmt_money(or.usage_week),
                                fmt_money(or.usage_month),
                            ),
                            Style::default().fg(Color::DarkGray),
                        ))),
                        r,
                    );
                }
            } else if or.key_limit_usd.is_none() && or.total_usage_usd > 0.0 {
                // hint only when there IS usage but no per-period data (limit off)
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            "  hint: set a key spending limit to see today/week/month spend",
                            Style::default().fg(Color::DarkGray),
                        ))),
                        r,
                    );
                }
            }
            // per-model usage from the web dashboard (or_sync cache)
            if let Some(u) = &snap.or_usage {
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let stale = crate::openrouter::is_stale(u, now_unix);
                if let Some(r) = next_slot(&mut y, 1, max_y) {
                    let r = Rect::new(area.x, r.y, area.width, r.height);
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            format!(
                                "  month {}{}",
                                fmt_money(u.month_total),
                                if stale { " · stale — press R" } else { "" },
                            ),
                            Style::default().fg(Color::DarkGray),
                        ))),
                        r,
                    );
                }
                for m in u.month_models.iter().take(3) {
                    if let Some(r) = next_slot(&mut y, 1, max_y) {
                        let r = Rect::new(area.x, r.y, area.width, r.height);
                        put(f, r, model_line(&m.label, m.tokens, m.cost, width));
                    }
                }
                if !u.today_models.is_empty() {
                    if let Some(r) = next_slot(&mut y, 1, max_y) {
                        let r = Rect::new(area.x, r.y, area.width, r.height);
                        f.render_widget(
                            Paragraph::new(Line::from(Span::styled(
                                format!("  today {}", fmt_money(u.today_total)),
                                Style::default().fg(Color::DarkGray),
                            ))),
                            r,
                        );
                    }
                }
                for m in u.today_models.iter().take(2) {
                    if let Some(r) = next_slot(&mut y, 1, max_y) {
                        let r = Rect::new(area.x, r.y, area.width, r.height);
                        put(f, r, model_line(&m.label, m.tokens, m.cost, width));
                    }
                }
            }
        }
    }
}

/// Provider row: bold name, body, optional right-aligned tail (cost / plan).
/// Everything is clipped to `width` with an ellipsis instead of ratatui's raw
/// buffer clip, so narrow panes never slice a word mid-glyph.
fn title_line(name: &str, body: &str, tail: &str, width: usize) -> Line<'static> {
    let name_w = name.chars().count();
    let tail_w = tail.chars().count();
    let body_max = width.saturating_sub(name_w + 2 + tail_w + 2).max(1);
    let body = fit_width(body, body_max);
    let used = name_w + 2 + body.chars().count() + tail_w;
    let pad = width.saturating_sub(used);
    let mut spans = vec![
        Span::styled(name.to_string(), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::raw("  ".to_string()),
        Span::raw(body),
    ];
    if !tail.is_empty() {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(tail.to_string(), Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

/// Indented detail row: dim `  · name` with tokens right-aligned.
fn item_line(name: &str, tokens: u64, width: usize) -> Line<'static> {
    let right = if tokens > 0 { fmt_tok(tokens) } else { String::new() };
    let left = format!("    · {name}");
    let left_max = width.saturating_sub(right.chars().count() + 2).max(1);
    let left = fit_width(&left, left_max);
    let used = left.chars().count() + right.chars().count();
    let pad = width.saturating_sub(used).max(1);
    Line::from(vec![
        Span::styled(left, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(Color::DarkGray)),
    ])
}

/// Generic dim left/right row used for the OpenRouter model lines.
fn dim_line(left: &str, right: &str, width: usize) -> Line<'static> {
    let left_max = width.saturating_sub(right.chars().count() + 2).max(1);
    let left = fit_width(left, left_max);
    let used = left.chars().count() + right.chars().count();
    let pad = width.saturating_sub(used).max(1);
    Line::from(vec![
        Span::styled(left, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right.to_string(), Style::default().fg(Color::DarkGray)),
    ])
}

/// Model row: fixed label(28) | tokens(9) | cost(8) columns, so the token and
/// money values line up across the today/month sections.
fn model_line(label: &str, tokens: u64, cost: f64, width: usize) -> Line<'static> {
    let left = format!("    · {:<22}", fit_width(label, 22));
    let right = format!(
        "{:>9}  {:>8}",
        crate::model::fmt_compact(tokens),
        fmt_money(cost)
    );
    dim_line(&left, &right, width)
}

/// `  budget  10.00M  █████████░░░ 46%` — same bar style as the Copilot quota
/// rows below, with the `%` right-aligned to `width`.
fn budget_line(label: &str, total: u64, budget: u64, width: usize) -> Line<'static> {
    let frac = if budget > 0 { total as f64 / budget as f64 } else { 0.0 }.clamp(0.0, 1.0);
    let color = bar_color(frac);
    let bar = crate::model::bar(frac, 20);
    // fixed-width label column, same as meter_line, so bars line up vertically
    let left = format!("  {:<RLEN$}", format!("{label} {}", crate::model::fmt_tok(budget)), RLEN = BAR_LABEL_W);
    let right = format!("{:.0}%", frac * 100.0);
    // left + bar + pad + right must fit in `width`
    let pad = width
        .saturating_sub(left.chars().count() + 20 + right.chars().count())
        .max(2);
    Line::from(vec![
        Span::styled(left, Style::default().fg(Color::DarkGray)),
        Span::styled(bar, Style::default().fg(color)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

/// `  AI credits  ██████████████░░░░ 22% · 2,220 / 10,000` — a full-width
/// colored meter bar. `right` is the fully-formatted trailing label.
fn meter_line(name: &str, frac: f64, right: String, width: usize) -> Line<'static> {
    let frac = frac.clamp(0.0, 1.0);
    let color = bar_color(frac);
    let bar = crate::model::bar(frac, 20);
    // fixed-width label column (20 cols from the left edge) so every bar gets
    // the same start X regardless of the label's length.
    let left = format!("  {name:<RLEN$}", RLEN = BAR_LABEL_W);
    let pad = width
        .saturating_sub(left.chars().count() + 20 + right.chars().count())
        .max(1);
    Line::from(vec![
        Span::styled(left, Style::default().fg(Color::DarkGray)),
        Span::styled(bar, Style::default().fg(color)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

fn bar_color(frac: f64) -> Color {
    if frac > 0.7 {
        Color::Red
    } else if frac > 0.4 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Fixed width of the label column in bar rows (2-space indent + this), so the
/// `█░` bars all start at the same X regardless of label length.
const BAR_LABEL_W: usize = 18;

/// Truncate with an ellipsis to at most `max` chars.
fn fit_width(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    #[test]
    fn footer_click_ranges_pick_the_right_action() {
        // footer text: "[C] Connect   [R] Refresh   [q] Close"
        let layout = footer_layout();
        // Connect = cols 0..11, Refresh = 14..23, Close = 26..34
        assert_eq!(layout[0].0, Action::Connect);
        assert_eq!((layout[0].2, layout[0].3), (0, 11));
        assert_eq!(layout[1].0, Action::Refresh);
        assert_eq!((layout[1].2, layout[1].3), (14, 25));
        assert_eq!(layout[2].0, Action::Close);
        assert_eq!((layout[2].2, layout[2].3), (28, 37));

        let rect = Rect::new(0, 20, 80, 2);
        assert!(matches!(mouse_to_action(&click(5, 20), &rect), Some(Action::Connect)));
        assert!(matches!(mouse_to_action(&click(0, 20), &rect), Some(Action::Connect)));
        assert!(matches!(mouse_to_action(&click(10, 20), &rect), Some(Action::Connect)));
        assert!(matches!(mouse_to_action(&click(20, 20), &rect), Some(Action::Refresh)));
        assert!(matches!(mouse_to_action(&click(33, 20), &rect), Some(Action::Close)));
        // off the button row -> not clickable
        assert!(mouse_to_action(&click(5, 21), &rect).is_none());
        // gap between labels (separator) -> none
        assert!(mouse_to_action(&click(12, 20), &rect).is_none());
    }

    #[test]
    fn menu_hit_maps_rows_to_provider_indices() {
        // item rows start at modal.y + 3
        let mrect = Rect::new(10, 5, 50, 16);
        assert_eq!(menu_hit(&click(12, 8), mrect, 2), Some(0)); // y=5+3
        assert_eq!(menu_hit(&click(12, 9), mrect, 2), Some(1)); // y=5+4
        assert!(menu_hit(&click(12, 7), mrect, 2).is_none()); // title/blank
        assert!(menu_hit(&click(12, 12), mrect, 2).is_none()); // hint row
        assert!(menu_hit(&click(9, 8), mrect, 2).is_none()); // left of box
    }

    #[test]
    fn menu_arrow_keys_move_selection() {
        let snap = Snapshot::default();
        let mut modal = Some(open_menu(&snap));
        let cfg = crate::config::load();
        let mut next = Instant::now();

        // starts on index 0
        let Modal::Menu(menu) = modal.as_ref().unwrap() else { panic!() };
        assert_eq!(menu.selected, 0);

        // down -> 1
        handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()), &mut modal, &mut next, &cfg, &snap).unwrap();
        let Modal::Menu(menu) = modal.as_ref().unwrap() else { panic!() };
        assert_eq!(menu.selected, 1);

        // up -> wraps back to 0
        handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()), &mut modal, &mut next, &cfg, &snap).unwrap();
        let Modal::Menu(menu) = modal.as_ref().unwrap() else { panic!() };
        assert_eq!(menu.selected, 0);

        // Esc closes the modal
        handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &mut modal, &mut next, &cfg, &snap).unwrap();
        assert!(modal.is_none());
    }

    #[test]
    fn openrouter_key_entry_keeps_the_o() {
        let cfg = crate::config::load();
        let mut modal = Some(start_login(LoginKind::OpenRouter, &cfg));
        let mut next = Instant::now();
        let snap = Snapshot::default();
        // pasting "sk-or-v1-…" is delivered as one char at a time; the 'o'
        // in "sk-or-" must land in the key, never be eaten as "open page".
        let key = "sk-or-v1-0123456789abcdef0123456789abcdef";
        for ch in key.chars() {
            handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()), &mut modal, &mut next, &cfg, &snap).unwrap();
        }
        let Modal::Login(ls) = modal.as_ref().unwrap() else { panic!() };
        assert_eq!(ls.input, key);
    }

    #[test]
    fn login_fold_shows_confirmation_before_closing() {
        fn login(done: bool, connected: bool) -> LoginState {
            LoginState {
                kind: LoginKind::Copilot,
                lines: Arc::new(Mutex::new(Vec::new())),
                done: Arc::new(AtomicBool::new(done)),
                connected: Arc::new(AtomicBool::new(connected)),
                connected_at: None,
                input: String::new(),
            }
        }
        let base = Instant::now();

        // not finished yet -> stay open
        let mut ls = login(false, false);
        assert!(!login_fold(&mut ls, base));

        // finished but failed -> stay open so the user can read the error
        let mut ls = login(true, false);
        assert!(!login_fold(&mut ls, base));

        // finished + connected: first tick records the start time, stays open
        let mut ls = login(true, true);
        assert!(!login_fold(&mut ls, base));
        // shortly after -> still showing the confirmation
        assert!(!login_fold(&mut ls, base + Duration::from_millis(500)));
        // after the window -> close (& caller refreshes)
        assert!(login_fold(&mut ls, base + Duration::from_millis(2000)));
    }

    #[test]
    fn menu_status_strings_are_produced() {
        let mut snap = Snapshot::default();
        snap.copilot = Status::NeedsLogin { hint: "login".into() };
        snap.grok.needs_login = true;
        snap.openrouter.needs_key = true;
        snap.opencode_go.needs_key = true;
        let Modal::Menu(menu) = open_menu(&snap) else { panic!() };
        assert_eq!(menu.items.len(), 8);

        let MenuEntry::Provider(p0) = &menu.items[0] else { panic!() };
        assert_eq!(p0.name, "GitHub Copilot");
        assert!(p0.status.contains("needs login"));
        let MenuEntry::Provider(p1) = &menu.items[1] else { panic!() };
        assert_eq!(p1.name, "Grok (xAI)");
        assert!(p1.status.contains("needs login"));
        let MenuEntry::Provider(p2) = &menu.items[2] else { panic!() };
        assert_eq!(p2.name, "OpenRouter");
        // default snapshot has no OpenRouter key -> needs API key
        assert!(p2.status.contains("needs API key"));
        let MenuEntry::Provider(p3) = &menu.items[3] else { panic!() };
        assert_eq!(p3.name, "OpenCode Go");
        // default snapshot has no opencode key -> needs key
        assert!(p3.status.contains("needs key"));
        // individual logouts, then the all-in-one
        assert!(matches!(menu.items[4], MenuEntry::Logout(LogoutKind::Copilot)));
        assert!(matches!(menu.items[5], MenuEntry::Logout(LogoutKind::OpenRouter)));
        assert!(matches!(menu.items[6], MenuEntry::Logout(LogoutKind::OpenCodeGo)));
        assert!(matches!(menu.items[7], MenuEntry::Logout(LogoutKind::All)));
    }

    #[test]
    fn releasing_or_dragging_never_dismisses_the_modal() {
        let footer = Rect::new(0, 20, 80, 2);       // row 20
        let mrect = Rect::new(10, 5, 50, 16);        // menu modal open
        let items = Some(2usize);

        // Hold-then-release after opening Connect must keep the modal open:
        // Up on the footer Connect column while a menu is open -> ignored.
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 5,
            row: 20,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(decide_mouse(&up, items, Some(mrect), &footer), MouseDecision::None);

        // A tiny drag (Down+move kind) while held must also be ignored.
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 30,
            row: 8,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(decide_mouse(&drag, items, Some(mrect), &footer), MouseDecision::None);

        // Release (Up) anywhere, even on a blank area, must not close it.
        let up_blank = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(decide_mouse(&up_blank, items, Some(mrect), &footer), MouseDecision::None);
    }

    #[test]
    fn real_clicks_still_act() {
        let footer = Rect::new(0, 20, 80, 2);
        let mrect = Rect::new(10, 5, 50, 16);

        // Opening Connect with no modal open -> OpenMenu (Down on footer row).
        assert_eq!(
            decide_mouse(&click(5, 20), None, None, &footer),
            MouseDecision::OpenMenu
        );
        // Down on a menu row -> connect that provider.
        assert_eq!(
            decide_mouse(&click(12, 8), Some(2), Some(mrect), &footer),
            MouseDecision::ConnectProvider(0)
        );
        assert_eq!(
            decide_mouse(&click(12, 9), Some(2), Some(mrect), &footer),
            MouseDecision::ConnectProvider(1)
        );
        // Down outside the menu -> dismiss.
        assert_eq!(
            decide_mouse(&click(70, 20), Some(2), Some(mrect), &footer),
            MouseDecision::Dismiss
        );
        // Login modal open -> mouse is ignored entirely, so the user can
        // select/copy the verification code without dismissing it.
        assert_eq!(
            decide_mouse(&click(50, 15), None, Some(mrect), &footer),
            MouseDecision::None
        );
        // ... including clicks right on the modal content, and drag/release.
        assert_eq!(
            decide_mouse(&click(12, 9), None, Some(mrect), &footer),
            MouseDecision::None
        );
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 40,
            row: 12,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(decide_mouse(&up, None, Some(mrect), &footer), MouseDecision::None);
        // Footer Close / Refresh.
        assert_eq!(decide_mouse(&click(33, 20), None, None, &footer), MouseDecision::Close);
        assert_eq!(decide_mouse(&click(20, 20), None, None, &footer), MouseDecision::Refresh);
    }
}
