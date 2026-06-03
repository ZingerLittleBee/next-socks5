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
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{broadcast, watch};

use crate::metrics::{ConnInfo, Event, Metrics, Snapshot};
use widgets::LogRing;

/// How often the dashboard re-samples metrics and redraws.
const TICK: Duration = Duration::from_millis(250);
/// Maximum number of log lines retained in the scrolling panel.
const LOG_CAPACITY: usize = 500;

/// Snapshot of everything the renderer needs for a single frame.
pub struct DashboardState {
    /// Most recent global counters sample.
    pub snapshot: Snapshot,
    /// Current upload throughput in KB/s.
    pub up_kbps: f64,
    /// Current download throughput in KB/s.
    pub down_kbps: f64,
    /// Scrolling log buffer.
    pub log: LogRing,
    /// Active connections at the last sample.
    pub connections: Vec<ConnInfo>,
    /// Optional listen address shown in the title bar.
    pub listen_addr: Option<String>,
}

impl DashboardState {
    fn new(listen_addr: Option<String>) -> Self {
        Self {
            snapshot: Snapshot::default(),
            up_kbps: 0.0,
            down_kbps: 0.0,
            log: LogRing::new(LOG_CAPACITY),
            connections: Vec::new(),
            listen_addr,
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
    metrics: Arc<Metrics>,
    mut events: broadcast::Receiver<Event>,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> io::Result<()> {
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let mut state = DashboardState::new(None);
    let mut last_snapshot = metrics.snapshot();
    let mut last_instant = Instant::now();
    let mut ticker = tokio::time::interval(TICK);

    loop {
        tokio::select! {
            // Periodic sample + redraw.
            _ = ticker.tick() => {
                let now = Instant::now();
                let dt = now.duration_since(last_instant);
                let snap = metrics.snapshot();

                state.up_kbps =
                    widgets::rate_kbps(snap.bytes_up.saturating_sub(last_snapshot.bytes_up), dt);
                state.down_kbps =
                    widgets::rate_kbps(snap.bytes_down.saturating_sub(last_snapshot.bytes_down), dt);

                // Drain whatever events are pending without blocking.
                drain_events(&mut events, &mut state.log);

                state.connections = metrics.connections();
                state.connections.sort_by_key(|c| c.id);
                state.snapshot = snap.clone();

                last_snapshot = snap;
                last_instant = now;

                terminal.draw(|f| widgets::render(f, &state))?;
            }

            // Key input: poll on a blocking thread so we never stall the runtime.
            res = tokio::task::spawn_blocking(poll_key) => {
                if let Ok(Ok(Some(action))) = res {
                    match action {
                        KeyAction::Quit => {
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                    }
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

    Ok(())
}

/// Drain all currently-queued events into the log ring, recording lag if the
/// receiver fell behind the broadcast channel.
fn drain_events(events: &mut broadcast::Receiver<Event>, log: &mut LogRing) {
    loop {
        match events.try_recv() {
            Ok(ev) => log.push(crate::metrics::format_event(&ev)),
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => break,
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                log.push(format!("(log lagged, dropped {n} events)"));
            }
        }
    }
}

/// Result of polling for a key the dashboard cares about.
enum KeyAction {
    Quit,
}

/// Poll crossterm for a key event for up to one tick. Returns `Some(Quit)` for
/// `q` or Ctrl-C, `None` otherwise. Runs on a blocking thread.
fn poll_key() -> io::Result<Option<KeyAction>> {
    if event::poll(TICK)? {
        if let CtEvent::Key(key) = event::read()? {
            // Only react to presses (Windows also emits Release/Repeat).
            if key.kind == KeyEventKind::Press {
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if key.code == KeyCode::Char('q') || ctrl_c {
                    return Ok(Some(KeyAction::Quit));
                }
            }
        }
    }
    Ok(None)
}
