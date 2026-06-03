//! Terminal dashboard for the SOCKS5 server.
//!
//! The bulk of the testable logic (throughput math, the log ring, event
//! formatting, layout) lives in [`widgets`]. This module owns the runtime: it
//! sets up the terminal, runs a `tokio::select!` event loop that samples
//! [`Metrics`], drains the broadcast [`Event`] bus, draws frames, and reacts to
//! key input and the shutdown channel. Terminal state is restored on drop via
//! an RAII guard and a panic hook, so neither a clean exit nor a panic leaves
//! the user's terminal in raw/alternate-screen mode.

pub mod widgets;

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{broadcast, mpsc, watch};

use std::collections::HashMap;

use crate::metrics::{ConnInfo, Event, MetricsSource, Snapshot};
use widgets::{Focus, LogRing, RateHistory, SortKey};

/// How often the dashboard re-samples metrics and redraws.
const TICK: Duration = Duration::from_millis(250);
/// Maximum number of log lines retained for scrollback.
const LOG_CAPACITY: usize = 1000;
/// How many throughput samples the sparklines retain (~30s at 250ms/tick).
const RATE_HISTORY: usize = 120;

/// Snapshot of everything the renderer needs for a single frame.
pub struct DashboardState {
    /// Most recent global counters sample.
    pub snapshot: Snapshot,
    /// Current upload throughput in KB/s.
    pub up_kbps: f64,
    /// Current download throughput in KB/s.
    pub down_kbps: f64,
    /// Recent upload-rate samples (KB/s) for the sparkline.
    pub up_history: RateHistory,
    /// Recent download-rate samples (KB/s) for the sparkline.
    pub down_history: RateHistory,
    /// Scrolling log buffer.
    pub log: LogRing,
    /// Active connections at the last sample.
    pub connections: Vec<ConnInfo>,
    /// First time each still-active connection id was observed, for age.
    pub first_seen: HashMap<u64, Instant>,
    /// When the dashboard started, for the uptime readout.
    pub start: Instant,
    /// Optional listen address shown in the title bar.
    pub listen_addr: Option<String>,
    /// Current connection-table sort order.
    pub sort: SortKey,
    /// Which panel the scroll keys act on.
    pub focus: Focus,
    /// Scroll offset (rows from the top) of the connections table.
    pub conn_scroll: usize,
    /// Scroll offset (lines from the top) of the log panel. `0` keeps the view
    /// pinned to the newest line (tailing).
    pub log_scroll: usize,
}

impl DashboardState {
    fn new(listen_addr: Option<String>) -> Self {
        Self {
            snapshot: Snapshot::default(),
            up_kbps: 0.0,
            down_kbps: 0.0,
            up_history: RateHistory::new(RATE_HISTORY),
            down_history: RateHistory::new(RATE_HISTORY),
            log: LogRing::new(LOG_CAPACITY),
            connections: Vec::new(),
            first_seen: HashMap::new(),
            start: Instant::now(),
            listen_addr,
            sort: SortKey::Id,
            focus: Focus::Connections,
            conn_scroll: 0,
            log_scroll: 0,
        }
    }
}

/// RAII guard that restores the terminal (disable raw mode + leave the
/// alternate screen) when dropped, including during a panic-driven unwind.
struct TerminalGuard;

impl TerminalGuard {
    /// Enter raw mode and the alternate screen, returning a guard that undoes
    /// both on drop.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore: ignore errors because we may already be
        // unwinding from a panic and there is nothing useful to do on failure.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Install a panic hook that restores the terminal before the previous hook
/// runs, so a panic does not leave the terminal in raw/alternate-screen mode.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
}

/// Run the dashboard event loop until shutdown.
///
/// Sets up the terminal (raw mode + alternate screen) behind a [`TerminalGuard`]
/// and installs a panic hook so the terminal is always restored. On `q` or
/// Ctrl-C the function sets `shutdown` to `true` and returns; it also returns
/// when some other party flips the shutdown watch channel.
pub async fn run(
    source: Arc<dyn MetricsSource>,
    mut events: broadcast::Receiver<Event>,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
    listen_addr: Option<String>,
) -> io::Result<()> {
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let mut state = DashboardState::new(listen_addr);
    let mut last_snapshot = source.snapshot();
    let mut last_instant = Instant::now();
    let mut ticker = tokio::time::interval(TICK);

    // Read key input on a dedicated thread that forwards actions over a channel.
    // Polling directly inside `select!` via `spawn_blocking` loses keystrokes:
    // when the tick branch wins the race, the in-flight blocking read is
    // detached but keeps running and swallows the next key. Queued channel
    // messages, by contrast, survive `select!` cancelling the recv future, so
    // every key lands and scrolling stays smooth.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<KeyAction>();
    let reader_stop = Arc::new(AtomicBool::new(false));
    let reader_handle = {
        let stop = reader_stop.clone();
        std::thread::spawn(move || input_loop(&key_tx, &stop))
    };

    loop {
        tokio::select! {
            // Periodic sample + redraw.
            _ = ticker.tick() => {
                let now = Instant::now();
                let dt = now.duration_since(last_instant);
                let snap = source.snapshot();

                state.up_kbps =
                    widgets::rate_kbps(snap.bytes_up.saturating_sub(last_snapshot.bytes_up), dt);
                state.down_kbps =
                    widgets::rate_kbps(snap.bytes_down.saturating_sub(last_snapshot.bytes_down), dt);
                state.up_history.push(state.up_kbps as u64);
                state.down_history.push(state.down_kbps as u64);

                // Drain whatever events are pending without blocking. When the
                // user has scrolled the log up (not tailing), keep their view
                // anchored on the same lines by pushing the offset down by the
                // number of lines just appended.
                let added = drain_events(&mut events, &mut state.log);
                if state.log_scroll > 0 {
                    let avail = panel_avail(terminal.size()?.height).1;
                    let max_off = state.log.len().saturating_sub(avail);
                    state.log_scroll = (state.log_scroll + added).min(max_off);
                }

                state.snapshot = snap.clone();

                // Track when each connection was first observed (for age) and
                // forget ids that are no longer active, then sort by the chosen
                // key (age needs the freshly-updated first_seen map).
                let mut conns = source.connections();
                for c in &conns {
                    state.first_seen.entry(c.id).or_insert(now);
                }
                state
                    .first_seen
                    .retain(|id, _| conns.iter().any(|c| c.id == *id));
                widgets::sort_connections(&mut conns, state.sort, &state.first_seen);
                state.connections = conns;

                last_snapshot = snap;
                last_instant = now;

                terminal.draw(|f| widgets::render(f, &state))?;
            }

            // Key input forwarded from the reader thread.
            maybe_action = key_rx.recv() => {
                match maybe_action {
                    Some(KeyAction::Quit) => {
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                    Some(action) => {
                        let (conn_avail, log_avail) = panel_avail(terminal.size()?.height);
                        apply_action(&mut state, action, conn_avail, log_avail);
                        terminal.draw(|f| widgets::render(f, &state))?;
                    }
                    // Reader thread exited; nothing more will arrive.
                    None => break,
                }
            }

            // External shutdown request.
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    // Signal the reader thread to stop and wait for it so it doesn't outlive the
    // restored terminal.
    reader_stop.store(true, Ordering::Relaxed);
    let _ = reader_handle.join();

    Ok(())
}

/// Drain all currently-queued events into the log ring, recording lag if the
/// receiver fell behind the broadcast channel. Returns the number of lines
/// appended (in-place "closed" updates do not count) so the caller can keep a
/// scrolled-up log view anchored.
fn drain_events(events: &mut broadcast::Receiver<Event>, log: &mut LogRing) -> usize {
    let mut added = 0;
    loop {
        match events.try_recv() {
            // A close updates the connection's existing line in place rather
            // than emitting a separate "[#id] closed" entry.
            Ok(Event::Closed { id }) => log.mark_closed(id),
            Ok(ev) => {
                let conn_id = match &ev {
                    Event::Connect { id, .. } => Some(*id),
                    _ => None,
                };
                log.push(
                    widgets::severity_of(&ev),
                    crate::metrics::format_event(&ev),
                    conn_id,
                );
                added += 1;
            }
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => break,
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                log.push(
                    widgets::Severity::Warn,
                    format!("(log lagged, dropped {n} events)"),
                    None,
                );
                added += 1;
            }
        }
    }
    added
}

/// Number of data rows the connections table and log panel can show, derived
/// from the terminal height: the title bar (1) and info band sit above the main
/// row, the table loses two borders plus a header, the log loses two borders.
fn panel_avail(term_height: u16) -> (usize, usize) {
    let main = term_height.saturating_sub(1 + widgets::INFO_BAND_HEIGHT);
    let conn = main.saturating_sub(3) as usize;
    let log = main.saturating_sub(2) as usize;
    (conn, log)
}

/// Apply a non-quit key action to the dashboard state, clamping scroll offsets
/// against the panel sizes so over-scrolling settles at the edge.
fn apply_action(state: &mut DashboardState, action: KeyAction, conn_avail: usize, log_avail: usize) {
    let conn_max = state.connections.len().saturating_sub(conn_avail.max(1));
    let log_max = state.log.len().saturating_sub(log_avail.max(1));
    match action {
        // Already handled by the caller; kept exhaustive for clarity.
        KeyAction::Quit => {}
        KeyAction::CycleSort => {
            state.sort = state.sort.next();
            state.conn_scroll = 0;
        }
        KeyAction::SwitchFocus => state.focus = state.focus.next(),
        // Connections scroll from the top (down = toward the end); the log
        // scrolls from the bottom (up = toward older lines, away from the tail).
        KeyAction::ScrollUp(n) => match state.focus {
            Focus::Connections => state.conn_scroll = state.conn_scroll.saturating_sub(n),
            Focus::Log => state.log_scroll = (state.log_scroll + n).min(log_max),
        },
        KeyAction::ScrollDown(n) => match state.focus {
            Focus::Connections => state.conn_scroll = (state.conn_scroll + n).min(conn_max),
            Focus::Log => state.log_scroll = state.log_scroll.saturating_sub(n),
        },
    }
}

/// A key the dashboard reacts to. Scroll variants carry their line count so the
/// same handler serves both arrow keys (1) and Page Up/Down (one screen).
enum KeyAction {
    Quit,
    /// Cycle the connection-table sort order.
    CycleSort,
    /// Move focus between the connections table and the log.
    SwitchFocus,
    /// Scroll the focused panel up by `n` lines (toward older content).
    ScrollUp(usize),
    /// Scroll the focused panel down by `n` lines (toward newer content).
    ScrollDown(usize),
}

/// One screen's worth of lines for Page Up / Page Down.
const PAGE: usize = 10;

/// How long the reader thread blocks per poll before re-checking the stop flag.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// Map a crossterm key press to a [`KeyAction`], or `None` if it isn't one we
/// handle.
fn map_key(key: KeyEvent) -> Option<KeyAction> {
    // Only react to presses (Windows also emits Release/Repeat).
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl_c =
        key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        _ if ctrl_c => Some(KeyAction::Quit),
        KeyCode::Char('q') => Some(KeyAction::Quit),
        KeyCode::Char('s') => Some(KeyAction::CycleSort),
        KeyCode::Tab => Some(KeyAction::SwitchFocus),
        KeyCode::Up | KeyCode::Char('k') => Some(KeyAction::ScrollUp(1)),
        KeyCode::Down | KeyCode::Char('j') => Some(KeyAction::ScrollDown(1)),
        KeyCode::PageUp => Some(KeyAction::ScrollUp(PAGE)),
        KeyCode::PageDown => Some(KeyAction::ScrollDown(PAGE)),
        _ => None,
    }
}

/// Reader-thread loop: block on terminal input and forward each mapped action
/// over `tx`. Returns when `stop` is set, the channel closes, or crossterm
/// errors. Polling (rather than an unbounded `read`) lets it observe `stop`
/// within [`INPUT_POLL`] even when no keys arrive.
fn input_loop(tx: &mpsc::UnboundedSender<KeyAction>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match event::poll(INPUT_POLL) {
            Ok(true) => {
                if let Ok(CtEvent::Key(key)) = event::read() {
                    if let Some(action) = map_key(key) {
                        if tx.send(action).is_err() {
                            break; // receiver gone
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(_) => break,
        }
    }
}
