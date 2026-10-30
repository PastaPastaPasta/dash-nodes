//! Turns what a bench test measured into a 0–100 score and a letter.
//!
//! The rubric is ported from QuantumExplorer's spv-network-health (src/grading.rs,
//! https://github.com/dashpay/spv-network-health, MIT), so a node grades the same in both tools:
//! the same five dimensions, weights, thresholds, completeness caps and letter cut-offs.
//!
//! | dimension    | weight | measures                                            |
//! |--------------|--------|-----------------------------------------------------|
//! | completeness | 0.45   | fraction of requested headers the node served       |
//! | throughput   | 0.20   | headers per second (100 at 2500/s or more)          |
//! | reliability  | 0.20   | 100, minus 25 per stall/timeout, 50 per bad header  |
//! | latency      | 0.10   | ping: 100 at 50 ms or less, 0 at 1 s or more         |
//! | connectivity | 0.05   | connect + handshake: 100 at 150 ms, 0 at 5 s         |
//!
//! Serving less than everything caps the score (C at best, under half D at best, under a tenth F),
//! and unmeasured dimensions (no ping answered) are left out with the weights renormalized.
//!
//! One deliberate difference: spv-network-health's latency uses dash-spv's average ping; we use the
//! median of our samples, which one delayed pong can't skew.

use serde::{Deserialize, Serialize};

const W_COMPLETENESS: f64 = 0.45;
const W_THROUGHPUT: f64 = 0.20;
const W_RELIABILITY: f64 = 0.20;
const W_LATENCY: f64 = 0.10;
const W_CONNECTIVITY: f64 = 0.05;

const FULL_MARKS_HEADERS_PER_SEC: f64 = 2500.0;
const FULL_MARKS_PING_MS: f64 = 50.0;
const ZERO_MARKS_PING_MS: f64 = 1000.0;
const FULL_MARKS_CONNECT_MS: f64 = 150.0;
const ZERO_MARKS_CONNECT_MS: f64 = 5000.0;
const TIMEOUT_PENALTY: f64 = 25.0;
const VALIDATION_FAILURE_PENALTY: f64 = 50.0;

const INCOMPLETE_SCORE_CAP: f64 = 69.0;
const UNDER_HALF_SCORE_CAP: f64 = 49.0;
const UNDER_TENTH_SCORE_CAP: f64 = 29.0;

/// The measurements the rubric uses (spv-network-health's ProbeMetrics, in milliseconds).
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub connect_ms: Option<f64>,
    pub handshake_ms: Option<f64>,
    pub ping_ms: Option<f64>,
    pub headers_served: u32,
    pub headers_requested: u32,
    pub headers_per_sec: f64,
    pub timeouts: u32,
    pub validation_failures: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grade {
    pub score: f64,
    pub letter: String,
    pub completeness: f64,
    pub throughput: f64,
    pub reliability: f64,
    pub latency: Option<f64>,
    pub connectivity: Option<f64>,
}

pub fn grade(m: &Metrics) -> Grade {
    let served_fraction = if m.headers_requested == 0 {
        0.0
    } else {
        f64::from(m.headers_served.min(m.headers_requested)) / f64::from(m.headers_requested)
    };
    let completeness = 100.0 * served_fraction;
    let throughput = 100.0 * (m.headers_per_sec / FULL_MARKS_HEADERS_PER_SEC).clamp(0.0, 1.0);
    let reliability = (100.0
        - f64::from(m.timeouts) * TIMEOUT_PENALTY
        - f64::from(m.validation_failures) * VALIDATION_FAILURE_PENALTY)
        .max(0.0);
    let latency = m.ping_ms.map(|p| inverse_scale(p, FULL_MARKS_PING_MS, ZERO_MARKS_PING_MS));
    let connect_total = m.connect_ms.map(|c| c + m.handshake_ms.unwrap_or(0.0));
    let connectivity = connect_total.map(|t| inverse_scale(t, FULL_MARKS_CONNECT_MS, ZERO_MARKS_CONNECT_MS));

    let mut weighted = completeness * W_COMPLETENESS + throughput * W_THROUGHPUT + reliability * W_RELIABILITY;
    let mut total_weight = W_COMPLETENESS + W_THROUGHPUT + W_RELIABILITY;
    for (score, weight) in [(latency, W_LATENCY), (connectivity, W_CONNECTIVITY)] {
        if let Some(s) = score {
            weighted += s * weight;
            total_weight += weight;
        }
    }
    let cap = if served_fraction >= 1.0 {
        100.0
    } else if served_fraction >= 0.5 {
        INCOMPLETE_SCORE_CAP
    } else if served_fraction >= 0.1 {
        UNDER_HALF_SCORE_CAP
    } else {
        UNDER_TENTH_SCORE_CAP
    };
    // The letter comes from the published (rounded) score, so the two never disagree.
    let score = round1((weighted / total_weight).min(cap));
    Grade {
        score,
        letter: letter_for(score).into(),
        completeness: round1(completeness),
        throughput: round1(throughput),
        reliability: round1(reliability),
        latency: latency.map(round1),
        connectivity: connectivity.map(round1),
    }
}

/// 100 at `best` or better, 0 at `worst` or worse, linear in between.
fn inverse_scale(v: f64, best: f64, worst: f64) -> f64 {
    if v <= best {
        100.0
    } else if v >= worst {
        0.0
    } else {
        100.0 * (worst - v) / (worst - best)
    }
}

pub fn letter_for(score: f64) -> &'static str {
    match score {
        s if s >= 90.0 => "A",
        s if s >= 75.0 => "B",
        s if s >= 60.0 => "C",
        s if s >= 40.0 => "D",
        _ => "F",
    }
}

pub fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

pub fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    //! spv-network-health's grading tests, translated one for one, plus exact values.
    use super::*;

    fn perfect() -> Metrics {
        Metrics {
            connect_ms: Some(40.0),
            handshake_ms: Some(60.0),
            ping_ms: Some(30.0),
            headers_served: 1000,
            headers_requested: 1000,
            headers_per_sec: 3000.0,
            ..Default::default()
        }
    }

    fn rank(letter: &str) -> usize {
        ["A", "B", "C", "D", "F"].iter().position(|l| *l == letter).unwrap()
    }

    #[test]
    fn perfect_node_gets_an_a() {
        let g = grade(&perfect());
        assert!(g.score > 95.0, "score was {}", g.score);
        assert_eq!(g.letter, "A");
    }

    #[test]
    fn unreachable_node_gets_an_f() {
        let g = grade(&Metrics { headers_requested: 1000, ..Default::default() });
        assert!(g.score <= 40.0, "score was {}", g.score);
        assert_eq!(g.letter, "F");
        assert_eq!(g.completeness, 0.0);
        assert_eq!(g.latency, None);
        assert_eq!(g.connectivity, None);
    }

    #[test]
    fn missing_ping_is_excluded_not_zeroed() {
        let a = grade(&Metrics { ping_ms: Some(10.0), ..perfect() });
        let b = grade(&Metrics { ping_ms: None, ..perfect() });
        assert!((a.score - b.score).abs() < 1.0, "with ping: {}, without: {}", a.score, b.score);
    }

    #[test]
    fn validation_failures_tank_reliability() {
        let g = grade(&Metrics { validation_failures: 2, ..perfect() });
        assert_eq!(g.reliability, 0.0);
        assert!(rank(&g.letter) > rank("A"), "bad data should cost the top grade");
    }

    #[test]
    fn partial_sync_scores_partially() {
        let g = grade(&Metrics { headers_served: 500, ..perfect() });
        assert!((g.completeness - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn any_shortfall_caps_at_c() {
        let g = grade(&Metrics { headers_served: 900, timeouts: 1, ..perfect() });
        assert!(g.score <= INCOMPLETE_SCORE_CAP, "score was {}", g.score);
        assert!(rank(&g.letter) >= rank("C"), "got {}", g.letter);
    }

    #[test]
    fn under_half_served_caps_at_d() {
        let g = grade(&Metrics { headers_served: 352, timeouts: 1, ..perfect() });
        assert!(g.score <= UNDER_HALF_SCORE_CAP, "score was {}", g.score);
        assert!(rank(&g.letter) >= rank("D"), "got {}", g.letter);
    }

    #[test]
    fn under_a_tenth_served_is_an_f() {
        let g = grade(&Metrics { headers_served: 50, timeouts: 1, ..perfect() });
        assert_eq!(g.letter, "F", "score was {}", g.score);
    }

    #[test]
    fn throughput_differentiates_below_full_marks() {
        let slow = grade(&Metrics { headers_per_sec: 800.0, ..perfect() });
        let fast = grade(&Metrics { headers_per_sec: 2600.0, ..perfect() });
        assert!(fast.score > slow.score + 5.0, "fast {} vs slow {}", fast.score, slow.score);
    }

    #[test]
    fn exact_values() {
        // 1250 headers/s = 50 throughput; ping 525 ms = 50 latency; connect+handshake 2575 ms = 50.
        let m = Metrics {
            connect_ms: Some(2000.0),
            handshake_ms: Some(575.0),
            ping_ms: Some(525.0),
            headers_per_sec: 1250.0,
            timeouts: 1,
            ..perfect()
        };
        let g = grade(&m);
        assert_eq!((g.throughput, g.latency, g.connectivity, g.reliability), (50.0, Some(50.0), Some(50.0), 75.0));
        // (100*.45 + 50*.2 + 75*.2 + 50*.1 + 50*.05) / 1.0 = 77.5
        assert_eq!(g.score, 77.5);
        assert_eq!(g.letter, "B");
        assert_eq!(letter_for(89.99), "B");
        assert_eq!(letter_for(60.0), "C");
        assert_eq!(letter_for(39.9), "F");
    }
}
