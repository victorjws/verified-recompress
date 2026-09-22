//! Feedback for calls that block for minutes at a time.
//!
//! `scan` and `cleanup` each hand rclone a single request covering the whole
//! remote, and until it returns there is nothing to print. Without a counter
//! there is no way to tell a slow call from a hung one, which is the only
//! question worth answering while waiting.
//!
//! [`Activity`] draws a spinner alongside whatever counter the caller can
//! supply. When stderr is not a terminal — a pipe, a redirect, CI — indicatif
//! draws nothing at all, so the same updates go out as occasional log lines
//! instead.

use std::borrow::Cow;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Redraw rate for the spinner itself. Independent of how often the caller has
/// a new counter to report, so the spinner keeps moving between updates.
const TICK: Duration = Duration::from_millis(100);

/// How often to log when there is no terminal to draw on. Often enough to show
/// the run is alive, rare enough not to bury the real output in a CI log.
const LOG_EVERY: Duration = Duration::from_secs(15);

/// A long-running step, shown as a spinner with a counter and elapsed time.
pub struct Activity {
    bar: ProgressBar,
    /// Only consulted on the non-terminal path.
    last_log: Mutex<Instant>,
}

impl Activity {
    /// Announces the step and starts the spinner under it.
    ///
    /// The label is logged rather than drawn so it survives [`Activity::finish`]
    /// clearing the spinner line, and so a redirected run still records what was
    /// being worked on.
    pub fn start(label: impl Into<Cow<'static, str>>) -> Self {
        let label = label.into();
        tracing::info!("{label}");

        let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr());
        // A malformed template is a bug in this file, not a runtime condition. An
        // unstyled spinner is a better outcome than taking the command down.
        match ProgressStyle::with_template("  {spinner} {msg} · {elapsed_precise}") {
            Ok(style) => bar.set_style(style.tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")),
            Err(e) => tracing::debug!("progress template rejected: {e}"),
        }
        bar.set_message("working");
        bar.enable_steady_tick(TICK);

        Self {
            bar,
            last_log: Mutex::new(Instant::now()),
        }
    }

    /// Replaces the counter shown beside the spinner.
    ///
    /// Takes `&self` so it can be called from a polling closure that the caller
    /// still holds the [`Activity`] across.
    pub fn set(&self, detail: impl Into<Cow<'static, str>>) {
        if self.bar.is_hidden() {
            if self.due_to_log(Instant::now()) {
                tracing::info!("  {}", detail.into());
            }
            return;
        }
        self.bar.set_message(detail);
    }

    /// Clears the spinner line. Call before printing anything else, or the
    /// redraw and the output fight over the same line.
    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }

    /// Whether enough time has passed to log again, stamping `now` if so.
    fn due_to_log(&self, now: Instant) -> bool {
        let mut last = self
            .last_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if now.duration_since(*last) < LOG_EVERY {
            return false;
        }
        *last = now;
        true
    }
}

/// Groups digits so a six-figure counter is readable at a glance.
///
/// Counters here are read while scrolling past, where `84120` and `8412` look
/// alike and `84,120` and `8,412` do not.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1), "1");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(8_412), "8,412");
        assert_eq!(thousands(84_120), "84,120");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(u64::MAX), "18,446,744,073,709,551,615");
    }

    /// The non-terminal path logs on the first update and then stays quiet, which
    /// is what keeps a redirected scan from writing a line per second.
    #[test]
    fn logging_is_throttled() {
        let activity = Activity::start("listing /");
        let start = Instant::now();

        assert!(
            activity.due_to_log(start + LOG_EVERY),
            "the first update after the interval should log"
        );
        assert!(
            !activity.due_to_log(start + LOG_EVERY + Duration::from_secs(1)),
            "a second update one second later should not"
        );
        assert!(
            activity.due_to_log(start + LOG_EVERY + LOG_EVERY),
            "another full interval later it should log again"
        );
    }

    /// Nothing about updating an [`Activity`] may depend on there being a
    /// terminal; under `cargo test` stderr is usually not one.
    #[test]
    fn updates_without_a_terminal() {
        let activity = Activity::start("listing /");
        activity.set("listed 8,412 entries");
        activity.set(format!("listed {} entries", thousands(92_277)));
        activity.finish();
    }
}
