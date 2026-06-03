//! Pure helpers and the ratatui render function for the dashboard.
//!
//! Everything that is *testable in isolation* (throughput math, the log ring
//! buffer, event formatting) lives here as plain functions/structs so it can be
//! unit-tested headlessly. The [`render`] function is integration glue that
//! draws those values onto a [`ratatui::Frame`] and is exercised by running the
//! binary, not by unit tests.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset, GraphType, List, ListItem, Paragraph, Row, Table,
};
use ratatui::Frame;

use crate::metrics::{ConnKind, Event};

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

/// Severity of a log line, used to colour the log panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Routine activity (connect, auth ok, free-form log).
    Info,
    /// Low-signal lifecycle noise (connection closed) — rendered dimmed.
    Dim,
    /// Something the operator may care about (auth failure, dropped logs).
    Warn,
    /// A request/relay error.
    Error,
}

impl Severity {
    /// The style used to render a line of this severity.
    fn style(self) -> Style {
        match self {
            Severity::Info => Style::default(),
            Severity::Dim => Style::default().fg(Color::DarkGray),
            Severity::Warn => Style::default().fg(Color::Yellow),
            Severity::Error => Style::default().fg(Color::Red),
        }
    }
}

/// Classify an [`Event`] into a log [`Severity`].
pub fn severity_of(ev: &Event) -> Severity {
    match ev {
        Event::Connect { .. } | Event::Log(_) => Severity::Info,
        Event::Closed { .. } => Severity::Dim,
        Event::Auth { ok, .. } => {
            if *ok {
                Severity::Info
            } else {
                Severity::Warn
            }
        }
        Event::Error { .. } => Severity::Error,
    }
}

/// One stored log line: its rendered text, a severity for colouring, and the
/// connection id it belongs to (so a later "closed" can update it in place).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub severity: Severity,
    pub text: String,
    pub conn_id: Option<u64>,
}

/// Fixed-capacity log ring buffer.
///
/// `push` appends a line; when already at capacity the oldest entry is dropped.
/// [`lines`](LogRing::lines) yields the current entries oldest -> newest.
pub struct LogRing {
    buf: VecDeque<LogLine>,
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

    /// Append a line with the given severity (and optional owning connection
    /// id), dropping the oldest when at capacity.
    pub fn push(&mut self, severity: Severity, text: String, conn_id: Option<u64>) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(LogLine {
            severity,
            text,
            conn_id,
        });
    }

    /// Mark a connection's existing log line as closed: dim it and append
    /// `closed`, rather than emitting a separate line. No-op if the line has
    /// already scrolled out of the ring.
    pub fn mark_closed(&mut self, id: u64) {
        for line in self.buf.iter_mut().rev() {
            if line.conn_id == Some(id) && line.severity != Severity::Dim {
                line.severity = Severity::Dim;
                line.text.push_str("  closed");
                return;
            }
        }
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
    pub fn lines(&self) -> impl Iterator<Item = &LogLine> {
        self.buf.iter()
    }
}

/// Fixed-capacity ring of recent per-tick throughput samples (KB/s), feeding
/// the throughput sparklines. Oldest samples drop off the front.
pub struct RateHistory {
    buf: VecDeque<u64>,
    cap: usize,
}

impl RateHistory {
    /// Create a history retaining at most `cap` samples (clamped to >= 1).
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap: cap.max(1),
        }
    }

    /// Append a sample, dropping the oldest when at capacity.
    pub fn push(&mut self, sample: u64) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(sample);
    }

    /// Current samples, oldest -> newest.
    pub fn samples(&self) -> Vec<u64> {
        self.buf.iter().copied().collect()
    }

    /// Highest sample currently retained (0 when empty).
    pub fn peak(&self) -> u64 {
        self.buf.iter().copied().max().unwrap_or(0)
    }

    /// Maximum number of samples retained (the chart's time window).
    pub fn capacity(&self) -> usize {
        self.cap
    }
}

/// Format a duration as `H:MM:SS` (hours uncapped), used for uptime.
pub fn fmt_hms(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Y-axis labels for the throughput chart, scaled to a single unit chosen from
/// the axis maximum (KB/s -> MB/s -> GB/s). The bottom (zero) label is left
/// blank so the axis isn't cluttered with a `0`. Returns `["", mid, max]`, or
/// `["", max]` when the range is too small for a distinct midpoint.
/// `ymax_kbps` is the axis maximum in KB/s.
pub fn rate_axis_labels(ymax_kbps: f64) -> Vec<String> {
    const MB: f64 = 1024.0;
    const GB: f64 = 1024.0 * 1024.0;
    let (div, unit) = if ymax_kbps >= GB {
        (GB, "GB/s")
    } else if ymax_kbps >= MB {
        (MB, "MB/s")
    } else {
        (1.0, "KB/s")
    };
    let fmt = |v: f64| {
        if div == 1.0 {
            format!("{:.0} {unit}", v / div)
        } else {
            format!("{:.1} {unit}", v / div)
        }
    };
    if ymax_kbps <= 1.0 {
        vec![String::new(), fmt(ymax_kbps.max(1.0))]
    } else {
        vec![String::new(), fmt(ymax_kbps / 2.0), fmt(ymax_kbps)]
    }
}

/// Success percentage of completed requests, e.g. `98.2%`; `--` when none yet.
pub fn fmt_pct(ok: u64, fail: u64) -> String {
    let total = ok + fail;
    if total == 0 {
        "--".to_string()
    } else {
        format!("{:.1}%", ok as f64 * 100.0 / total as f64)
    }
}

/// Format a connection age compactly for the narrow table column:
/// `<60s` -> `12s`, `<60m` -> `3m`, otherwise `1h2m`.
pub fn fmt_age(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
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

    // Title bar, throughput row, a main row (connections + log side by side),
    // then the stats footer.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // title bar
            Constraint::Length(10), // rate panel (text + sparklines)
            Constraint::Min(6),     // main row: connections | log
            Constraint::Length(6),  // stats panel
        ])
        .split(area);

    render_title(frame, chunks[0], state);
    render_rate(frame, chunks[1], state);

    // Connections on the left, log on the right, so the terminal width is used
    // instead of leaving a full-width log half-empty.
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(chunks[2]);
    render_connections(frame, main[0], state);
    render_log(frame, main[1], state);

    render_stats(frame, chunks[3], state);
}

fn render_title(frame: &mut Frame, area: Rect, state: &super::DashboardState) {
    let up = fmt_hms(state.start.elapsed());
    let active = state.snapshot.active_conns;
    let title = match &state.listen_addr {
        Some(addr) => {
            format!(" next-socks5  -  {addr}  -  up {up}  -  active {active}  -  q:quit ")
        }
        None => format!(" next-socks5  -  up {up}  -  active {active}  -  q:quit "),
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
    // Left: current rates + totals as text. Right: a combined Up/Down line
    // chart sharing one set of axes (x = time, y = KB/s).
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(40), Constraint::Min(24)])
        .split(area);

    let text = vec![
        Line::from(format!(
            "Up:   {:>8.1} KB/s  total {}",
            state.up_kbps,
            human_bytes(snap.bytes_up)
        )),
        Line::from(format!(
            "Down: {:>8.1} KB/s  total {}",
            state.down_kbps,
            human_bytes(snap.bytes_down)
        )),
        Line::from(format!(
            "peak  ↑{} ↓{} KB/s",
            state.up_history.peak(),
            state.down_history.peak()
        ))
        .style(Style::default().fg(Color::DarkGray)),
    ];
    let block = Block::default().borders(Borders::ALL).title("Throughput");
    frame.render_widget(Paragraph::new(text).block(block), cols[0]);

    // Pin the newest sample to the right edge ("now"); history fills leftward.
    let up = state.up_history.samples();
    let down = state.down_history.samples();
    let window = state.up_history.capacity();
    let offset = window.saturating_sub(up.len());
    let to_points = |series: &[u64]| -> Vec<(f64, f64)> {
        series
            .iter()
            .enumerate()
            .map(|(i, &v)| ((offset + i) as f64, v as f64))
            .collect()
    };
    let up_pts = to_points(&up);
    let down_pts = to_points(&down);

    let ymax = up
        .iter()
        .chain(down.iter())
        .copied()
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let y_labels = rate_axis_labels(ymax);

    let datasets = vec![
        Dataset::default()
            .name("↑ up")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Green))
            .data(&up_pts),
        Dataset::default()
            .name("↓ down")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan))
            .data(&down_pts),
    ];
    // The time window is RATE_HISTORY * TICK = 120 * 250ms = 30s.
    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).title("Trend (30s)"))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, (window.max(1) - 1) as f64])
                .labels(["-30s", "-15s", "now"]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, ymax])
                .labels(y_labels),
        );
    frame.render_widget(chart, cols[1]);
}

fn render_connections(frame: &mut Frame, area: Rect, state: &super::DashboardState) {
    let now = Instant::now();
    let header = Row::new(vec![
        Cell::from("ID"),
        Cell::from("Source"),
        Cell::from("Target"),
        Cell::from("Kind"),
        Cell::from("Up"),
        Cell::from("Down"),
        Cell::from("Age"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    // Rows that fit = area height minus the two borders and the header row.
    let total = state.connections.len();
    let avail = area.height.saturating_sub(3) as usize;
    let show = if total > avail && avail >= 1 {
        avail - 1 // reserve one row for the "+N more" marker
    } else {
        total
    };

    let mut rows: Vec<Row> = state
        .connections
        .iter()
        .take(show)
        .map(|c| {
            let kind = match c.kind {
                ConnKind::Connect => "CONNECT",
                ConnKind::Udp => "UDP",
            };
            let age = state
                .first_seen
                .get(&c.id)
                .map(|t| fmt_age(now.duration_since(*t)))
                .unwrap_or_default();
            Row::new(vec![
                Cell::from(c.id.to_string()),
                Cell::from(c.src.to_string()),
                Cell::from(c.target.clone()),
                Cell::from(kind.to_string()),
                Cell::from(human_bytes(c.up)),
                Cell::from(human_bytes(c.down)),
                Cell::from(age),
            ])
        })
        .collect();
    if show < total {
        rows.push(
            Row::new(vec![Cell::from(format!("… +{} more", total - show))])
                .style(Style::default().fg(Color::DarkGray)),
        );
    }

    let widths = [
        Constraint::Length(6),
        Constraint::Percentage(25),
        Constraint::Percentage(30),
        Constraint::Length(8),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(7),
    ];
    let title = format!("Active connections ({total})");
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
    // One span per error code; zero buckets are dimmed so non-zero ones pop.
    let code_span = |i: usize| -> Span {
        let count = snap.error_codes[i];
        let s = format!("0x{:02x} {}={}   ", i, CODE_NAMES[i - 1], count);
        let style = if count > 0 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Span::styled(s, style)
    };

    let text = vec![
        Line::from(format!(
            "conns total={}  active={}  ok={}  fail={}  ({})",
            snap.total_conns,
            snap.active_conns,
            snap.successes,
            snap.failures,
            fmt_pct(snap.successes, snap.failures),
        )),
        Line::from((1..=4).map(code_span).collect::<Vec<Span>>()),
        Line::from((5..=8).map(code_span).collect::<Vec<Span>>()),
    ];
    let block = Block::default().borders(Borders::ALL).title("Stats");
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn render_log(frame: &mut Frame, area: Rect, state: &super::DashboardState) {
    // Show the most recent lines that fit; List renders top -> bottom so we
    // take the tail of the (oldest -> newest) ring.
    let capacity = area.height.saturating_sub(2) as usize; // minus borders
    let lines: Vec<&LogLine> = state.log.lines().collect();
    let start = lines.len().saturating_sub(capacity.max(1));
    let items: Vec<ListItem> = lines[start..]
        .iter()
        .map(|l| ListItem::new(l.text.as_str()).style(l.severity.style()))
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
        for t in ["a", "b", "c", "d"] {
            ring.push(Severity::Info, t.into(), None);
        }
        assert_eq!(ring.len(), 3);
        let texts: Vec<&str> = ring.lines().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["b", "c", "d"]);
    }

    #[test]
    fn logring_within_capacity_preserves_order() {
        let mut ring = LogRing::new(5);
        assert!(ring.is_empty());
        ring.push(Severity::Info, "one".into(), None);
        ring.push(Severity::Warn, "two".into(), None);
        assert_eq!(ring.len(), 2);
        let lines: Vec<(Severity, &str)> =
            ring.lines().map(|l| (l.severity, l.text.as_str())).collect();
        assert_eq!(
            lines,
            vec![(Severity::Info, "one"), (Severity::Warn, "two")]
        );
    }

    #[test]
    fn mark_closed_updates_connect_line_in_place() {
        let mut ring = LogRing::new(10);
        ring.push(Severity::Info, "[#1] CONNECT open a -> b".into(), Some(1));
        ring.push(Severity::Info, "[#2] CONNECT open c -> d".into(), Some(2));

        ring.mark_closed(1);

        // No new line is added; the matching line is dimmed and ends with closed.
        assert_eq!(ring.len(), 2);
        let lines: Vec<&LogLine> = ring.lines().collect();
        assert_eq!(lines[0].severity, Severity::Dim);
        assert!(lines[0].text.ends_with("closed"));
        // The other connection is untouched.
        assert_eq!(lines[1].severity, Severity::Info);

        // Closing an unknown / already-gone id is a no-op.
        ring.mark_closed(999);
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn severity_classifies_events() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1080);
        assert_eq!(
            severity_of(&Event::Connect {
                id: 1,
                src,
                target: "x:80".into(),
                kind: ConnKind::Connect
            }),
            Severity::Info
        );
        assert_eq!(severity_of(&Event::Closed { id: 1 }), Severity::Dim);
        assert_eq!(
            severity_of(&Event::Auth { ok: true, user: "a".into() }),
            Severity::Info
        );
        assert_eq!(
            severity_of(&Event::Auth { ok: false, user: "a".into() }),
            Severity::Warn
        );
        assert_eq!(
            severity_of(&Event::Error { code: 5, msg: "x".into() }),
            Severity::Error
        );
    }

    #[test]
    fn rate_history_caps_and_orders() {
        let mut h = RateHistory::new(3);
        for v in [1, 2, 3, 4] {
            h.push(v);
        }
        assert_eq!(h.samples(), vec![2, 3, 4]);
    }

    #[test]
    fn fmt_hms_formats_hours_minutes_seconds() {
        assert_eq!(fmt_hms(Duration::from_secs(0)), "0:00:00");
        assert_eq!(fmt_hms(Duration::from_secs(75)), "0:01:15");
        assert_eq!(fmt_hms(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn rate_history_peak_tracks_max() {
        let mut h = RateHistory::new(5);
        assert_eq!(h.peak(), 0);
        for v in [3, 9, 1, 4] {
            h.push(v);
        }
        assert_eq!(h.peak(), 9);
    }

    #[test]
    fn rate_axis_labels_scale_unit_and_blank_zero() {
        // Bottom label is blank (no cluttering 0); idle shows just the max.
        assert_eq!(rate_axis_labels(1.0), vec!["", "1 KB/s"]);
        // Small KB/s range: integer KB/s labels.
        assert_eq!(rate_axis_labels(40.0), vec!["", "20 KB/s", "40 KB/s"]);
        // Large range scales to MB/s with one decimal.
        assert_eq!(rate_axis_labels(1500.0), vec!["", "0.7 MB/s", "1.5 MB/s"]);
    }

    #[test]
    fn fmt_pct_handles_zero_and_ratio() {
        assert_eq!(fmt_pct(0, 0), "--");
        assert_eq!(fmt_pct(99, 1), "99.0%");
        assert_eq!(fmt_pct(1, 1), "50.0%");
    }

    #[test]
    fn fmt_age_is_compact() {
        assert_eq!(fmt_age(Duration::from_secs(12)), "12s");
        assert_eq!(fmt_age(Duration::from_secs(185)), "3m");
        assert_eq!(fmt_age(Duration::from_secs(3720)), "1h2m");
    }
}
