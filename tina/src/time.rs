//! Replay-safe timer helper state.
//!
//! Helpers in this module decide delays around runtime-owned sleep. They do
//! not schedule work, retry operations, read clocks, or own hidden queues.
//! Runtime-aware code still returns an explicit runtime call such as
//! `sleep(delay).then(...)`.
//!
//! All APIs that need a current time take `now` from the caller. Inside an
//! isolate, that should usually be [`Context::now`](crate::Context::now).

use std::time::{Duration, Instant};

use crate::Deadline;

const NANOS_PER_SEC: u128 = 1_000_000_000;
const TIMER_SATURATION_CEILING: Duration = Duration::from_secs(60 * 60 * 24 * 365 * 100);

/// Error returned when timer helper configuration is unsafe or meaningless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerConfigError {
    /// Interval period must be greater than zero.
    ZeroIntervalPeriod,
    /// Backoff delay must be greater than zero.
    ZeroBackoffDelay,
    /// Maximum backoff delay must be at least the initial delay.
    MaxBackoffBelowInitial,
    /// Backoff multiplier must be greater than zero.
    ZeroBackoffMultiplier,
    /// Maximum attempts must be greater than zero.
    ZeroMaxAttempts,
}

/// A timer helper decision that may refuse to sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "handle DeadlineElapsed/Exhausted, or return sleep(decision.delay()).then(...)"]
pub enum TimerDecision<T> {
    /// Sleep for the delay carried by `T`.
    Sleep(T),
    /// The caller-provided deadline has already elapsed at the supplied `now`.
    DeadlineElapsed,
    /// The helper's explicit attempt budget is exhausted.
    Exhausted,
}

impl<T> TimerDecision<T> {
    /// Maps a sleep payload without changing terminal decisions.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> TimerDecision<U> {
        match self {
            Self::Sleep(value) => TimerDecision::Sleep(f(value)),
            Self::DeadlineElapsed => TimerDecision::DeadlineElapsed,
            Self::Exhausted => TimerDecision::Exhausted,
        }
    }
}

/// Delay chosen by [`Backoff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "return sleep(delay()).then(...) or record why retry stopped"]
pub struct BackoffDelay {
    attempt: u64,
    delay: Duration,
    deadline_capped: bool,
}

impl BackoffDelay {
    /// One-based retry attempt number for this delay.
    pub const fn attempt(self) -> u64 {
        self.attempt
    }

    /// Delay to pass to runtime sleep.
    pub const fn delay(self) -> Duration {
        self.delay
    }

    /// Whether the delay was capped by a supplied deadline.
    pub const fn deadline_capped(self) -> bool {
        self.deadline_capped
    }
}

/// Explicit retry/backoff delay state.
///
/// `Backoff` does not retry work. It only returns the delay for a caller-owned
/// retry attempt until the explicit max-attempt budget is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "Backoff is state; store it and call next_delay only when you choose to retry"]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    multiplier: u32,
    max_attempts: u64,
    attempts_started: u64,
    next: Duration,
}

impl Backoff {
    /// Creates a constant backoff with an explicit retry-attempt budget.
    pub fn constant(delay: Duration, max_attempts: u64) -> Result<Self, TimerConfigError> {
        Self::new(delay, delay, 1, max_attempts)
    }

    /// Creates an exponential backoff with multiplier `2` and an explicit
    /// retry-attempt budget.
    pub fn exponential(
        initial: Duration,
        max: Duration,
        max_attempts: u64,
    ) -> Result<Self, TimerConfigError> {
        Self::new(initial, max, 2, max_attempts)
    }

    /// Creates a backoff with an explicit multiplier and retry-attempt budget.
    pub fn with_multiplier(
        initial: Duration,
        max: Duration,
        multiplier: u32,
        max_attempts: u64,
    ) -> Result<Self, TimerConfigError> {
        Self::new(initial, max, multiplier, max_attempts)
    }

    fn new(
        initial: Duration,
        max: Duration,
        multiplier: u32,
        max_attempts: u64,
    ) -> Result<Self, TimerConfigError> {
        if initial.is_zero() || max.is_zero() {
            return Err(TimerConfigError::ZeroBackoffDelay);
        }
        if max < initial {
            return Err(TimerConfigError::MaxBackoffBelowInitial);
        }
        if multiplier == 0 {
            return Err(TimerConfigError::ZeroBackoffMultiplier);
        }
        if max_attempts == 0 {
            return Err(TimerConfigError::ZeroMaxAttempts);
        }
        Ok(Self {
            initial,
            max,
            multiplier,
            max_attempts,
            attempts_started: 0,
            next: initial,
        })
    }

    /// Returns the initial delay.
    pub const fn initial(self) -> Duration {
        self.initial
    }

    /// Returns the maximum delay.
    pub const fn max(self) -> Duration {
        self.max
    }

    /// Returns the multiplier applied after each chosen delay.
    pub const fn multiplier(self) -> u32 {
        self.multiplier
    }

    /// Returns the maximum number of retry delays.
    pub const fn max_attempts(self) -> u64 {
        self.max_attempts
    }

    /// Returns how many retry delays this state has handed out.
    pub const fn attempts_started(self) -> u64 {
        self.attempts_started
    }

    /// Returns the delay that will be chosen next, if attempts remain.
    pub const fn peek(self) -> Option<Duration> {
        if self.attempts_started >= self.max_attempts {
            None
        } else {
            Some(self.next)
        }
    }

    /// Chooses the next retry delay without a deadline cap.
    pub fn next_delay(&mut self) -> TimerDecision<BackoffDelay> {
        if self.attempts_started >= self.max_attempts {
            return TimerDecision::Exhausted;
        }

        self.attempts_started = self.attempts_started.saturating_add(1);
        let attempt = self.attempts_started;
        let delay = self.next;
        self.next = duration_mul_saturating(self.next, u128::from(self.multiplier)).min(self.max);
        TimerDecision::Sleep(BackoffDelay {
            attempt,
            delay,
            deadline_capped: false,
        })
    }

    /// Chooses the next retry delay, optionally capped by a deadline.
    ///
    /// The `now` argument is only used when a deadline is supplied. It is still
    /// explicit so deadline math stays anchored to runtime/simulator time.
    pub fn next_delay_until(
        &mut self,
        now: Instant,
        deadline: Option<Deadline>,
    ) -> TimerDecision<BackoffDelay> {
        if self.attempts_started >= self.max_attempts {
            return TimerDecision::Exhausted;
        }

        let mut delay = self.next;
        let mut deadline_capped = false;
        if let Some(raw_deadline) = deadline {
            let remaining = raw_deadline.remaining_or_zero(now);
            if remaining.is_zero() {
                return TimerDecision::DeadlineElapsed;
            }
            if delay > remaining {
                delay = remaining;
                deadline_capped = true;
            }
        }

        self.attempts_started = self.attempts_started.saturating_add(1);
        let attempt = self.attempts_started;
        self.next = duration_mul_saturating(self.next, u128::from(self.multiplier)).min(self.max);
        TimerDecision::Sleep(BackoffDelay {
            attempt,
            delay,
            deadline_capped,
        })
    }

    /// Restarts the backoff sequence at the initial delay.
    pub fn reset(&mut self) {
        self.attempts_started = 0;
        self.next = self.initial;
    }
}

fn checked_add_or_max(now: Instant, after: Duration) -> Instant {
    now.checked_add(after)
        .unwrap_or_else(|| now.checked_add(TIMER_SATURATION_CEILING).unwrap_or(now))
}

fn elapsed_periods_saturating(overdue: Duration, period: Duration) -> u64 {
    overdue
        .as_nanos()
        .checked_div(period.as_nanos())
        .and_then(|periods| periods.checked_add(1))
        .and_then(|periods| u64::try_from(periods).ok())
        .unwrap_or(u64::MAX)
}

/// What [`RecurringTick`] does when the caller is overdue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecurringCatchUp {
    /// Coalesce all missed ticks into one visible [`RecurringTickDecision::Skip`]
    /// report. The next `next(now)` call returns a fresh future
    /// [`Sleep`](RecurringTickDecision::Sleep).
    Skip,
    /// Allow at most `n` immediate catch-up ticks. Each one resolves as a
    /// [`Sleep`](RecurringTickDecision::Sleep) with a zero delay (fire-now)
    /// and consumes one catch-up budget. Ticks beyond the budget are dropped
    /// and counted in the first catch-up `Sleep`'s `missed_ticks`; this skips
    /// tick numbers for the dropped portion of the burst. Queued catch-up fires
    /// report `missed_ticks = 1` and advance tick numbers by two per fire.
    Bounded(u32),
    /// Drift forward: schedule the next tick `period` after the observed
    /// `now`. No catch-up; missed ticks become zero.
    Delay,
}

/// Opaque token issued for each scheduled [`Sleep`](RecurringTickDecision::Sleep)
/// decision. Carry it inside the continuation message and call
/// [`RecurringTick::validate`] when the message arrives to distinguish a
/// live tick from a stale one whose schedule has already been rearmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "store the token on the continuation and call validate(token) when it arrives"]
pub struct RecurringTickToken {
    ordinal: u64,
}

impl RecurringTickToken {
    /// Strictly increasing ordinal of this token across the [`RecurringTick`]'s
    /// lifetime. Useful for trace-visible reports; not used for liveness logic.
    pub const fn ordinal(self) -> u64 {
        self.ordinal
    }
}

/// Description of a single [`RecurringTick`] decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecurringTickReport {
    /// One-based tick number for the tick this decision schedules or fires.
    pub tick_number: u64,
    /// Time slot the tick was scheduled against (before any catch-up).
    pub scheduled_at: Instant,
    /// `now` the caller supplied to produce this decision.
    pub observed_at: Instant,
    /// Number of missed periods coalesced into this decision. Zero when on
    /// time. Non-zero on [`RecurringTickDecision::Skip`], on the first bounded
    /// catch-up `Sleep` that reports dropped overflow, and on queued bounded
    /// catch-ups that each represent one overdue tick.
    pub missed_ticks: u64,
    /// How many bounded catch-up ticks remain on this run.
    pub catch_up_remaining: u32,
}

/// A [`RecurringTick`] decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "handle both Sleep and Skip; never re-enter without using the report"]
pub enum RecurringTickDecision {
    /// Sleep `delay` and then deliver a continuation message carrying `token`.
    /// When the continuation arrives, validate it via [`RecurringTick::validate`].
    Sleep {
        /// Duration to pass to runtime sleep.
        delay: Duration,
        /// Token to embed in the continuation message.
        token: RecurringTickToken,
        /// Report describing the scheduled tick.
        report: RecurringTickReport,
    },
    /// The caller is overdue. The configured policy coalesced missed ticks
    /// into this visible report. The caller records the report and calls
    /// [`RecurringTick::next`] again to obtain a fresh future `Sleep`.
    Skip(RecurringTickReport),
}

/// Error returned by [`RecurringTick::validate`] when a continuation token
/// does not match the current expected ordinal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecurringTickStale {
    /// The carried token was issued before the current schedule.
    StaleOrdinal {
        /// The token the continuation carried.
        token_ordinal: u64,
        /// The ordinal the helper currently expects.
        expected_ordinal: u64,
    },
    /// The helper has been cleared since this token was issued and no
    /// schedule is currently armed.
    Cleared {
        /// The ordinal carried by the stale continuation token.
        token_ordinal: u64,
    },
}

/// Recurring service-tick helper.
///
/// `RecurringTick` only computes decisions. The caller
/// owns the continuation message and the sleep effect. Each `Sleep` decision
/// carries a [`RecurringTickToken`]; the continuation handler must call
/// [`validate`](Self::validate) to detect stale ticks rearmed under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "RecurringTick is state; store it and call next(ctx.now()) when you want to (re)arm"]
pub struct RecurringTick {
    period: Duration,
    catch_up: RecurringCatchUp,
    next_due: Option<Instant>,
    next_tick_number: u64,
    next_ordinal: u64,
    /// Currently-armed Sleep ordinal, if any. Cleared by [`Self::clear`] and
    /// during a [`Skip`](RecurringTickDecision::Skip) decision.
    armed_ordinal: Option<u64>,
    /// Number of immediate catch-up ticks still owed for the current overdue
    /// burst. Each [`Bounded`](RecurringCatchUp::Bounded) overdue observation
    /// loads this; each call drains one with `delay = 0`.
    catch_up_pending: u32,
}

impl RecurringTick {
    /// Creates a recurring tick state with period `period` and the default
    /// catch-up policy [`RecurringCatchUp::Delay`].
    pub fn every(period: Duration) -> Result<Self, TimerConfigError> {
        if period.is_zero() {
            return Err(TimerConfigError::ZeroIntervalPeriod);
        }
        Ok(Self {
            period,
            catch_up: RecurringCatchUp::Delay,
            next_due: None,
            next_tick_number: 1,
            next_ordinal: 1,
            armed_ordinal: None,
            catch_up_pending: 0,
        })
    }

    /// Picks the catch-up policy for overdue ticks.
    pub const fn catch_up(mut self, policy: RecurringCatchUp) -> Self {
        self.catch_up = policy;
        self
    }

    /// Returns the configured catch-up policy.
    pub const fn catch_up_policy(self) -> RecurringCatchUp {
        self.catch_up
    }

    /// Returns the interval period.
    pub const fn period(self) -> Duration {
        self.period
    }

    /// Returns the next scheduled instant, if a `Sleep` is currently armed.
    pub const fn next_due(self) -> Option<Instant> {
        self.next_due
    }

    /// Clears the schedule. Any continuation message still in flight is now
    /// stale and will fail [`validate`](Self::validate).
    pub fn clear(&mut self) {
        self.next_due = None;
        self.armed_ordinal = None;
        self.catch_up_pending = 0;
    }

    /// Validates a continuation token against the currently armed ordinal.
    pub fn validate(
        &self,
        token: RecurringTickToken,
    ) -> Result<RecurringTickToken, RecurringTickStale> {
        match self.armed_ordinal {
            Some(expected) if expected == token.ordinal => Ok(token),
            Some(expected) => Err(RecurringTickStale::StaleOrdinal {
                token_ordinal: token.ordinal,
                expected_ordinal: expected,
            }),
            None => Err(RecurringTickStale::Cleared {
                token_ordinal: token.ordinal,
            }),
        }
    }

    fn arm_sleep(
        &mut self,
        delay: Duration,
        scheduled_at: Instant,
        observed_at: Instant,
        missed_ticks: u64,
    ) -> RecurringTickDecision {
        let ordinal = self.next_ordinal;
        self.next_ordinal = ordinal.saturating_add(1);
        self.armed_ordinal = Some(ordinal);
        let tick_number = self.next_tick_number.saturating_add(missed_ticks);
        self.next_tick_number = tick_number.saturating_add(1);
        let report = RecurringTickReport {
            tick_number,
            scheduled_at,
            observed_at,
            missed_ticks,
            catch_up_remaining: self.catch_up_pending,
        };
        RecurringTickDecision::Sleep {
            delay,
            token: RecurringTickToken { ordinal },
            report,
        }
    }

    /// Returns the next decision for the caller-supplied `now`.
    pub fn next(&mut self, now: Instant) -> RecurringTickDecision {
        // First: if a bounded burst has buffered catch-up ticks, drain one.
        // The interval's `next_due` stays parked at the future slot; the
        // helper hands out `Sleep(0)` with fresh tokens until the burst is
        // drained.
        if self.catch_up_pending > 0 {
            self.catch_up_pending -= 1;
            let scheduled_at = self.next_due.unwrap_or(now);
            return self.arm_sleep(Duration::ZERO, scheduled_at, now, 1);
        }

        // No catch-up pending: arm or advance the schedule against now.
        let scheduled_at = match self.next_due {
            Some(scheduled_at) => scheduled_at,
            None => {
                let scheduled_at = checked_add_or_max(now, self.period);
                self.next_due = Some(scheduled_at);
                scheduled_at
            }
        };

        if scheduled_at > now {
            let delay = scheduled_at.duration_since(now);
            return self.arm_sleep(delay, scheduled_at, now, 0);
        }

        // Overdue: compute how many periods have elapsed since scheduled_at.
        let overdue = now.saturating_duration_since(scheduled_at);
        let elapsed_periods = elapsed_periods_saturating(overdue, self.period);

        match self.catch_up {
            RecurringCatchUp::Delay => {
                // Drift forward: next tick is `now + period`.
                let new_scheduled_at = checked_add_or_max(now, self.period);
                self.next_due = Some(new_scheduled_at);
                self.arm_sleep(self.period, new_scheduled_at, now, 0)
            }
            RecurringCatchUp::Skip => {
                let missed = elapsed_periods.saturating_sub(1);
                if missed == 0 {
                    // Slightly late but no full period was missed. Surfacing a
                    // Skip here would tell the caller to drop the tick; arm a
                    // fresh Sleep from now instead so a recurring service does
                    // not stall after a barely-late observation.
                    let new_scheduled_at = checked_add_or_max(now, self.period);
                    self.next_due = Some(new_scheduled_at);
                    return self.arm_sleep(self.period, new_scheduled_at, now, 0);
                }
                // Coalesce missed periods into one visible Skip. Advance
                // next_due past the overdue burst; the next `next` call arms a
                // fresh Sleep from that future slot.
                let advance = duration_mul_saturating(self.period, u128::from(elapsed_periods));
                let new_scheduled_at = scheduled_at
                    .checked_add(advance)
                    .filter(|slot| *slot > now)
                    .unwrap_or_else(|| checked_add_or_max(now, self.period));
                self.next_due = Some(new_scheduled_at);
                let tick_number = self.next_tick_number.saturating_add(missed);
                self.next_tick_number = tick_number.saturating_add(1);
                // No live ordinal during a Skip — a continuation arriving now
                // is stale by construction.
                self.armed_ordinal = None;
                RecurringTickDecision::Skip(RecurringTickReport {
                    tick_number,
                    scheduled_at,
                    observed_at: now,
                    missed_ticks: missed,
                    catch_up_remaining: 0,
                })
            }
            RecurringCatchUp::Bounded(budget) => {
                // Fire at most `budget` immediate ticks. Anything past the
                // budget is dropped, and the count of dropped ticks is carried
                // visibly on the first Sleep(0) via `missed_ticks`. Queued
                // catch-ups report one missed tick per immediate fire.
                if budget == 0 {
                    let missed = elapsed_periods.saturating_sub(1);
                    let advance = duration_mul_saturating(self.period, u128::from(elapsed_periods));
                    let new_scheduled_at = scheduled_at
                        .checked_add(advance)
                        .filter(|slot| *slot > now)
                        .unwrap_or_else(|| checked_add_or_max(now, self.period));
                    self.next_due = Some(new_scheduled_at);
                    self.armed_ordinal = None;
                    let tick_number = self.next_tick_number.saturating_add(missed);
                    self.next_tick_number = tick_number.saturating_add(1);
                    return RecurringTickDecision::Skip(RecurringTickReport {
                        tick_number,
                        scheduled_at,
                        observed_at: now,
                        missed_ticks: missed,
                        catch_up_remaining: 0,
                    });
                }
                let immediate = u64::from(budget).min(elapsed_periods);
                let skipped = elapsed_periods - immediate;
                // The first immediate tick is fired now; queue the rest.
                self.catch_up_pending =
                    u32::try_from(immediate.saturating_sub(1)).unwrap_or(u32::MAX);
                // Advance the schedule past the whole overdue burst.
                let advance = duration_mul_saturating(self.period, u128::from(elapsed_periods));
                let new_scheduled_at = scheduled_at
                    .checked_add(advance)
                    .filter(|slot| *slot > now)
                    .unwrap_or_else(|| checked_add_or_max(now, self.period));
                self.next_due = Some(new_scheduled_at);
                self.arm_sleep(Duration::ZERO, scheduled_at, now, skipped)
            }
        }
    }
}

fn duration_mul_saturating(duration: Duration, factor: u128) -> Duration {
    let nanos = duration.as_nanos().saturating_mul(factor);
    let secs = nanos / NANOS_PER_SEC;
    let sub_nanos = nanos % NANOS_PER_SEC;
    if secs > u128::from(u64::MAX) {
        return Duration::MAX;
    }
    Duration::new(secs as u64, sub_nanos as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_at_max_delay_and_exhausts() {
        let mut backoff =
            Backoff::exponential(Duration::from_millis(5), Duration::from_millis(20), 4).unwrap();

        let delays: Vec<_> = (0..4)
            .map(|_| match backoff.next_delay() {
                TimerDecision::Sleep(delay) => (delay.attempt(), delay.delay()),
                other => panic!("expected sleep, got {other:?}"),
            })
            .collect();

        assert_eq!(
            delays,
            [
                (1, Duration::from_millis(5)),
                (2, Duration::from_millis(10)),
                (3, Duration::from_millis(20)),
                (4, Duration::from_millis(20)),
            ]
        );
        assert_eq!(backoff.next_delay(), TimerDecision::Exhausted);
    }

    #[test]
    fn backoff_duration_math_saturates_before_max_cap() {
        let mut backoff = Backoff::with_multiplier(
            Duration::from_secs(u64::MAX / 4),
            Duration::MAX,
            u32::MAX,
            2,
        )
        .unwrap();

        match backoff.next_delay() {
            TimerDecision::Sleep(delay) => {
                assert_eq!(delay.delay(), Duration::from_secs(u64::MAX / 4));
            }
            other => panic!("expected sleep, got {other:?}"),
        }
        match backoff.next_delay() {
            TimerDecision::Sleep(delay) => {
                assert_eq!(delay.delay(), Duration::MAX);
            }
            other => panic!("expected saturated sleep, got {other:?}"),
        }
    }

    #[test]
    fn backoff_deadline_caps_delay_or_reports_elapsed() {
        let now = Instant::now();
        let deadline = Deadline::from_instant(now, Duration::from_millis(3));
        let mut backoff = Backoff::constant(Duration::from_millis(10), 1).unwrap();

        match backoff.next_delay_until(now, Some(deadline)) {
            TimerDecision::Sleep(delay) => {
                assert_eq!(delay.delay(), Duration::from_millis(3));
                assert!(delay.deadline_capped());
            }
            other => panic!("expected capped sleep, got {other:?}"),
        }

        let mut backoff = Backoff::constant(Duration::from_millis(10), 1).unwrap();
        assert_eq!(
            backoff.next_delay_until(now + Duration::from_millis(3), Some(deadline)),
            TimerDecision::DeadlineElapsed
        );
    }

    #[test]
    fn recurring_tick_first_sleep_arms_token_one() {
        let now = Instant::now();
        let mut tick = RecurringTick::every(Duration::from_millis(10)).unwrap();

        match tick.next(now) {
            RecurringTickDecision::Sleep {
                delay,
                token,
                report,
            } => {
                assert_eq!(delay, Duration::from_millis(10));
                assert_eq!(token.ordinal(), 1);
                assert_eq!(report.tick_number, 1);
                assert_eq!(report.missed_ticks, 0);
                assert_eq!(report.scheduled_at, now + Duration::from_millis(10));
                assert!(tick.validate(token).is_ok());
            }
            other => panic!("expected sleep, got {other:?}"),
        }
    }

    #[test]
    fn recurring_tick_token_detects_stale_after_rearm() {
        let now = Instant::now();
        let mut tick = RecurringTick::every(Duration::from_millis(10)).unwrap();

        let first_token = match tick.next(now) {
            RecurringTickDecision::Sleep { token, .. } => token,
            other => panic!("expected first sleep, got {other:?}"),
        };

        // External event (e.g. a size-triggered flush) clears the schedule and
        // arms a fresh tick. The first token must now be stale.
        tick.clear();
        match tick.validate(first_token) {
            Err(RecurringTickStale::Cleared { token_ordinal }) => {
                assert_eq!(token_ordinal, 1);
            }
            other => panic!("expected Cleared after clear, got {other:?}"),
        }

        let second_token = match tick.next(now) {
            RecurringTickDecision::Sleep { token, .. } => token,
            other => panic!("expected second sleep, got {other:?}"),
        };
        assert_eq!(second_token.ordinal(), 2);
        assert!(tick.validate(second_token).is_ok());
        assert!(matches!(
            tick.validate(first_token),
            Err(RecurringTickStale::StaleOrdinal {
                token_ordinal: 1,
                expected_ordinal: 2,
            })
        ));
    }

    #[test]
    fn recurring_tick_skip_policy_coalesces_missed_into_one_skip() {
        let now = Instant::now();
        let mut tick = RecurringTick::every(Duration::from_millis(10))
            .unwrap()
            .catch_up(RecurringCatchUp::Skip);
        assert!(matches!(
            tick.next(now),
            RecurringTickDecision::Sleep { .. }
        ));

        // Caller observes time 35ms later — three periods elapsed.
        let late = now + Duration::from_millis(35);
        match tick.next(late) {
            RecurringTickDecision::Skip(report) => {
                assert!(report.missed_ticks >= 2);
                assert_eq!(report.catch_up_remaining, 0);
            }
            other => panic!("expected Skip, got {other:?}"),
        }

        // Next call returns a fresh future Sleep, never loops on missed ticks.
        match tick.next(late) {
            RecurringTickDecision::Sleep { delay, token, .. } => {
                assert!(delay > Duration::ZERO);
                assert!(tick.validate(token).is_ok());
            }
            other => panic!("expected Sleep after Skip, got {other:?}"),
        }
    }

    #[test]
    fn recurring_tick_bounded_catch_up_caps_immediate_ticks() {
        let now = Instant::now();
        let mut tick = RecurringTick::every(Duration::from_millis(10))
            .unwrap()
            .catch_up(RecurringCatchUp::Bounded(2));
        assert!(matches!(
            tick.next(now),
            RecurringTickDecision::Sleep { .. }
        ));

        // Five periods late: bounded(2) fires two immediate ticks with the
        // dropped ones visibly recorded on the first Sleep, then returns to
        // ordinary future scheduling.
        let late = now + Duration::from_millis(55);

        let (first_delay, first_missed) = match tick.next(late) {
            RecurringTickDecision::Sleep {
                delay,
                report,
                token,
            } => {
                assert!(tick.validate(token).is_ok());
                (delay, report.missed_ticks)
            }
            other => panic!("expected bounded Sleep, got {other:?}"),
        };
        assert_eq!(first_delay, Duration::ZERO);
        assert!(
            first_missed >= 3,
            "skipped count must surface on the first catch-up sleep (got {first_missed})"
        );

        let (second_delay, second_missed) = match tick.next(late) {
            RecurringTickDecision::Sleep {
                delay,
                report,
                token,
            } => {
                assert!(tick.validate(token).is_ok());
                (delay, report.missed_ticks)
            }
            other => panic!("expected second bounded Sleep, got {other:?}"),
        };
        assert_eq!(second_delay, Duration::ZERO);
        assert_eq!(
            second_missed, 1,
            "queued catch-up reports one tick per fire, not the dropped burst"
        );

        // After the bounded budget drains, the next decision is a regular
        // future Sleep — never loop forever.
        match tick.next(late) {
            RecurringTickDecision::Sleep { delay, token, .. } => {
                assert!(delay > Duration::ZERO);
                assert!(tick.validate(token).is_ok());
            }
            other => panic!("expected future Sleep after bounded burst, got {other:?}"),
        }
    }

    #[test]
    fn recurring_tick_bounded_zero_skips_without_immediate_sleep() {
        let now = Instant::now();
        let mut tick = RecurringTick::every(Duration::from_millis(10))
            .unwrap()
            .catch_up(RecurringCatchUp::Bounded(0));
        assert!(matches!(
            tick.next(now),
            RecurringTickDecision::Sleep { .. }
        ));

        let late = now + Duration::from_millis(35);
        match tick.next(late) {
            RecurringTickDecision::Skip(report) => {
                assert!(
                    report.missed_ticks >= 2,
                    "zero-budget bounded catch-up should report missed ticks like Skip"
                );
                assert_eq!(report.catch_up_remaining, 0);
            }
            other => panic!("expected Skip for zero catch-up budget, got {other:?}"),
        }

        match tick.next(late) {
            RecurringTickDecision::Sleep { delay, token, .. } => {
                assert!(delay > Duration::ZERO);
                assert!(tick.validate(token).is_ok());
            }
            other => panic!("expected future Sleep after zero-budget Skip, got {other:?}"),
        }
    }

    #[test]
    fn recurring_tick_elapsed_periods_saturates_instead_of_truncating() {
        assert_eq!(
            elapsed_periods_saturating(Duration::MAX, Duration::from_nanos(1)),
            u64::MAX
        );
    }

    #[test]
    fn recurring_tick_skip_policy_returns_sleep_when_overdue_by_less_than_one_period() {
        // A barely-overdue arm under Skip policy must not surface Skip(missed=0)
        // — that would cause caller loops that treat Skip as "drop the tick" to
        // stall. The helper arms a fresh Sleep from now instead.
        let now = Instant::now();
        let mut tick = RecurringTick::every(Duration::from_millis(10))
            .unwrap()
            .catch_up(RecurringCatchUp::Skip);
        assert!(matches!(
            tick.next(now),
            RecurringTickDecision::Sleep { .. }
        ));

        // Observe 11ms later: scheduled_at was now+10ms, so we are 1ms past
        // the scheduled slot with no full period missed.
        let slightly_late = now + Duration::from_millis(11);
        match tick.next(slightly_late) {
            RecurringTickDecision::Sleep { delay, token, .. } => {
                assert_eq!(delay, Duration::from_millis(10));
                assert!(tick.validate(token).is_ok());
            }
            other => panic!("expected Sleep when overdue by < 1 period, got {other:?}"),
        }
    }

    #[test]
    fn recurring_tick_delay_policy_drifts_no_catch_up() {
        let now = Instant::now();
        let mut tick = RecurringTick::every(Duration::from_millis(10))
            .unwrap()
            .catch_up(RecurringCatchUp::Delay);
        assert!(matches!(
            tick.next(now),
            RecurringTickDecision::Sleep { .. }
        ));

        let late = now + Duration::from_millis(35);
        match tick.next(late) {
            RecurringTickDecision::Sleep {
                delay,
                report,
                token,
            } => {
                assert_eq!(delay, Duration::from_millis(10));
                assert_eq!(report.missed_ticks, 0);
                assert!(tick.validate(token).is_ok());
            }
            other => panic!("expected drifting Sleep, got {other:?}"),
        }
    }

    #[test]
    fn recurring_tick_clear_makes_in_flight_token_stale() {
        let now = Instant::now();
        let mut tick = RecurringTick::every(Duration::from_millis(10)).unwrap();
        let token = match tick.next(now) {
            RecurringTickDecision::Sleep { token, .. } => token,
            other => panic!("expected sleep, got {other:?}"),
        };
        assert!(tick.validate(token).is_ok());

        // Service did a size-triggered flush and cleared the recurring tick;
        // the already-scheduled continuation must report stale rather than
        // double-firing the flush.
        tick.clear();
        assert!(matches!(
            tick.validate(token),
            Err(RecurringTickStale::Cleared { token_ordinal: 1 })
        ));
    }

    #[test]
    fn recurring_tick_rejects_zero_period() {
        assert_eq!(
            RecurringTick::every(Duration::ZERO).err(),
            Some(TimerConfigError::ZeroIntervalPeriod)
        );
    }

    #[test]
    fn backoff_rejects_bad_config() {
        assert_eq!(
            Backoff::constant(Duration::ZERO, 1),
            Err(TimerConfigError::ZeroBackoffDelay)
        );
        assert_eq!(
            Backoff::exponential(Duration::from_millis(10), Duration::from_millis(5), 1),
            Err(TimerConfigError::MaxBackoffBelowInitial)
        );
        assert_eq!(
            Backoff::with_multiplier(Duration::from_millis(1), Duration::from_millis(1), 0, 1),
            Err(TimerConfigError::ZeroBackoffMultiplier)
        );
        assert_eq!(
            Backoff::constant(Duration::from_millis(1), 0),
            Err(TimerConfigError::ZeroMaxAttempts)
        );
    }
}
