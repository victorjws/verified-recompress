//! Feedback for calls that block for minutes at a time.
//!
//! `scan` and `cleanup` each hand rclone a single request covering the whole
//! remote, and until it returns there is nothing to print. Without a counter
//! there is no way to tell a slow call from a hung one, which is the only
//! question worth answering while waiting.
//!
//! `run` has the same problem in a different shape: a single AV1 encode can
//! take hours, and the pipeline works on several files at once, so [`Board`]
//! keeps a line per file under a summary of the run as a whole.
//!
//! Everything here draws a spinner alongside whatever the caller can report.
//! When stderr is not a terminal — a pipe, a redirect, CI — indicatif draws
//! nothing at all, so the same updates go out as occasional log lines instead.

use std::borrow::Cow;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Redraw rate for the spinner itself. Independent of how often the caller has
/// a new counter to report, so the spinner keeps moving between updates.
const TICK: Duration = Duration::from_millis(100);

/// How often to log when there is no terminal to draw on. Often enough to show
/// the run is alive, rare enough not to bury the real output in a CI log.
const LOG_EVERY: Duration = Duration::from_secs(15);

/// Braille spinner frames. The trailing space is the finished state.
const TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";

/// Whatever is currently drawing, if anything.
///
/// A global because the log writer has to find it from anywhere: a line written
/// mid-redraw would otherwise land on top of the display, which is exactly what
/// happens under `-vv` when the poll loop traces every request.
static ACTIVE: Mutex<Option<Drawing>> = Mutex::new(None);

enum Drawing {
    One(ProgressBar),
    Many(MultiProgress),
}

/// Runs `f` with anything on screen cleared, restoring it afterwards.
///
/// Falls through to running `f` directly if the registry is busy. Losing the
/// clear costs one smudged line; blocking on it could deadlock the logger.
pub fn suspend<R>(f: impl FnOnce() -> R) -> R {
    match ACTIVE.try_lock() {
        Ok(guard) => match guard.as_ref() {
            Some(Drawing::One(bar)) => bar.suspend(f),
            Some(Drawing::Many(multi)) => multi.suspend(f),
            None => f(),
        },
        Err(_) => f(),
    }
}

fn register(drawing: Option<Drawing>) {
    *ACTIVE.lock().unwrap_or_else(PoisonError::into_inner) = drawing;
}

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
            Ok(style) => bar.set_style(style.tick_chars(TICKS)),
            Err(e) => tracing::debug!("progress template rejected: {e}"),
        }
        bar.set_message("working");
        bar.enable_steady_tick(TICK);

        // After the label is logged, so `start` cannot suspend a bar it is still
        // in the middle of creating.
        register(Some(Drawing::One(bar.clone())));

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
        register(None);
        self.bar.finish_and_clear();
    }

    /// Whether enough time has passed to log again, stamping `now` if so.
    fn due_to_log(&self, now: Instant) -> bool {
        due(&self.last_log, now)
    }
}


/// The run display: one summary line, plus a line per file being worked on.
///
/// Files come and go as the pipeline claims and finishes them, so the lines are
/// owned by the jobs rather than by the board. Dropping a [`Slot`] takes its
/// line away.
pub struct Board {
    multi: MultiProgress,
    summary: ProgressBar,
    last_log: Mutex<Instant>,
}

impl Board {
    pub fn start(total: Option<u64>) -> Self {
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
        let summary = multi.add(ProgressBar::new_spinner());
        if let Ok(style) = ProgressStyle::with_template("{msg} · {elapsed_precise}") {
            summary.set_style(style);
        }
        summary.set_message(match total {
            Some(n) => format!("converting 0/{}", thousands(n)),
            None => "converting".to_string(),
        });
        summary.enable_steady_tick(TICK);

        register(Some(Drawing::Many(multi.clone())));
        Self {
            multi,
            summary,
            last_log: Mutex::new(Instant::now()),
        }
    }

    /// Replaces the summary line. The caller owns the wording because only it
    /// knows what the run is counting.
    pub fn summarise(&self, text: impl Into<Cow<'static, str>>) {
        self.summary.set_message(text);
    }

    /// Opens a line for one file. It lives until the returned [`Slot`] is dropped.
    pub fn slot(&self, label: impl Into<String>) -> Slot {
        let bar = self.multi.add(ProgressBar::new_spinner());
        if let Ok(style) = ProgressStyle::with_template("  {spinner} {prefix} {msg}") {
            bar.set_style(style.tick_chars(TICKS));
        }
        bar.set_prefix(label.into());
        bar.enable_steady_tick(TICK);
        Slot {
            bar,
            hidden: self.multi.is_hidden(),
            last_log: Mutex::new(Instant::now()),
        }
    }

    pub fn finish(&self) {
        register(None);
        self.summary.finish_and_clear();
        let _ = self.multi.clear();
    }

    /// Whether enough time has passed to log the summary again.
    pub fn due_to_log(&self, now: Instant) -> bool {
        due(&self.last_log, now)
    }

    pub fn is_hidden(&self) -> bool {
        self.multi.is_hidden()
    }
}

/// One file's line on the [`Board`].
pub struct Slot {
    bar: ProgressBar,
    hidden: bool,
    last_log: Mutex<Instant>,
}

impl Slot {
    /// Names the stage this file is at: `downloading`, `av1 encode 2/3`, and so
    /// on. Stages are worth naming because an AV1 file goes through several, and
    /// "still going" is a different message from "still going, on attempt three".
    pub fn stage(&self, text: impl Into<Cow<'static, str>>) {
        let text = text.into();
        if self.hidden {
            // No terminal, so nothing was drawn. A stage change is rare enough to
            // log every time; it is the within-stage churn that needs throttling.
            tracing::info!("{}: {text}", self.bar.prefix());
            return;
        }
        self.bar.set_message(text);
    }

    /// Reports movement inside the current stage, which arrives far too often to
    /// log every time.
    pub fn detail(&self, text: impl Into<Cow<'static, str>>) {
        if self.hidden {
            if due(&self.last_log, Instant::now()) {
                tracing::info!("{}: {}", self.bar.prefix(), text.into());
            }
            return;
        }
        self.bar.set_message(text);
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}

fn due(last: &Mutex<Instant>, now: Instant) -> bool {
    let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
    if now.duration_since(*last) < LOG_EVERY {
        return false;
    }
    *last = now;
    true
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

    /// `suspend` has to work whether or not anything is on screen, because the
    /// log writer calls it for every line the process ever emits.
    #[test]
    fn suspend_runs_the_write_either_way() {
        assert_eq!(suspend(|| 7), 7, "with no spinner registered");

        let activity = Activity::start("listing /");
        assert_eq!(suspend(|| 7), 7, "with one registered");
        activity.finish();
        assert_eq!(suspend(|| 7), 7, "and after it is cleared");
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
