//! The `--live` panel: one screen, one contrast, one plain-words verdict.
//!
//! It shows requests arriving over the whole run (with the frozen windows
//! underneath), then two p99s as bars on one shared scale: what the load tool
//! reports (under `--live`, phantom's estimate of it: server time, `sent_ns -
//! arrival_ns`, which is what a tool measuring from its actual sends sees;
//! under `phantom demo`, the tool's own measurement), and what its users
//! actually waited (open-loop truth, computed exactly as `phantom report`
//! does, at the arrival rate measured outside the freeze). Once the first
//! freeze has ended, a verdict says whether the load tool kept sending
//! through it.
//!
//! Times are on the server's own monotonic clock, with t = 0 at t0 (the first
//! request's arrival) as in the log. [`Monitor`] collects counts as requests
//! arrive and finish; [`Snapshot`] is a copy of them, and [`render_panel`]
//! turns one into text without any I/O.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::time::Instant;

use crate::report::{Completed, Percentiles, open_loop_truth};

/// One bucket of the timeline: 100 ms.
pub const BUCKET_NS: u64 = 100_000_000;
const BUCKETS_PER_S: u64 = 1_000_000_000 / BUCKET_NS;

/// The freeze check needs at least this many arrivals in the interval before
/// a stall.
pub const MIN_ARRIVALS_BEFORE: u64 = 20;

/// The "users waited" bar turns red above this multiple of the tool's p99.
pub const RED_RATIO: f64 = 3.0;

/// The panel is never wider than this, however wide the terminal.
pub const MAX_WIDTH: usize = 80;

/// Below this many terminal columns the p99s are plain numbers, not bars.
pub const BARS_MIN_WIDTH: usize = 50;

/// Width of the timeline's row labels.
const LABEL_W: usize = 19;

const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// A bar's fractional end: one to eight eighths of a column.
const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Continuation lines of wrapped text start with this.
const HANGING_INDENT: &str = "  ";

/// One stall window and the arrivals around it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StallCounts {
    /// Start of the window, in ns since t0.
    pub start_ns: u64,
    /// End of the window (exclusive), in ns since t0.
    pub end_ns: u64,
    /// Arrivals in the same-length interval just before the window.
    pub before: u64,
    /// Arrivals inside the window.
    pub during: u64,
}

impl StallCounts {
    /// Counts `arrivals_ns` around the window [start_ns, end_ns).
    pub fn from_arrivals(start_ns: u64, end_ns: u64, arrivals_ns: impl IntoIterator<Item = u64>) -> Self {
        let mut counts = Self {
            start_ns,
            end_ns,
            ..Self::default()
        };
        for arrival in arrivals_ns {
            counts.count(arrival);
        }
        counts
    }

    fn count(&mut self, arrival_ns: u64) {
        let len = self.end_ns - self.start_ns;
        if (self.start_ns..self.end_ns).contains(&arrival_ns) {
            self.during += 1;
        } else if (self.start_ns.saturating_sub(len)..self.start_ns).contains(&arrival_ns) {
            self.before += 1;
        }
    }

    /// Whether the client kept sending through the window: `Stopped` when
    /// arrivals inside it are under 10% of those in the same-length interval
    /// before it and that interval had at least [`MIN_ARRIVALS_BEFORE`];
    /// `KeptSending` otherwise.
    pub fn verdict(&self) -> Verdict {
        // during < 10% of before, in integers.
        if self.before >= MIN_ARRIVALS_BEFORE && self.during * 10 < self.before {
            Verdict::Stopped
        } else {
            Verdict::KeptSending
        }
    }
}

/// How the load tool behaved while the server was frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Arrivals fell below 10% of the rate before: it waited instead of
    /// sending, and will leave the wait out of its latencies.
    Stopped,
    /// Arrivals kept on: its numbers include the freeze.
    KeptSending,
}

/// The verdict in plain words; `ratio` is how many times the users' p99 is
/// the tool's.
pub fn verdict_sentence(verdict: Verdict, ratio: Option<f64>) -> String {
    match verdict {
        Verdict::Stopped => {
            let by = ratio.map_or_else(String::new, |r| format!(" by {}×", fmt_ratio(r)));
            format!("✗ Your load tool stopped sending while the server was frozen. It will under-report p99{by}.")
        }
        Verdict::KeptSending => "✓ Your load tool kept sending. Its numbers include the freeze.".to_owned(),
    }
}

/// `137` for 137.2, `4.2` for 4.21.
pub fn fmt_ratio(r: f64) -> String {
    if r >= 10.0 {
        format!("{r:.0}")
    } else {
        format!("{r:.1}")
    }
}

/// A copy of the counters at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// ns since t0; `None` before the first request.
    pub now_ns: Option<u64>,
    /// Arrivals per 100 ms bucket from t0; the last bucket holds `now_ns`
    /// (still filling).
    pub arrivals: Vec<u64>,
    /// Every request answered so far, in no particular order.
    pub completed: Vec<Completed>,
    /// Requests whose client went away first.
    pub abandoned: u64,
    /// The script's stall windows, sorted by start.
    pub stalls: Vec<StallCounts>,
}

/// The two p99s, and the rate the users' one assumes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Numbers {
    /// The p99 the load tool reports, in ns: its own measurement when
    /// `measured`, else phantom's estimate, the p99 of server time
    /// (`sent_ns - arrival_ns`).
    pub tool_p99_ns: Option<i64>,
    /// Whether `tool_p99_ns` is the tool's own measurement.
    pub measured: bool,
    /// p99 of open-loop truth at `rate_per_s`, in ns.
    pub truth_p99_ns: Option<i64>,
    /// The intended rate per second: when not `measured`, the median
    /// arrivals per 100 ms bucket outside the stall windows.
    pub rate_per_s: Option<f64>,
}

impl Numbers {
    /// `truth / tool`, when both are known and tool > 0.
    pub fn ratio(&self) -> Option<f64> {
        match (self.tool_p99_ns, self.truth_p99_ns) {
            (Some(s), Some(t)) if s > 0 => Some(t as f64 / s as f64),
            _ => None,
        }
    }
}

impl Snapshot {
    /// The intended rate, per second: the median of arrivals per complete
    /// 100 ms bucket, leaving out buckets that touch a stall window, the
    /// still-filling last bucket, and the idle ones after the last arrival.
    pub fn intended_rate_per_s(&self) -> Option<f64> {
        let complete = self.arrivals.len().saturating_sub(1);
        let last_active = self.arrivals[..complete].iter().rposition(|&a| a > 0)?;
        let mut counts: Vec<u64> = self.arrivals[..=last_active]
            .iter()
            .enumerate()
            .filter(|&(k, _)| {
                let (lo, hi) = (k as u64 * BUCKET_NS, (k as u64 + 1) * BUCKET_NS);
                !self.stalls.iter().any(|s| s.start_ns < hi && s.end_ns > lo)
            })
            .map(|(_, &a)| a)
            .collect();
        counts.sort_unstable();
        let n = counts.len();
        let median = match n {
            0 => return None,
            _ if n % 2 == 1 => counts[n / 2] as f64,
            _ => (counts[n / 2 - 1] + counts[n / 2]) as f64 / 2.0,
        };
        (median > 0.0).then_some(median * BUCKETS_PER_S as f64)
    }

    /// The two p99s so far, the tool's estimated from server time.
    pub fn numbers(&self) -> Numbers {
        let server = Percentiles::of(self.completed.iter().map(|r| r.sent_ns as i64 - r.arrival_ns as i64));
        let rate = self.intended_rate_per_s();
        let truth = rate.and_then(|rate| {
            let period_ns = (1e9 / rate).round() as i64;
            let (_, truth) = open_loop_truth(&self.completed, period_ns)?;
            Percentiles::of(truth)
        });
        Numbers {
            tool_p99_ns: server.map(|p| p.p99()),
            measured: false,
            truth_p99_ns: truth.map(|p| p.p99()),
            rate_per_s: rate,
        }
    }
}

/// What the panel's header shows.
#[derive(Debug, Clone)]
pub struct PanelHeader {
    /// The address phantom listens on.
    pub listen: String,
    /// Shown in the header in place of `watching ADDR`, such as `1/2 naive
    /// client`.
    pub title: Option<String>,
}

/// A text style; drawn only when colour is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Plain,
    Bold,
    Dim,
    Red,
    Green,
}

impl Style {
    fn paint(self, text: &str, color: bool) -> String {
        let code = match self {
            Style::Plain => return text.to_owned(),
            Style::Bold => "1",
            Style::Dim => "2",
            Style::Red => "1;31",
            Style::Green => "1;32",
        };
        if !color || text.is_empty() {
            return text.to_owned();
        }
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

/// Colour on stderr: only on a terminal, and never when `NO_COLOR` is set to
/// anything but the empty string (<https://no-color.org>).
pub fn use_color(no_color: Option<&std::ffi::OsStr>, is_terminal: bool) -> bool {
    is_terminal && no_color.is_none_or(|v| v.is_empty())
}

/// One output line: text to wrap in one style, or a line already laid out to
/// fit (parts in their own styles).
enum Out {
    Text(String, Style),
    Laid(Vec<(String, Style)>),
}

/// The visible width of a panel on a terminal `width` columns wide: one
/// column short of it (so the last column never wraps), and at most
/// [`MAX_WIDTH`].
pub fn panel_width(width: usize) -> usize {
    (width.max(20) - 1).min(MAX_WIDTH)
}

/// The panel for `snap` on a terminal `width` columns wide, showing
/// `numbers` (normally `snap.numbers()`): no line is longer than
/// [`panel_width`] visible characters, text wraps with a two-space hanging
/// indent and is never cut, and every line ends in a newline. `color` turns
/// ANSI styles on.
pub fn render_panel(header: &PanelHeader, snap: &Snapshot, numbers: &Numbers, width: usize, color: bool) -> String {
    let w = panel_width(width);
    let mut out = Vec::new();
    let left = match &header.title {
        Some(title) => format!("phantom · {title}"),
        None => format!("phantom · watching {}", header.listen),
    };
    let right = snap
        .now_ns
        .map_or_else(|| "t = –".to_owned(), |ns| format!("t = {:.1} s", ns as f64 / 1e9));
    if left.chars().count() + 2 + right.chars().count() <= w {
        let gap = w - left.chars().count() - right.chars().count();
        out.push(Out::Laid(vec![
            (left, Style::Bold),
            (" ".repeat(gap), Style::Plain),
            (right, Style::Dim),
        ]));
    } else {
        out.push(Out::Text(format!("{left}  {right}"), Style::Bold));
    }
    out.push(Out::Text(String::new(), Style::Plain));

    let Some(now) = snap.now_ns.filter(|_| !snap.arrivals.is_empty()) else {
        out.push(Out::Text("waiting for the first request".to_owned(), Style::Dim));
        if let Some(first) = snap.stalls.first() {
            out.push(Out::Text(freeze_scheduled(first), Style::Dim));
        }
        return finish(out, w, color);
    };

    // The whole run, several buckets per column if it does not fit.
    let spark_w = w.saturating_sub(LABEL_W).max(8);
    let per_column = snap.arrivals.len().div_ceil(spark_w);
    let columns: Vec<(u64, u64, u64)> = snap
        .arrivals
        .chunks(per_column)
        .enumerate()
        .map(|(c, chunk)| {
            let lo = (c * per_column) as u64;
            (
                chunk.iter().sum(),
                lo * BUCKET_NS,
                (lo + chunk.len() as u64) * BUCKET_NS,
            )
        })
        .collect();
    let arrivals: Vec<u64> = columns.iter().map(|c| c.0).collect();
    out.push(Out::Laid(vec![
        (format!("{:LABEL_W$}", "requests arriving"), Style::Plain),
        (sparkline(&arrivals), Style::Plain),
    ]));
    if !snap.stalls.is_empty() {
        let mut parts = vec![(format!("{:LABEL_W$}", "server frozen"), Style::Plain)];
        for &(_, lo, hi) in &columns {
            let frozen = snap.stalls.iter().any(|s| s.start_ns < hi && s.end_ns > lo);
            parts.push(if frozen {
                ("█".to_owned(), Style::Red)
            } else {
                ("·".to_owned(), Style::Dim)
            });
        }
        out.push(Out::Laid(parts));
    }
    out.push(Out::Text(String::new(), Style::Plain));

    out.push(Out::Text("p99 latency".to_owned(), Style::Bold));
    out.extend(p99_rows(numbers, w, width >= BARS_MIN_WIDTH));
    out.push(Out::Text(String::new(), Style::Plain));

    if let Some(first) = snap.stalls.first() {
        if now < first.start_ns {
            out.push(Out::Text(freeze_scheduled(first), Style::Dim));
        } else if now < first.end_ns {
            out.push(Out::Text(
                format!("server frozen until {:.1} s", first.end_ns as f64 / 1e9),
                Style::Dim,
            ));
        } else {
            let verdict = first.verdict();
            let style = match verdict {
                Verdict::Stopped => Style::Red,
                Verdict::KeptSending => Style::Green,
            };
            out.push(Out::Text(verdict_sentence(verdict, numbers.ratio()), style));
        }
    }
    if let Some(rate) = numbers.rate_per_s {
        let footnote = if numbers.measured {
            format!("your tool's own p99; users waited: truth from phantom's log at {rate:.0} req/s")
        } else if snap.stalls.is_empty() {
            format!("assumes {rate:.0} req/s intended, the median rate measured")
        } else {
            format!("assumes {rate:.0} req/s intended, the rate measured outside the freeze")
        };
        out.push(Out::Text(footnote, Style::Dim));
    }
    finish(out, w, color)
}

/// The two p99 rows under "p99 latency", `w` visible columns at most: with
/// `bars`, each label, a bar on one linear scale shared by both, and the
/// value right-aligned after it; without, just the labels and values.
/// "users waited" is red when it is more than [`RED_RATIO`] times the tool's
/// p99, and otherwise both bars are green.
fn p99_rows(numbers: &Numbers, w: usize, bars: bool) -> Vec<Out> {
    let tool_label = if numbers.measured {
        "your tool reports"
    } else {
        "your tool ≈ reports"
    };
    let rows = [
        (tool_label, numbers.tool_p99_ns),
        ("users waited", numbers.truth_p99_ns),
    ];
    let red = numbers.ratio().is_some_and(|r| r > RED_RATIO);
    let styles = if red {
        [Style::Plain, Style::Red]
    } else {
        [Style::Green, Style::Green]
    };
    let label_w = rows.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0) + 2;
    let values = rows.map(|(_, ns)| fmt_p99(ns));
    let value_w = values.iter().map(|v| v.chars().count()).max().unwrap_or(0);
    let max = rows.iter().filter_map(|&(_, ns)| ns).max().unwrap_or(0);
    let columns = w.saturating_sub(2 + label_w + 1 + value_w);
    rows.iter()
        .zip(values)
        .zip(styles)
        .map(|((&(label, ns), value), style)| {
            let mut parts = vec![(format!("  {label:label_w$}"), Style::Plain)];
            if bars {
                parts.push((bar(ns.unwrap_or(0), max, columns), style));
                parts.push((" ".to_owned(), Style::Plain));
            }
            let pad = value_w - value.chars().count();
            parts.push((" ".repeat(pad), Style::Plain));
            parts.push((value, Style::Bold));
            Out::Laid(parts)
        })
        .collect()
}

/// A horizontal bar for `value` on a scale where `max` fills `columns`,
/// padded with spaces to `columns`: `value / max * columns` wide, rounded to
/// the nearest eighth of a column (`▏▎▍▌▋▊▉█`). Any value above zero draws
/// at least `▏`.
pub fn bar(value: i64, max: i64, columns: usize) -> String {
    let eighths = if value <= 0 || max <= 0 || columns == 0 {
        0
    } else {
        let exact = value as f64 / max as f64 * columns as f64 * 8.0;
        (exact.round() as usize).clamp(1, columns * 8)
    };
    let mut out = "█".repeat(eighths / 8);
    if eighths % 8 > 0 {
        out.push(EIGHTHS[eighths % 8 - 1]);
    }
    let pad = columns - out.chars().count();
    out + &" ".repeat(pad)
}

fn freeze_scheduled(stall: &StallCounts) -> String {
    format!("freeze scheduled at {:.1} s", stall.start_ns as f64 / 1e9)
}

/// The line printed under the final panel at shutdown, wrapped to `width`.
pub fn render_summary(snap: &Snapshot, width: usize, color: bool) -> String {
    let t = snap.now_ns.map_or_else(
        || "before the first request".to_owned(),
        |ns| format!("at t = {:.1} s", ns as f64 / 1e9),
    );
    let line = format!(
        "phantom: stopped {t}: {} answered, {} abandoned",
        snap.completed.len(),
        snap.abandoned
    );
    finish(vec![Out::Text(line, Style::Dim)], panel_width(width), color)
}

/// A p99 to 3 significant figures: `0.853 ms`, `4.60 ms`, `468 ms`, `2.50 s`;
/// `–` for none.
pub fn fmt_p99(ns: Option<i64>) -> String {
    let Some(ns) = ns else {
        return "–".to_owned();
    };
    let ms = ns as f64 / 1e6;
    let (value, unit) = if ms.abs() >= 999.5 { (ms / 1e3, "s") } else { (ms, "ms") };
    if value == 0.0 {
        return format!("0 {unit}");
    }
    let mut decimals = (2 - value.abs().log10().floor() as i32).clamp(0, 6);
    // 9.996 rounds up to 10.00, a fourth figure: one decimal fewer.
    let scale = 10f64.powi(decimals);
    if decimals > 0 && (value.abs() * scale).round() >= 1000.0 {
        decimals -= 1;
    }
    let decimals = decimals as usize;
    format!("{value:.decimals$} {unit}")
}

/// One level per value, scaled to the largest; zero is the lowest level and
/// any value above zero is at least the second.
pub fn sparkline(values: &[u64]) -> String {
    let max = values.iter().copied().max().unwrap_or(0);
    values
        .iter()
        .map(|&v| {
            if v == 0 || max == 0 {
                LEVELS[0]
            } else {
                LEVELS[(v * 7).div_ceil(max) as usize]
            }
        })
        .collect()
}

/// Lays out `lines` at most `width` visible characters wide: text is
/// word-wrapped with a two-space hanging indent (a word longer than a line
/// is split across lines, never cut) and then styled line by line, so a
/// style never spans a line break.
fn finish(lines: Vec<Out>, width: usize, color: bool) -> String {
    let mut out = String::new();
    for line in lines {
        match line {
            Out::Laid(parts) => {
                for (text, style) in parts {
                    out.push_str(&style.paint(&text, color));
                }
                out.push('\n');
            }
            Out::Text(text, style) => {
                for wrapped in wrap(&text, width) {
                    let _ = writeln!(out, "{}", style.paint(&wrapped, color));
                }
            }
        }
    }
    out
}

/// `text` word-wrapped to `width`, continuations with a two-space hanging
/// indent; nothing is cut.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.chars().count() <= width {
        return vec![text.to_owned()];
    }
    let mut lines = Vec::new();
    let mut current = text[..text.len() - text.trim_start_matches(' ').len()].to_owned();
    let mut empty = true;
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        if !empty && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::replace(&mut current, HANGING_INDENT.to_owned()));
            empty = true;
        }
        if !empty {
            current.push(' ');
        }
        for ch in word.chars() {
            if current.chars().count() >= width {
                lines.push(std::mem::replace(&mut current, HANGING_INDENT.to_owned()));
            }
            current.push(ch);
        }
        empty = false;
    }
    lines.push(current);
    lines
}

/// Collects the panel's counts; cheap to clone, shared by every handler.
#[derive(Clone)]
pub struct Monitor(Arc<Live>);

struct Live {
    t0: OnceLock<Instant>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    arrivals: Vec<u64>,
    completed: Vec<Completed>,
    abandoned: u64,
    stalls: Vec<StallCounts>,
}

impl State {
    /// Arrivals in bucket `k`, adding any missing buckets up to it.
    fn bucket(&mut self, k: usize) -> &mut u64 {
        if self.arrivals.len() <= k {
            self.arrivals.resize(k + 1, 0);
        }
        &mut self.arrivals[k]
    }
}

fn bucket_of(ns: u64) -> usize {
    (ns / BUCKET_NS) as usize
}

impl Monitor {
    /// A monitor for a script with these stall windows ([start, end) in ns).
    pub(crate) fn new(windows_ns: &[(u64, u64)]) -> Self {
        let stalls = windows_ns
            .iter()
            .map(|&(start, end)| StallCounts::from_arrivals(start, end, []))
            .collect();
        Self(Arc::new(Live {
            t0: OnceLock::new(),
            state: Mutex::new(State {
                stalls,
                ..State::default()
            }),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // Counters stay usable even if a holder panicked.
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn arrive(&self, t0: Instant, arrival_ns: u64) {
        self.0.t0.get_or_init(|| t0);
        let mut s = self.state();
        *s.bucket(bucket_of(arrival_ns)) += 1;
        for stall in &mut s.stalls {
            stall.count(arrival_ns);
        }
    }

    /// A request's line was written: `completed` is `Some` for an answered
    /// one.
    pub(crate) fn finish(&self, completed: Option<Completed>) {
        let mut s = self.state();
        match completed {
            Some(c) => s.completed.push(c),
            None => s.abandoned += 1,
        }
    }

    /// The counters now.
    pub fn snapshot(&self) -> Snapshot {
        let now_ns = self
            .0
            .t0
            .get()
            .map(|t0| u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
        self.snapshot_at(now_ns)
    }

    fn snapshot_at(&self, now_ns: Option<u64>) -> Snapshot {
        let mut s = self.state();
        if let Some(now) = now_ns {
            s.bucket(bucket_of(now));
        }
        Snapshot {
            now_ns,
            arrivals: s.arrivals.clone(),
            completed: s.completed.clone(),
            abandoned: s.abandoned,
            stalls: s.stalls.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn header() -> PanelHeader {
        PanelHeader {
            listen: "127.0.0.1:8090".to_owned(),
            title: None,
        }
    }

    /// A client asked for 100 requests/s for 6 s against 1 ms service and a
    /// 2.0–2.5 s freeze, seen at `now_ms`. Closed loop: it waits for each
    /// reply, so it sends nothing during the freeze, then catches up
    /// back-to-back. Open loop: it sends on schedule throughout.
    fn run(closed: bool, stall: Option<(u64, u64)>, now_ms: u64) -> Snapshot {
        let windows: Vec<(u64, u64)> = stall.into_iter().map(|(s, e)| (s * MS, e * MS)).collect();
        let m = Monitor::new(&windows);
        let t0 = Instant::now();
        let mut done = 0;
        for seq in 0..600u64 {
            let intended = seq * 10 * MS;
            let arrival = if closed { intended.max(done) } else { intended };
            if arrival >= now_ms * MS {
                break;
            }
            let held = windows
                .iter()
                .find(|&&(s, e)| (s..e).contains(&arrival))
                .map_or(arrival, |&(_, e)| e);
            let sent = held + MS;
            m.arrive(t0, arrival);
            if sent < now_ms * MS {
                m.finish(Some(Completed {
                    seq,
                    arrival_ns: arrival,
                    sent_ns: sent,
                }));
            }
            done = sent;
        }
        m.snapshot_at(Some(now_ms * MS))
    }

    fn panel(snap: &Snapshot, width: usize, color: bool) -> String {
        render_panel(&header(), snap, &snap.numbers(), width, color)
    }

    fn visible(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|&c| c == 'm');
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The bar on the row labelled `label`, trailing padding removed.
    fn bar_of(panel: &str, label: &str) -> String {
        let line = panel
            .lines()
            .find(|l| l.trim_start().starts_with(label))
            .unwrap_or_else(|| panic!("no {label:?} row in\n{panel}"));
        let rest = &line[2 + line.trim_start().find(label).unwrap() + label.len()..];
        rest.trim_start().chars().take_while(|c| EIGHTHS.contains(c)).collect()
    }

    #[test]
    fn closed_loop_gets_the_red_verdict() {
        let snap = run(true, Some((2_000, 2_500)), 6_000);
        let numbers = snap.numbers();
        assert_eq!(numbers.rate_per_s, Some(100.0));
        assert!(!numbers.measured);
        // The tool's own view: everything but one request took 1 ms.
        assert_eq!(numbers.tool_p99_ns, Some(MS as i64));
        // Its users: the catch-up requests were up to ~490 ms late.
        let truth = numbers.truth_p99_ns.unwrap();
        assert!((400 * MS as i64..=501 * MS as i64).contains(&truth), "{truth}");
        let ratio = numbers.ratio().unwrap();
        let panel = panel(&snap, 100, true);
        let sentence = format!(
            "✗ Your load tool stopped sending while the server was frozen. It will under-report p99 by {}×.",
            fmt_ratio(ratio)
        );
        // Wrapped at 80 columns, every line of it red.
        assert!(panel.contains("\x1b[1;31m✗ Your load tool"), "{panel}");
        assert!(panel.contains("\x1b[1;31m  under-report p99 by"), "{panel}");
        let plain = visible(&panel);
        assert!(plain.replace("\n  ", " ").contains(&sentence), "{plain}");
        // The users' bar is red and fills the scale; the tool's is plain.
        let users = bar_of(&plain, "users waited");
        assert!(panel.contains(&format!("\x1b[1;31m{users}")), "{panel}");
        assert!(panel.contains("your tool ≈ reports  ▏"), "{panel}");
        assert!(!panel.contains("\x1b[1;32m▏"), "{panel}");
        // Both values bold, 3 significant figures.
        assert!(panel.contains("\x1b[1m1.00 ms\x1b[0m"), "{panel}");
        assert!(
            panel.contains(&format!("\x1b[1m{}\x1b[0m", fmt_p99(Some(truth)))),
            "{panel}"
        );
        assert!(panel.contains("assumes 100 req/s intended, the rate measured outside the freeze"));
    }

    #[test]
    fn open_loop_gets_the_green_verdict() {
        let snap = run(false, Some((2_000, 2_500)), 6_000);
        let panel = panel(&snap, 100, true);
        assert!(panel.contains("\x1b[1;32m✓ Your load tool kept sending. Its numbers include the freeze.\x1b[0m"));
        assert!(!panel.contains('✗'), "{panel}");
        // Both numbers include the freeze: both bars green.
        let numbers = snap.numbers();
        assert!(numbers.ratio().unwrap() <= RED_RATIO, "{numbers:?}");
        let plain = visible(&panel);
        for label in ["your tool ≈ reports", "users waited"] {
            let bar = bar_of(&plain, label);
            assert!(bar.chars().count() > 10, "{label}: {plain}");
            assert!(panel.contains(&format!("\x1b[1;32m{bar}")), "{label}: {panel}");
        }
    }

    #[test]
    fn before_and_during_the_freeze_and_without_one() {
        let panel_at = |closed, stall, now| panel(&run(closed, stall, now), 100, false);
        let p = panel_at(true, Some((2_000, 2_500)), 1_500);
        assert!(p.contains("freeze scheduled at 2.0 s"), "{p}");
        assert!(!p.contains('✗') && !p.contains('✓'), "{p}");
        let p = panel_at(true, Some((2_000, 2_500)), 2_200);
        assert!(p.contains("server frozen until 2.5 s"), "{p}");
        let p = panel_at(true, None, 6_000);
        assert!(!p.contains("server frozen") && !p.contains("freeze scheduled"), "{p}");
        assert!(!p.contains('✗') && !p.contains('✓'), "{p}");
        assert!(p.contains("requests arriving") && p.contains("users waited"));
        assert!(
            p.contains("assumes 100 req/s intended, the median rate measured"),
            "{p}"
        );
        let p = panel(&Snapshot::default(), 100, false);
        assert!(p.contains("waiting for the first request"), "{p}");
    }

    #[test]
    fn no_color_means_no_escape_codes() {
        let snap = run(true, Some((2_000, 2_500)), 6_000);
        assert!(panel(&snap, 100, true).contains('\x1b'));
        assert!(!panel(&snap, 100, false).contains('\x1b'));
        assert!(!panel(&snap, 40, false).contains('\x1b'));
        assert!(!render_summary(&snap, 100, false).contains('\x1b'));
        use std::ffi::OsStr;
        assert!(use_color(None, true));
        assert!(use_color(Some(OsStr::new("")), true));
        assert!(!use_color(Some(OsStr::new("1")), true));
        assert!(!use_color(None, false));
    }

    #[test]
    fn bar_widths_share_one_scale() {
        // 4.6 ms against 468 ms on 42 columns: 0.41 of a column, 3.3
        // eighths, so a single ▍; 468 ms fills all 42.
        let (small, big) = (4_600_000, 468_000_000);
        assert_eq!(bar(small, big, 42), format!("▍{}", " ".repeat(41)));
        assert_eq!(bar(big, big, 42), "█".repeat(42));
        // Anything above zero draws at least ▏; zero draws nothing.
        assert_eq!(bar(1, big, 42).trim_end(), "▏");
        assert_eq!(bar(0, big, 42), " ".repeat(42));
        assert_eq!(bar(big / 2, big, 42).trim_end(), "█".repeat(21));
        assert_eq!(bar(3, 8, 1), "▍");
        // Equal values, equal bars.
        let numbers = Numbers {
            tool_p99_ns: Some(400 * MS as i64),
            measured: true,
            truth_p99_ns: Some(400 * MS as i64),
            rate_per_s: Some(100.0),
        };
        let snap = run(false, Some((2_000, 2_500)), 6_000);
        let p = render_panel(&header(), &snap, &numbers, 100, false);
        let (tool, users) = (bar_of(&p, "your tool reports"), bar_of(&p, "users waited"));
        assert_eq!(tool, users, "{p}");
        assert!(tool.chars().all(|c| c == '█'), "{p}");
        // The values line up at the right edge.
        let rows: Vec<&str> = p.lines().filter(|l| l.ends_with("400 ms")).collect();
        assert_eq!(rows.len(), 2, "{p}");
        assert_eq!(rows[0].chars().count(), rows[1].chars().count());
    }

    #[test]
    fn capped_at_80_columns() {
        let snap = run(true, Some((2_000, 2_500)), 6_000);
        let mut h = header();
        h.title = Some("1/2 naive client".to_owned());
        for width in [200, 100, 81] {
            let p = visible(&render_panel(&h, &snap, &snap.numbers(), width, true));
            assert!(p.lines().all(|l| l.chars().count() <= MAX_WIDTH), "{width}:\n{p}");
            // One header line: title on the left, t on the right, at 80.
            let first = p.lines().next().unwrap();
            assert!(first.starts_with("phantom · 1/2 naive client") && first.ends_with("t = 6.0 s"));
            assert_eq!(first.chars().count(), MAX_WIDTH, "{first:?}");
            assert!(!p.contains("watching"), "{p}");
            for label in ["your tool ≈ reports", "users waited"] {
                let row = p.lines().find(|l| l.trim_start().starts_with(label)).unwrap();
                assert_eq!(row.chars().count(), MAX_WIDTH, "{row:?}");
            }
        }
        // Narrower terminals: one column short of them.
        let p = visible(&panel(&snap, 70, true));
        assert_eq!(p.lines().next().unwrap().chars().count(), 69);
        assert!(p.lines().all(|l| l.chars().count() <= 69), "{p}");
    }

    #[test]
    fn fits_60_columns() {
        let snap = run(true, Some((2_000, 2_500)), 6_000);
        let p = visible(&panel(&snap, 60, true));
        for line in p.lines() {
            assert!(line.chars().count() < 60, "{line:?}\n{p}");
        }
        // The whole run fits: the frozen columns sit under the gap in
        // arrivals, column for column.
        let arriving = p.lines().find(|l| l.starts_with("requests arriving")).unwrap();
        let frozen = p.lines().find(|l| l.starts_with("server frozen")).unwrap();
        assert_eq!(arriving.chars().count(), frozen.chars().count(), "{p}");
        let spark: Vec<char> = arriving.chars().skip(LABEL_W).collect();
        let under: Vec<char> = frozen.chars().skip(LABEL_W).collect();
        let first = under.iter().position(|&f| f == '█').unwrap();
        assert!(spark[first + 1] <= '▂', "no gap in arrivals under the freeze: {p}");
        assert!(!bar_of(&p, "users waited").is_empty(), "{p}");
        // The verdict wraps with a two-space hanging indent, every word kept.
        let start = p.find('✗').unwrap();
        let verdict: Vec<&str> = p[start..].lines().take_while(|l| !l.starts_with("assumes")).collect();
        assert!(verdict.len() > 1, "{p}");
        assert!(
            verdict[1..]
                .iter()
                .all(|l| l.starts_with("  ") && !l.starts_with("   "))
        );
        assert!(verdict.join(" ").contains("p99"), "{p}");
    }

    #[test]
    fn under_50_columns_the_numbers_are_plain() {
        let snap = run(true, Some((2_000, 2_500)), 6_000);
        let p = panel(&snap, 49, false);
        let truth = fmt_p99(snap.numbers().truth_p99_ns);
        let lines: Vec<&str> = p.lines().skip_while(|l| *l != "p99 latency").skip(1).take(2).collect();
        assert!(!lines.concat().chars().any(|c| EIGHTHS.contains(&c)), "{p}");
        assert_eq!(lines[0], "  your tool ≈ reports  1.00 ms", "{p}");
        assert_eq!(lines[1], format!("  {:21}{truth:>7}", "users waited"), "{p}");
        assert!(p.lines().all(|l| l.chars().count() < 49), "{p}");
        // 50 has bars again.
        assert!(panel(&snap, 50, false).contains('▏'));
    }

    #[test]
    fn three_significant_figures() {
        assert_eq!(fmt_p99(Some(4_600_000)), "4.60 ms");
        assert_eq!(fmt_p99(Some(4_740_000)), "4.74 ms");
        assert_eq!(fmt_p99(Some(468 * MS as i64)), "468 ms");
        assert_eq!(fmt_p99(Some(853_000)), "0.853 ms");
        assert_eq!(fmt_p99(Some(12_345_678)), "12.3 ms");
        assert_eq!(fmt_p99(Some(9_996_000)), "10.0 ms");
        assert_eq!(fmt_p99(Some(99_960_000)), "100 ms");
        assert_eq!(fmt_p99(Some(999_600_000)), "1.00 s");
        assert_eq!(fmt_p99(Some(2_500 * MS as i64)), "2.50 s");
        assert_eq!(fmt_p99(Some(0)), "0 ms");
        assert_eq!(fmt_p99(None), "–");
    }

    #[test]
    fn verdict_heuristic_thresholds() {
        let counts = |before, during| StallCounts {
            start_ns: 0,
            end_ns: 1,
            before,
            during,
        };
        assert_eq!(counts(19, 0).verdict(), Verdict::KeptSending);
        assert_eq!(counts(20, 1).verdict(), Verdict::Stopped);
        assert_eq!(counts(50, 4).verdict(), Verdict::Stopped);
        assert_eq!(counts(50, 5).verdict(), Verdict::KeptSending);
        let c = StallCounts::from_arrivals(1_000, 1_500, [400, 500, 999, 1_000, 1_499, 1_500]);
        assert_eq!((c.before, c.during), (2, 2));
    }

    #[test]
    fn wrapping_never_cuts() {
        let long = "x".repeat(130);
        let lines = wrap(&format!("see {long}"), 59);
        assert_eq!(lines.concat().chars().filter(|&c| c == 'x').count(), 130);
        assert!(lines.iter().all(|l| l.chars().count() <= 59));
        assert!(lines[1..].iter().all(|l| l.starts_with(HANGING_INDENT)));
    }
}
