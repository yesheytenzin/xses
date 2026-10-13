//! Progress reporting for long operations.
//!
//! Drawn on stderr, only when stderr is a terminal: logs stay clean when
//! output is piped or redirected, and `XSES_NO_PROGRESS=1` turns it off when
//! someone wants a quiet terminal. The bar is redrawn at most every 100ms and
//! cleared when the operation reports its own summary.
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Bar width, in characters.
const WIDTH: usize = 24;

struct Inner {
    enabled: bool,
    total: AtomicUsize,
    done: AtomicUsize,
    failed: AtomicUsize,
    label: Mutex<String>,
    start: Mutex<Instant>,
    drawn_at: Mutex<Option<Instant>>,
    drawn: AtomicUsize,
}

/// Cheap clonable handle; workers update it from their own threads.
#[derive(Clone)]
pub struct Progress(Arc<Inner>);

impl Default for Progress {
    fn default() -> Self {
        Self::disabled()
    }
}

impl Progress {
    /// A bar for `total` units of work. Enabled only on an interactive stderr.
    pub fn new(total: usize) -> Self {
        let enabled =
            std::io::stderr().is_terminal() && std::env::var_os("XSES_NO_PROGRESS").is_none();
        Self::with_enabled(total, enabled)
    }

    pub fn with_enabled(total: usize, enabled: bool) -> Self {
        Self(Arc::new(Inner {
            enabled,
            total: AtomicUsize::new(total),
            done: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            label: Mutex::new(String::new()),
            start: Mutex::new(Instant::now()),
            drawn_at: Mutex::new(None),
            drawn: AtomicUsize::new(0),
        }))
    }

    pub fn disabled() -> Self {
        Self::with_enabled(0, false)
    }

    /// Grow the plan (targets are discovered as work is queued).
    pub fn add_total(&self, more: usize) {
        self.0.total.fetch_add(more, Ordering::Relaxed);
        self.redraw("", false);
    }

    /// Report what is happening without counting a unit (e.g. a phase that
    /// runs before the counted work, like transcript extraction).
    pub fn status(&self, label: &str) {
        self.redraw(label, false);
    }

    /// Force a frame (used when a long phase ends, so the bar never lags a
    /// whole phase behind the truth).
    pub fn flush(&self, label: &str) {
        self.redraw(label, true);
    }

    /// One unit finished; `label` names what is being worked on now.
    pub fn inc(&self, label: &str) {
        self.0.done.fetch_add(1, Ordering::Relaxed);
        self.update(label);
    }

    /// One unit failed (still counted as processed).
    pub fn fail(&self, label: &str) {
        self.0.failed.fetch_add(1, Ordering::Relaxed);
        self.inc(label);
    }

    fn update(&self, label: &str) {
        self.redraw(label, false);
    }

    fn redraw(&self, label: &str, force: bool) {
        if !self.0.enabled {
            return;
        }
        if let Ok(mut current) = self.0.label.lock() {
            *current = label.to_string();
        }
        let now = Instant::now();
        if let Ok(mut last) = self.0.drawn_at.lock() {
            // Redraw at most every 100ms, but always keep the label fresh:
            // the next redraw carries the newest one.
            if !force && last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(100)) {
                return;
            }
            *last = Some(now);
        }
        self.draw(true);
    }

    fn draw(&self, carriage: bool) {
        if !self.0.enabled {
            return;
        }
        let done = self.0.done.load(Ordering::Relaxed);
        let total = self.0.total.load(Ordering::Relaxed);
        let failed = self.0.failed.load(Ordering::Relaxed);
        let elapsed = self.0.start.lock().map(|t| t.elapsed()).unwrap_or_default();
        let label = self.0.label.lock().map(|l| l.clone()).unwrap_or_default();
        let line = render(done, total, failed, elapsed, &label);
        self.0.drawn.store(line.chars().count(), Ordering::Relaxed);
        let mut err = std::io::stderr();
        let _ = if carriage {
            write!(err, "\r{line}")
        } else {
            writeln!(err, "{line}")
        };
        let _ = err.flush();
    }

    /// Clear the bar (the caller prints its own summary next).
    pub fn finish(&self) {
        if !self.0.enabled {
            return;
        }
        // One last frame so the final count is visible before the summary.
        if self.0.done.load(Ordering::Relaxed) > 0 {
            self.redraw("", true);
        }
        let width = self.0.drawn.load(Ordering::Relaxed).max(1);
        let mut err = std::io::stderr();
        let _ = write!(err, "\r{}\r", " ".repeat(width));
        let _ = err.flush();
    }
}

/// One line of bar + counters. Kept pure so it can be asserted in tests.
fn render(done: usize, total: usize, failed: usize, elapsed: Duration, label: &str) -> String {
    let total = total.max(done).max(1);
    let filled = (done * WIDTH) / total;
    let bar: String = (0..WIDTH)
        .map(|i| if i < filled { '█' } else { '░' })
        .collect();
    let elapsed = if elapsed.as_secs() >= 60 {
        format!("{}m{:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{}s", elapsed.as_secs())
    };
    let failures = if failed > 0 {
        format!(" · {failed} failed")
    } else {
        String::new()
    };
    let label = if label.is_empty() {
        String::new()
    } else {
        format!(" · {label}")
    };
    format!("[{bar}] {done}/{total} {elapsed}{failures}{label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_reflects_progress_without_overflow() {
        let empty = render(0, 10, 0, Duration::from_secs(2), "");
        assert!(
            empty.starts_with("[░░░░░░░░░░░░░░░░░░░░░░░░] 0/10 2s"),
            "{empty}"
        );
        let half = render(5, 10, 0, Duration::from_secs(65), "opencode ses_1");
        assert!(
            half.starts_with("[████████████░░░░░░░░░░░░] 5/10 1m05s"),
            "{half}"
        );
        assert!(half.ends_with(" · opencode ses_1"));
        let full = render(10, 10, 2, Duration::from_secs(1), "x");
        assert!(full.contains("10/10"), "{full}");
        assert!(full.contains("2 failed"), "{full}");
        // an unknown total (0) must not divide by zero
        let unknown = render(3, 0, 0, Duration::from_secs(1), "");
        assert!(
            unknown.starts_with("[████████████████████████] 3/3"),
            "{unknown}"
        );
    }
}
