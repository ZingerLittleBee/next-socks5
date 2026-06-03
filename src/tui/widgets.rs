//! Pure helpers and the ratatui render function for the dashboard.
//!
//! Everything that is *testable in isolation* (throughput math, the log ring
//! buffer, event formatting) lives here as plain functions/structs so it can be
//! unit-tested headlessly. The [`render`] function is integration glue that
//! draws those values onto a [`ratatui::Frame`] and is exercised by running the
//! binary, not by unit tests.

use std::collections::VecDeque;
use std::time::Duration;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table};
use ratatui::Frame;

use crate::metrics::ConnKind;

/// Compute throughput in KB/s from a byte delta over an elapsed duration.
///
/// Returns `0.0` if `dt` is zero (avoids division by zero). 1 KB = 1024 bytes.
pub fn rate_kbps(bytes_delta: u64, dt: Duration) -> f64 {
    let secs = dt.as_secs_f64();
    if secs == 0.0 {
        return 0.0;
    }
    (bytes_delta as f64 / 1024.0) / secs
}

/// Fixed-capacity log ring buffer.
///
/// `push` appends a line; when already at capacity the oldest entry is dropped.
/// [`lines`](LogRing::lines) yields the current entries oldest -> newest.
pub struct LogRing {
    buf: VecDeque<String>,
    cap: usize,
}

impl LogRing {
    /// Create a ring buffer holding at most `cap` lines. A `cap` of zero is
    /// clamped to 1 so the buffer can always hold the most recent line.
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap: cap.max(1),
        }
    }

    /// Append a line, dropping the oldest entry when at capacity.
    pub fn push(&mut self, line: String) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(line);
    }

    /// Number of lines currently stored.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the buffer holds no lines.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Iterate the current lines oldest -> newest.
    pub fn lines(&self) -> impl Iterator<Item = &String> {
        self.buf.iter()
    }
}

/// Human-readable byte count (B / KB / MB / GB).
fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let f = n as f64;
    if f >= GB {
        format!("{:.2} GB", f / GB)
    } else if f >= MB {
        format!("{:.2} MB", f / MB)
    } else if f >= KB {
        format!("{:.2} KB", f / KB)
    } else {
        format!("{n} B")
    }
}

/// Draw the full dashboard for the current [`DashboardState`](super::DashboardState).
pub fn render(frame: &mut Frame, state: &super::DashboardState) {
    let area = frame.area();

    // Title bar, then four stacked panels.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Length(4), // rate panel
            Constraint::Min(6),    // connections table
            Constraint::Length(6), // stats panel
            Constraint::Min(5),    // log panel
        ])
        .split(area);

    render_title(frame, chunks[0], state);
    render_rate(frame, chunks[1], state);
    render_connections(frame, chunks[2], state);
    render_stats(frame, chunks[3], state);
    render_log(frame, chunks[4], state);
}

fn render_title(frame: &mut Frame, area: Rect, state: &super::DashboardState) {
    let title = match &state.listen_addr {
        Some(addr) => format!(" next-socks5  -  listening on {addr}  (press q to quit) "),
        None => " next-socks5  (press q to quit) ".to_string(),
    };
    let p = Paragraph::new(title).style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(p, area);
}

fn render_rate(frame: &mut Frame, area: Rect, state: &super::DashboardState) {
    let snap = &state.snapshot;
    let text = vec![
        Line::from(format!(
            "Up:   {:>8.1} KB/s    total {}",
            state.up_kbps,
            human_bytes(snap.bytes_up)
        )),
        Line::from(format!(
            "Down: {:>8.1} KB/s    total {}",
            state.down_kbps,
            human_bytes(snap.bytes_down)
        )),
    ];
    let block = Block::default().borders(Borders::ALL).title("Throughput");
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn render_connections(frame: &mut Frame, area: Rect, state: &super::DashboardState) {
    let header = Row::new(vec![
        Cell::from("ID"),
        Cell::from("Source"),
        Cell::from("Target"),
        Cell::from("Kind"),
        Cell::from("Up"),
        Cell::from("Down"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .connections
        .iter()
        .map(|c| {
            let kind = match c.kind {
                ConnKind::Connect => "CONNECT",
                ConnKind::Udp => "UDP",
            };
            Row::new(vec![
                Cell::from(c.id.to_string()),
                Cell::from(c.src.to_string()),
                Cell::from(c.target.clone()),
                Cell::from(kind.to_string()),
                Cell::from(human_bytes(c.up)),
                Cell::from(human_bytes(c.down)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(6),
        Constraint::Percentage(25),
        Constraint::Percentage(30),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(12),
    ];
    let title = format!("Active connections ({})", state.connections.len());
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(table, area);
}

fn render_stats(frame: &mut Frame, area: Rect, state: &super::DashboardState) {
    let snap = &state.snapshot;
    // Per-error-code names for RFC 1928 reply codes 0x01..=0x08.
    const CODE_NAMES: [&str; 8] = [
        "general",
        "ruleset",
        "netunreach",
        "hostunreach",
        "connrefused",
        "ttl",
        "badcmd",
        "badatyp",
    ];
    let codes: Vec<String> = (1..=8)
        .map(|i| format!("0x{:02x} {}={}", i, CODE_NAMES[i - 1], snap.error_codes[i]))
        .collect();

    let text = vec![
        Line::from(format!(
            "conns total={}  active={}  ok={}  fail={}",
            snap.total_conns, snap.active_conns, snap.successes, snap.failures
        )),
        Line::from(codes[0..4].join("   ")),
        Line::from(codes[4..8].join("   ")),
    ];
    let block = Block::default().borders(Borders::ALL).title("Stats");
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn render_log(frame: &mut Frame, area: Rect, state: &super::DashboardState) {
    // Show the most recent lines that fit; List renders top -> bottom so we
    // take the tail of the (oldest -> newest) ring.
    let capacity = area.height.saturating_sub(2) as usize; // minus borders
    let lines: Vec<&String> = state.log.lines().collect();
    let start = lines.len().saturating_sub(capacity.max(1));
    let items: Vec<ListItem> = lines[start..]
        .iter()
        .map(|s| ListItem::new(Line::from(s.as_str())))
        .collect();
    let block = Block::default().borders(Borders::ALL).title("Log");
    frame.render_widget(List::new(items).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_kbps_one_kb_per_sec() {
        assert!((rate_kbps(1024, Duration::from_secs(1)) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rate_kbps_half_second() {
        // 2048 bytes = 2 KB over 0.5 s = 4 KB/s.
        assert!((rate_kbps(2048, Duration::from_millis(500)) - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rate_kbps_zero_duration_is_zero() {
        assert_eq!(rate_kbps(100, Duration::ZERO), 0.0);
    }

    #[test]
    fn logring_drops_oldest_at_capacity() {
        let mut ring = LogRing::new(3);
        ring.push("a".into());
        ring.push("b".into());
        ring.push("c".into());
        ring.push("d".into());
        assert_eq!(ring.len(), 3);
        let lines: Vec<&String> = ring.lines().collect();
        assert_eq!(
            lines,
            vec![&"b".to_string(), &"c".to_string(), &"d".to_string()]
        );
    }

    #[test]
    fn logring_within_capacity_preserves_order() {
        let mut ring = LogRing::new(5);
        assert!(ring.is_empty());
        ring.push("one".into());
        ring.push("two".into());
        assert_eq!(ring.len(), 2);
        let lines: Vec<&String> = ring.lines().collect();
        assert_eq!(lines, vec![&"one".to_string(), &"two".to_string()]);
    }
}
