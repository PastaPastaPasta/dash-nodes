//! Pacing: who is due, how fast to start tests, and which tests may run side by side.
//!
//! A masternode is due again a cycle after its last successful test; after a failure it's retried
//! sooner, after 12 h, 24 h, 48 h ... (up to a cycle). Starts are spread so that a full pass, plus
//! the retries of the masternodes currently failing, fits in 90% of the cycle, jittered ±50%. When
//! a tested masternode is more than a tenth of a cycle overdue (the vantage was down, or retries
//! outgrew the budget), starts come twice as fast until it isn't. Due masternodes go most overdue
//! first, never two in the same /16 (IPv4) or /32 (IPv6) at once.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::probe::BenchResult;
use crate::addr::{Host, PeerAddr};

/// First retry after a failed test; doubles with each further failure, up to the cycle.
const RETRY_BASE_SECS: i64 = 12 * 3600;
/// Retries a failing masternode takes per 14-day cycle (12 h, 24 h, 48 h, 96 h, then 8 days).
const RETRIES_PER_FAILING: usize = 4;
/// Share of the cycle a full pass is spread over.
const PASS_SHARE: f64 = 0.9;
/// Overdue by more than this share of a cycle: catch up.
const LATE_SHARE: f64 = 0.1;

/// When a masternode with `history` (newest first) should be tested next (unix seconds).
pub fn due_at(history: &[BenchResult], cycle_secs: i64) -> i64 {
    let Some(last) = history.first() else { return 0 };
    if last.ok {
        return last.ts + cycle_secs;
    }
    let fails = history.iter().take_while(|r| !r.ok).count() as u32;
    let backoff = RETRY_BASE_SECS.saturating_mul(1 << (fails - 1).min(20));
    last.ts + backoff.min(cycle_secs)
}

/// Whether a masternode has a result from the current cycle (what a sweep skips and the summary's
/// `tested` counts).
pub fn tested_since(history: &[BenchResult], since: i64) -> bool {
    history.first().is_some_and(|r| r.ts >= since)
}

/// Whether a masternode tested before is far enough overdue that the pace should pick up.
pub fn is_late(history: &[BenchResult], cycle_secs: i64, now: i64) -> bool {
    !history.is_empty() && due_at(history, cycle_secs) < now - (cycle_secs as f64 * LATE_SHARE) as i64
}

/// Pause between test starts: `targets` masternodes, `failing` of them due for retries.
pub fn interval(cycle_secs: i64, targets: usize, failing: usize, catching_up: bool) -> Duration {
    let demand = (targets + failing * RETRIES_PER_FAILING).max(1);
    let d = Duration::from_secs_f64(cycle_secs as f64 * PASS_SHARE / demand as f64);
    if catching_up { d / 2 } else { d }
}

pub fn jittered(d: Duration) -> Duration {
    d.mul_f64(rand::random_range(0.5..1.5))
}

/// Tests that share a key don't run at the same time: the /16 of an IPv4 address, the /32 of an
/// IPv6 (or CJDNS) address, and the host itself for onion and I2P.
pub fn subnet_key(addr: &PeerAddr) -> String {
    match addr.host {
        Host::V4(ip) => {
            let o = ip.octets();
            format!("{}.{}/16", o[0], o[1])
        }
        Host::V6(ip) | Host::Cjdns(ip) => {
            let s = ip.segments();
            format!("{:x}:{:x}::/32", s[0], s[1])
        }
        Host::Onion(_) | Host::I2p(_) => addr.host_string(),
    }
}

/// Download rate over the last few seconds, from a running byte counter.
pub struct RateWindow {
    samples: VecDeque<(Instant, u64)>,
    span: Duration,
}

impl RateWindow {
    pub fn new(span: Duration) -> RateWindow {
        RateWindow { samples: VecDeque::new(), span }
    }

    /// Records the counter's value now and returns the rate in bits per second.
    pub fn sample(&mut self, now: Instant, total_bytes: u64) -> f64 {
        self.samples.push_back((now, total_bytes));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > self.span {
            self.samples.pop_front();
        }
        let (t0, b0) = self.samples[0];
        let secs = now.duration_since(t0).as_secs_f64();
        if secs < 0.5 { 0.0 } else { (total_bytes - b0) as f64 * 8.0 / secs }
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::tests::result;
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn due_times() {
        let cycle = 14 * DAY;
        assert_eq!(due_at(&[], cycle), 0);
        // Tested fine: due a cycle later.
        assert_eq!(due_at(&[result(1000, true), result(10, false)], cycle), 1000 + cycle);
        // Failures back off 12 h, 24 h, 48 h, ... up to the cycle.
        assert_eq!(due_at(&[result(1000, false), result(500, true)], cycle), 1000 + 12 * 3600);
        assert_eq!(due_at(&[result(1000, false), result(900, false)], cycle), 1000 + DAY);
        let five: Vec<_> = (0..5).map(|i| result(1000 - i, false)).collect();
        assert_eq!(due_at(&five, cycle), 1000 + 8 * DAY);
        let six: Vec<_> = (0..6).map(|i| result(1000 - i, false)).collect();
        assert_eq!(due_at(&six, cycle), 1000 + cycle);
        assert_eq!(due_at(&six, DAY), 1000 + DAY);
    }

    #[test]
    fn lateness() {
        let cycle = 10 * DAY;
        let now = 100 * DAY;
        // Never tested: due, not late (the first pass doesn't speed up).
        assert!(!is_late(&[], cycle, now));
        assert!(!is_late(&[result(now - cycle - DAY / 2, true)], cycle, now));
        assert!(is_late(&[result(now - cycle - 2 * DAY, true)], cycle, now));
    }

    #[test]
    fn current_cycle() {
        assert!(!tested_since(&[], 0));
        assert!(tested_since(&[result(100, false)], 100));
        assert!(!tested_since(&[result(99, true)], 100));
    }

    #[test]
    fn pacing_spreads_a_pass_over_the_cycle() {
        // 2,356 masternodes over 14 days: one start every ~462 s, a pass in 12.6 days.
        let i = interval(14 * DAY, 2356, 0, false);
        assert_eq!(i.as_secs(), 462);
        assert!((i.as_secs_f64() * 2356.0 / DAY as f64 - 12.6).abs() < 0.01);
        assert_eq!(interval(DAY, 0, 0, false), Duration::from_secs_f64(DAY as f64 * 0.9));
        // 31 failing masternodes take 124 retries' worth of starts; catching up halves the pause.
        assert_eq!(interval(14 * DAY, 2356, 31, false).as_secs(), 438);
        assert_eq!(interval(14 * DAY, 2356, 31, true).as_secs(), 219);
        for _ in 0..100 {
            let j = jittered(i);
            assert!(j >= i / 2 && j <= i * 3 / 2);
        }
    }

    #[test]
    fn subnet_keys() {
        let k = |s: &str| subnet_key(&s.parse().unwrap());
        assert_eq!(k("95.183.53.19:9999"), k("95.183.99.1:9999"));
        assert_ne!(k("95.183.53.19:9999"), k("96.183.53.19:9999"));
        assert_eq!(k("[2a01:4f8:1:2::1]:9999"), k("[2a01:4f8:ffff::2]:9999"));
        assert_ne!(k("[2a01:4f8::1]:9999"), k("[2a01:4f9::1]:9999"));
        let onion = "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:9999";
        assert_eq!(k(onion), "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion");
    }

    #[test]
    fn rate_window() {
        let mut w = RateWindow::new(Duration::from_secs(10));
        let t = Instant::now();
        assert_eq!(w.sample(t, 0), 0.0);
        assert_eq!(w.sample(t + Duration::from_secs(2), 1_000_000), 4_000_000.0);
        // Old samples fall out of the window.
        w.sample(t + Duration::from_secs(20), 1_000_000);
        assert_eq!(w.sample(t + Duration::from_secs(22), 1_000_000), 0.0);
    }
}
