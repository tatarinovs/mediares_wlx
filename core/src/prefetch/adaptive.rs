//! Adjust the shared background budget using completed analysis and host latency.
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_millis(750);
const MIN_SAMPLES: usize = 16;
const MIN_CALLS: usize = 12;
const STALE_AFTER: Duration = Duration::from_secs(30);
const LONG_CALL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Window {
    count: usize,
    service: Duration,
    saturated: usize,
    foreground: [u32; 32],
    calls: usize,
    long_calls: usize,
}

impl Window {
    fn record_foreground(&mut self, latency: Duration) {
        self.foreground[self.calls % self.foreground.len()] =
            u32::try_from(latency.as_millis()).unwrap_or(u32::MAX);
        self.calls += 1;
        self.long_calls += usize::from(latency >= LONG_CALL);
    }

    fn metrics(&mut self, elapsed: Duration) -> Metrics {
        let len = self.calls.min(self.foreground.len());
        let rank = (len * 9).div_ceil(10) - 1;
        Metrics {
            rate: self.count as f64 / elapsed.as_secs_f64(),
            service: self.service.as_secs_f64() / self.count as f64,
            host_rate: self.calls as f64 / elapsed.as_secs_f64(),
            p90: *self.foreground[..len].select_nth_unstable(rank).1,
            long_fraction: self.long_calls as f64 / self.calls as f64,
            long_calls: self.long_calls,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Metrics {
    rate: f64,
    service: f64,
    host_rate: f64,
    p90: u32,
    long_fraction: f64,
    long_calls: usize,
}

#[derive(Clone, Copy, Default, Debug)]
enum Phase {
    #[default]
    Baseline,
    Candidate {
        before: Metrics,
    },
    Confirm {
        before: Metrics,
    },
    Verify {
        before: Metrics,
        candidate: Metrics,
        limit: usize,
    },
}

impl Metrics {
    fn permits_latency(self, candidate: Self) -> bool {
        candidate.host_rate >= self.host_rate * 0.95
            && f64::from(candidate.p90) <= f64::from(self.p90) * 1.20 + 10.0
            && (candidate.long_calls < 3 || candidate.long_fraction <= self.long_fraction + 0.04)
    }

    fn permits_growth(self, candidate: Self) -> bool {
        candidate.rate > self.rate * 1.05
            && candidate.service < self.service * 2.5
            && self.permits_latency(candidate)
    }

    fn clear_gain(self, candidate: Self) -> bool {
        candidate.rate >= self.rate * 1.20
            && candidate.host_rate >= self.host_rate * 1.05
            && f64::from(candidate.p90) <= f64::from(self.p90) * 1.05 + 5.0
            && self.permits_growth(candidate)
    }
}

pub(super) struct Controller {
    max: usize,
    fixed: bool,
    limit: usize,
    since: Instant,
    changed_at: Instant,
    idle_at: Option<Instant>,
    window: Window,
    stable: usize,
    phase: Phase,
    settling: bool,
    cooldown: usize,
    down: bool,
    fine_steps: bool,
}

impl Controller {
    pub(super) fn new(max: usize, fixed: bool) -> Self {
        let start = if fixed { max } else { max.min(4) };
        let now = Instant::now();
        Self {
            max,
            fixed,
            limit: start,
            since: now,
            changed_at: now,
            idle_at: None,
            window: Window::default(),
            stable: start,
            phase: Phase::Baseline,
            settling: false,
            cooldown: 0,
            down: false,
            fine_steps: false,
        }
    }

    pub(super) fn limit(&self) -> usize {
        self.limit
    }

    pub(super) fn enabled(&self) -> bool {
        !self.fixed && self.max > 1
    }

    pub(super) fn reset(&mut self) {
        *self = Self::new(self.max, self.fixed);
    }

    pub(super) fn idle(&mut self) {
        if self.enabled() {
            self.idle_at.get_or_insert_with(Instant::now);
        }
    }

    pub(super) fn resume(&mut self, now: Instant) {
        if let Some(idle_at) = self.idle_at.take() {
            let gap = now.saturating_duration_since(idle_at);
            if gap >= STALE_AFTER {
                self.reset();
            } else {
                // Album boundaries and short pauses can span a single sample window.
                self.since = now - idle_at.saturating_duration_since(self.since);
            }
        }
    }

    pub(super) fn interrupt(&mut self, now: Instant) {
        self.limit = self.stable;
        self.phase = Phase::Baseline;
        self.settling = true;
        self.changed_at = now;
        self.clear_window(now);
        self.idle_at = self.enabled().then_some(now);
    }

    fn clear_window(&mut self, now: Instant) {
        self.since = now;
        self.window = Window::default();
    }

    pub(super) fn foreground(&mut self, started: Instant, now: Instant) {
        if !self.settling && started >= self.changed_at {
            self.window.record_foreground(now.duration_since(started));
        }
    }

    pub(super) fn complete(&mut self, started: Instant, now: Instant, active: usize) {
        if self.settling {
            if active <= self.limit {
                self.settling = false;
                self.changed_at = now;
                self.clear_window(now);
            }
            return;
        }
        // Discard samples from tasks launched under an earlier limit. This avoids skipping
        // an entire measurement window every time the pool changes concurrency.
        if !self.enabled() || started < self.changed_at {
            return;
        }
        self.window.count += 1;
        self.window.service += now.duration_since(started);
        self.window.saturated += usize::from(active >= self.limit);
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < WINDOW || self.window.count < MIN_SAMPLES || self.window.calls < MIN_CALLS {
            return;
        }
        let saturated = self.window.saturated * 2 >= self.window.count;
        let metrics = self.window.metrics(elapsed);
        #[cfg(feature = "profile-stats")]
        eprintln!(
            "[prefetch auto] phase={} limit={} stable={} count={} calls={} long={} elapsed={:.3} saturated={} rate={:.1} service_ms={:.1} host_rate={:.1} p90={}",
            self.phase_name(), self.limit, self.stable, self.window.count, self.window.calls,
            self.window.long_calls, elapsed.as_secs_f64(), saturated, metrics.rate,
            metrics.service * 1000.0, metrics.host_rate, metrics.p90
        );
        self.clear_window(now);
        let old_limit = self.limit;
        let measured_gain = match self.phase {
            Phase::Candidate { before } => self.limit > self.stable && before.clear_gain(metrics),
            Phase::Confirm { before } => before.permits_growth(metrics),
            _ => false,
        };
        // Saturation shows that the confirmed budget can be a bottleneck.
        // A higher budget can remove that bottleneck before using every slot.
        let reason = if saturated || measured_gain {
            self.observe(metrics)
        } else {
            self.limit = self.stable;
            self.phase = Phase::Baseline;
            self.cooldown = 1;
            "insufficient-demand"
        };
        // Consecutive windows at the same budget can share in-flight work.
        // Budget changes and draining establish a new measurement boundary.
        if old_limit != self.limit {
            self.changed_at = now;
        }
        self.settling = self.limit < old_limit;
        #[cfg(feature = "profile-stats")]
        eprintln!(
            "[prefetch auto] decision={reason} {old_limit} -> {} stable={}",
            self.limit, self.stable
        );
        #[cfg(not(feature = "profile-stats"))]
        let _ = reason;
    }

    #[cfg(feature = "profile-stats")]
    fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Baseline => "A1",
            Phase::Candidate { .. } => "B",
            Phase::Confirm { .. } => "B2",
            Phase::Verify { .. } => "A2",
        }
    }

    fn observe(&mut self, metrics: Metrics) -> &'static str {
        match std::mem::take(&mut self.phase) {
            Phase::Candidate { before }
                if self.limit > self.stable && before.clear_gain(metrics) =>
            {
                self.phase = Phase::Confirm { before };
                "keep-candidate"
            }
            Phase::Confirm { before } if before.permits_growth(metrics) => {
                self.stable = self.limit;
                "accepted-clear-gain"
            }
            Phase::Candidate { before } | Phase::Confirm { before } => {
                self.phase = Phase::Verify {
                    before,
                    candidate: metrics,
                    limit: self.limit,
                };
                self.limit = self.stable;
                "verify-baseline"
            }
            Phase::Verify {
                before,
                candidate,
                limit,
            } => {
                let comparable = before.rate.max(metrics.rate)
                    <= before.rate.min(metrics.rate) * 1.25
                    && f64::from(before.p90.max(metrics.p90))
                        <= f64::from(before.p90.min(metrics.p90)) * 1.35 + 40.0;
                if !comparable {
                    self.cooldown = 1;
                    return "conditions-changed";
                }
                let base = if before.rate > metrics.rate {
                    before
                } else {
                    metrics
                };
                let lower = limit < self.stable;
                let responsive =
                    before.permits_latency(candidate) && metrics.permits_latency(candidate);
                let accept = responsive
                    && if lower {
                        candidate.rate >= base.rate * 0.95
                    } else {
                        base.permits_growth(candidate)
                    };
                if accept {
                    self.limit = limit;
                    self.stable = limit;
                } else {
                    self.fine_steps |= !lower;
                }
                self.down = if lower { accept } else { !accept };
                self.cooldown = if accept && !lower { 0 } else { 6 };
                if accept {
                    "accepted"
                } else {
                    "rejected"
                }
            }
            Phase::Baseline => {
                if self.cooldown > 0 {
                    self.cooldown -= 1;
                    return "cooldown";
                }
                let next = if self.down {
                    self.limit.saturating_sub(1).max(1)
                } else {
                    let step = if self.fine_steps {
                        1
                    } else {
                        self.limit.min(4)
                    };
                    (self.limit + step).min(self.max)
                };
                if next == self.limit {
                    self.down = !self.down;
                    return "boundary";
                }
                self.phase = Phase::Candidate { before: metrics };
                self.limit = next;
                "probe"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller_starting_at_two(max: usize) -> Controller {
        let mut controller = Controller::new(max, false);
        controller.stable = 2;
        controller.limit = 2;
        controller
    }

    fn metrics(rate: f64, p90: u32, long_fraction: f64) -> Metrics {
        Metrics {
            rate,
            service: 0.1,
            host_rate: rate,
            p90,
            long_fraction,
            long_calls: (long_fraction * 32.0).round() as usize,
        }
    }

    #[test]
    fn grows_until_host_latency_regresses() {
        let mut c = controller_starting_at_two(15);
        for (rate, p90, limit) in [
            (20., 90, 4),
            (38., 100, 2),
            (20., 90, 4),
            (38., 100, 8),
            (70., 300, 4),
            (38., 100, 4),
        ] {
            c.observe(metrics(rate, p90, 0.0));
            assert_eq!(c.limit(), limit);
        }
        assert!(!metrics(40., 80, 0.0).permits_growth(metrics(50., 80, 0.10)));
        let mut trial = metrics(60., 80, 0.0);
        trial.host_rate = 30.;
        assert!(!metrics(40., 80, 0.0).permits_growth(trial));
    }

    #[test]
    fn waits_for_host_latency_samples_before_raising_budget() {
        let mut c = controller_starting_at_two(8);
        let start = c.changed_at;
        let end = start + WINDOW;
        for _ in 0..MIN_SAMPLES {
            c.complete(start, end, 2);
        }
        assert_eq!(c.limit(), 2);
        for _ in 0..MIN_CALLS {
            c.foreground(start, end);
        }
        c.complete(start, end, 2);
        assert_eq!(c.limit(), 4);
    }

    #[test]
    fn keeps_probe_on_short_pause_but_discards_it_on_stop_or_long_pause() {
        let mut c = controller_starting_at_two(8);
        c.observe(metrics(20., 90, 0.0));
        c.window.count = 5;
        c.idle();
        c.resume(c.idle_at.unwrap() + Duration::from_millis(100));
        assert_eq!(c.window.count, 5);
        assert_eq!(c.limit(), 4);
        c.idle();
        let stopped = c.idle_at.unwrap() + Duration::from_millis(20);
        c.interrupt(stopped);
        assert_eq!(c.idle_at, Some(stopped));
        assert_eq!(c.limit(), 2);
        assert_eq!(c.window.count, 0);
        c.observe(metrics(20., 90, 0.0));
        c.resume(c.idle_at.unwrap() + STALE_AFTER);
        assert_eq!(c.limit(), 4);
    }

    #[test]
    fn fixed_and_starved_work_do_not_tune() {
        for fixed in [false, true] {
            let mut c = Controller::new(8, fixed);
            let initial = c.limit();
            let start = c.since;
            for i in 1..200 {
                let end = start + Duration::from_millis(i * 100);
                c.complete(end - Duration::from_millis(10), end, 1);
            }
            assert_eq!(c.limit(), initial);
        }
    }

    #[test]
    fn old_budget_completions_do_not_enter_new_window() {
        let mut c = controller_starting_at_two(8);
        let started = c.changed_at;
        c.changed_at += Duration::from_millis(50);
        c.complete(started, c.changed_at, 2);
        c.foreground(started, c.changed_at);
        assert_eq!(c.window.count, 0);
        assert_eq!(c.window.calls, 0);
    }

    #[test]
    fn changing_conditions_do_not_block_subsequent_growth() {
        let mut c = controller_starting_at_two(8);
        c.observe(metrics(305., 80, 0.0));
        c.observe(metrics(30., 80, 0.0));
        assert_eq!(c.observe(metrics(19., 80, 0.0)), "conditions-changed");
        assert_eq!(c.stable, 2);
        assert!(!c.fine_steps);
        c.observe(metrics(19., 80, 0.0));
        c.observe(metrics(19., 80, 0.0));
        c.observe(metrics(35., 95, 0.0));
        assert_eq!(c.stable, 2);
        assert_eq!(c.observe(metrics(20., 80, 0.0)), "accepted");
        assert_eq!(c.stable, 4);
    }

    #[test]
    fn clear_gain_keeps_candidate_running() {
        let mut c = controller_starting_at_two(8);
        c.observe(metrics(20., 90, 0.0));
        assert_eq!(c.stable, 2);
        assert_eq!(c.observe(metrics(28., 80, 0.0)), "keep-candidate");
        assert_eq!(c.limit(), 4);
        assert_eq!(c.stable, 2);
        assert_eq!(c.observe(metrics(26., 82, 0.0)), "accepted-clear-gain");
        assert_eq!(c.stable, 4);
        assert!(matches!(c.phase, Phase::Baseline));
        let before = metrics(14.2, 217, 0.0);
        let mut candidate = metrics(20.4, 231, 0.0);
        candidate.host_rate = 15.3;
        assert!(before.clear_gain(candidate));
        candidate.p90 = 240;
        assert!(!before.clear_gain(candidate));
    }

    #[test]
    fn a_gain_does_not_require_filling_every_new_slot() {
        let mut c = controller_starting_at_two(8);
        c.observe(metrics(4., 300, 0.0));
        for _ in 0..2 {
            let start = c.since;
            let end = start + WINDOW;
            for _ in 0..MIN_CALLS {
                c.foreground(end - Duration::from_millis(50), end);
            }
            for _ in 0..MIN_SAMPLES {
                c.complete(end - Duration::from_millis(50), end, 3);
            }
            assert_eq!(c.limit(), 4);
        }
        assert_eq!(c.stable, 4);
    }

    #[test]
    fn brief_gain_cannot_confirm_a_slow_candidate() {
        let mut c = controller_starting_at_two(8);
        c.observe(metrics(20., 90, 0.0));
        c.observe(metrics(28., 80, 0.0));
        assert_eq!(c.observe(metrics(25., 160, 0.0)), "verify-baseline");
        assert_eq!(c.limit(), 2);
        assert_eq!(c.observe(metrics(20., 90, 0.0)), "rejected");
    }

    #[test]
    fn lower_budget_is_accepted_on_a_responsive_plateau() {
        let mut c = controller_starting_at_two(8);
        c.down = true;
        c.observe(metrics(20., 90, 0.0));
        assert_eq!(c.limit(), 1);
        c.observe(metrics(20., 90, 0.0));
        assert_eq!(c.limit(), 2);
        assert_eq!(c.observe(metrics(20., 90, 0.0)), "accepted");
        assert_eq!(c.stable, 1);
    }

    #[test]
    fn cancellation_restores_confirmed_budget_in_each_phase() {
        for observations in 0..3 {
            let mut c = controller_starting_at_two(8);
            for _ in 0..observations {
                c.observe(metrics(20., 90, 0.0));
            }
            c.interrupt(Instant::now());
            assert_eq!(c.limit(), 2);
            assert!(matches!(c.phase, Phase::Baseline));
        }
    }

    #[test]
    fn verification_waits_for_old_workers_to_drain() {
        let mut c = controller_starting_at_two(8);
        c.settling = true;
        let start = c.changed_at;
        c.foreground(start, start + WINDOW);
        c.complete(start, start + WINDOW, 4);
        assert!(c.settling);
        c.complete(start, start + WINDOW, 2);
        assert!(!c.settling);
        c.complete(start, start + WINDOW, 2);
        assert_eq!(c.window.count, 0);
        assert_eq!(c.window.calls, 0);
    }

    #[test]
    fn long_call_guard_does_not_react_to_one_in_a_large_window() {
        let base = metrics(20., 90, 0.0);
        assert!(base.permits_latency(metrics(30., 90, 1.0 / 32.0)));
        let mut one_slow_call = metrics(30., 90, 1.0 / 12.0);
        one_slow_call.long_calls = 1;
        assert!(base.permits_latency(one_slow_call));
        assert!(!base.permits_latency(metrics(30., 90, 3.0 / 32.0)));
        assert!(!base.permits_latency(metrics(30., 150, 0.0)));
        for max in [0, 1] {
            let c = Controller::new(max, false);
            assert!(!c.enabled());
            assert_eq!(c.limit(), max);
        }
    }
}
