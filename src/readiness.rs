//! Will a masternode be judged a bad connection in DKGs (distributed key generation)?
//!
//! When quorum PoSe is on, each DKG member checks its connections to the other members and votes a
//! member a bad connection if it isn't connected or failed outbound connections too often, is
//! Platform PoSe banned, runs a protocol older than MIN_MASTERNODE_PROTO_VERSION, or, on its
//! masternode connections, doesn't relay transactions or doesn't serve compact block filters.
//! Members voted bad are left out of the quorum and get PoSe penalties.
//!
//! We estimate each rule from what we see: the version handshake a masternode sends our crawler
//! (an inbound, non-masternode connection), our port check, and our dashd's Platform ban flag.
//! Services and relay are the same for every inbound connection unless the operator does
//! something unusual (e.g. relay permission for some peers in blocksonly mode).

use serde::Serialize;
use serde_json::{Value, json};

/// The rules as of Dash Core `develop` at commit 4326d223dbee (2026-10-30):
/// `src/active/dkgsession.cpp`, `ActiveDKGSession::VerifyConnectionAndMinProtoVersions`. Bump these
/// together when Core changes them.
pub mod core {
    /// Commit the constants below were read at.
    pub const COMMIT: &str = "4326d223dbee";
    /// `MIN_MASTERNODE_PROTO_VERSION`, src/version.h:23 (bumped for v24 in f76414762e1a).
    pub const MIN_MASTERNODE_PROTO_VERSION: i64 = 70242;
    /// `NODE_COMPACT_FILTERS = (1 << 6)`, src/protocol.h:322.
    pub const NODE_COMPACT_FILTERS_BIT: u32 = 6;
    /// `MASTERNODE_MAX_FAILED_OUTBOUND_ATTEMPTS`, src/masternode/meta.cpp:13: more failed outbound
    /// attempts than this (the count resets on a successful connection) and members vote it bad.
    pub const MASTERNODE_MAX_FAILED_OUTBOUND_ATTEMPTS: i64 = 5;
    /// The first release that enforces these rules, as people know it.
    pub const RELEASE: &str = "Dash Core 24";
}

/// Port checks missed in a row before we call reachability failed. Core judges repeated failed
/// connections (above MASTERNODE_MAX_FAILED_OUTBOUND_ATTEMPTS, reset by any success), not one miss;
/// our check runs every crawl from one place, so a few misses in a row is our stand-in.
pub const SUSTAINED_FAILURES: i64 = 3;

const NODE_COMPACT_FILTERS: i64 = 1 << core::NODE_COMPACT_FILTERS_BIT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Pass,
    Fail,
    Unknown,
}

impl Check {
    pub fn name(self) -> &'static str {
        match self {
            Check::Pass => "pass",
            Check::Fail => "fail",
            Check::Unknown => "unknown",
        }
    }

    fn of(ok: Option<bool>) -> Check {
        match ok {
            Some(true) => Check::Pass,
            Some(false) => Check::Fail,
            None => Check::Unknown,
        }
    }
}

/// What a masternode announced in its latest version handshake with us.
#[derive(Clone, Copy, Debug, Default)]
pub struct Handshake {
    pub protocol: Option<i64>,
    pub services: Option<i64>,
    /// None for handshakes recorded before the relay flag was stored.
    pub relay: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Input {
    /// The latest handshake, if it's recent enough to go by (None: no handshake this week).
    pub handshake: Option<Handshake>,
    /// Our latest port check (None: not checked in the latest crawl).
    pub reachable: Option<bool>,
    /// Port checks missed in a row (`nodes.fail_streak`).
    pub fail_streak: i64,
    pub evo: bool,
    /// Our dashd's `metaInfo.is_platform_banned` (None: not reported).
    pub platform_banned: Option<bool>,
}

/// The rules, in the order the page lists them. `platform_ban` is None for regular masternodes,
/// which Platform can't ban.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Readiness {
    pub protocol: Check,
    pub relay: Check,
    pub filters: Check,
    pub reachable: Check,
    pub platform_ban: Option<Check>,
}

pub const CHECKS: [&str; 5] = ["protocol", "relay", "filters", "reachable", "platform_ban"];

pub fn evaluate(i: &Input) -> Readiness {
    let h = i.handshake.unwrap_or_default();
    Readiness {
        protocol: Check::of(h.protocol.map(|p| p >= core::MIN_MASTERNODE_PROTO_VERSION)),
        relay: Check::of(h.relay),
        filters: Check::of(h.services.map(|s| s & NODE_COMPACT_FILTERS != 0)),
        // One missed check isn't what Core judges; only a run of them.
        reachable: match i.reachable {
            Some(false) if i.fail_streak < SUSTAINED_FAILURES => Check::Pass,
            r => Check::of(r),
        },
        platform_ban: i.evo.then(|| Check::of(i.platform_banned.map(|b| !b))),
    }
}

impl Readiness {
    /// Every check in CHECKS order; None where it doesn't apply.
    fn slots(&self) -> [Option<Check>; CHECKS.len()] {
        [Some(self.protocol), Some(self.relay), Some(self.filters), Some(self.reachable), self.platform_ban]
    }

    /// Every check by name, the ones that don't apply left out.
    pub fn checks(&self) -> impl Iterator<Item = (&'static str, Check)> {
        CHECKS.into_iter().zip(self.slots()).filter_map(|(k, c)| Some((k, c?)))
    }

    /// Fails if any check fails; passes only if all pass.
    pub fn overall(&self) -> Check {
        let mut out = Check::Pass;
        for (_, c) in self.checks() {
            match c {
                Check::Fail => return Check::Fail,
                Check::Unknown => out = Check::Unknown,
                Check::Pass => {}
            }
        }
        out
    }

    pub fn json(&self) -> Value {
        let checks: serde_json::Map<String, Value> = self.checks().map(|(k, c)| (k.into(), c.name().into())).collect();
        json!({"overall": self.overall().name(), "checks": checks})
    }
}

/// The thresholds, for pages to quote instead of hardcoding them.
pub fn rules_json() -> Value {
    json!({
        "release": core::RELEASE,
        "min_protocol": core::MIN_MASTERNODE_PROTO_VERSION,
        "compact_filters_bit": core::NODE_COMPACT_FILTERS_BIT,
        "reach_failures": SUSTAINED_FAILURES,
        "core_max_failed_outbound": core::MASTERNODE_MAX_FAILED_OUTBOUND_ATTEMPTS,
        "core_commit": core::COMMIT,
    })
}

#[derive(Default, Serialize)]
struct Counts {
    pass: i64,
    fail: i64,
    unknown: i64,
}

impl Counts {
    fn add(&mut self, c: Check) {
        match c {
            Check::Pass => self.pass += 1,
            Check::Fail => self.fail += 1,
            Check::Unknown => self.unknown += 1,
        }
    }
}

/// Pass, fail and unknown counts per check and overall.
#[derive(Default)]
pub struct Tally {
    masternodes: i64,
    /// Failing the protocol check and passing every other: these only need the upgrade.
    version_only: i64,
    /// Failing a check other than the protocol: these need a fix besides the upgrade.
    beyond_version: i64,
    overall: Counts,
    checks: [Counts; CHECKS.len()],
}

impl Tally {
    pub fn add(&mut self, r: &Readiness) {
        self.masternodes += 1;
        self.overall.add(r.overall());
        if r.protocol == Check::Fail && r.checks().filter(|(_, c)| *c != Check::Pass).count() == 1 {
            self.version_only += 1;
        }
        if [r.relay, r.filters, r.reachable].contains(&Check::Fail) || r.platform_ban == Some(Check::Fail) {
            self.beyond_version += 1;
        }
        for (counts, c) in self.checks.iter_mut().zip(r.slots()) {
            if let Some(c) = c {
                counts.add(c);
            }
        }
    }

    pub fn to_json(&self) -> Value {
        let checks: serde_json::Map<String, Value> =
            CHECKS.iter().zip(&self.checks).map(|(k, n)| ((*k).into(), json!(n))).collect();
        json!({
            "rules": rules_json(), "masternodes": self.masternodes, "overall": self.overall, "checks": checks,
            "version_only": self.version_only, "beyond_version": self.beyond_version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = core::MIN_MASTERNODE_PROTO_VERSION;

    fn ready_input() -> Input {
        Input {
            handshake: Some(Handshake { protocol: Some(MIN), services: Some(0x0c45), relay: Some(true) }),
            reachable: Some(true),
            fail_streak: 0,
            evo: false,
            platform_banned: None,
        }
    }

    #[test]
    fn reachability_needs_a_run_of_misses() {
        let mut i = ready_input();
        i.reachable = Some(false);
        i.fail_streak = 1;
        assert_eq!(evaluate(&i).reachable, Check::Pass);
        i.fail_streak = SUSTAINED_FAILURES;
        assert_eq!(evaluate(&i).reachable, Check::Fail);
        i.reachable = None;
        assert_eq!(evaluate(&i).reachable, Check::Unknown);
    }

    #[test]
    fn protocol_threshold() {
        let mut i = ready_input();
        assert_eq!(evaluate(&i).protocol, Check::Pass);
        assert_eq!(evaluate(&i).overall(), Check::Pass);
        i.handshake.as_mut().unwrap().protocol = Some(MIN - 1);
        assert_eq!(evaluate(&i).protocol, Check::Fail);
        assert_eq!(evaluate(&i).overall(), Check::Fail);
        i.handshake.as_mut().unwrap().protocol = Some(MIN + 1);
        assert_eq!(evaluate(&i).protocol, Check::Pass);
    }

    #[test]
    fn relay_off_fails() {
        let mut i = ready_input();
        i.handshake.as_mut().unwrap().relay = Some(false);
        let r = evaluate(&i);
        assert_eq!((r.relay, r.overall()), (Check::Fail, Check::Fail));
        // A handshake stored before the relay flag was: unknown, not a failure.
        i.handshake.as_mut().unwrap().relay = None;
        let r = evaluate(&i);
        assert_eq!((r.relay, r.overall()), (Check::Unknown, Check::Unknown));
    }

    #[test]
    fn compact_filters_bit() {
        let mut i = ready_input();
        // A node without bit 6 (23.x masternodes have it: Core turns peerblockfilters on for them).
        i.handshake.as_mut().unwrap().services = Some(0x0c05);
        let r = evaluate(&i);
        assert_eq!((r.filters, r.overall()), (Check::Fail, Check::Fail));
        i.handshake.as_mut().unwrap().services = Some(NODE_COMPACT_FILTERS);
        assert_eq!(evaluate(&i).filters, Check::Pass);
    }

    #[test]
    fn no_handshake_is_unknown() {
        let i = Input { handshake: None, reachable: None, fail_streak: 0, evo: false, platform_banned: None };
        let r = evaluate(&i);
        for (_, c) in r.checks() {
            assert_eq!(c, Check::Unknown);
        }
        assert_eq!(r.overall(), Check::Unknown);
        assert_eq!(r.json()["checks"].as_object().unwrap().len(), 4);
        // One known failure decides, whatever is unknown.
        let r = evaluate(&Input { reachable: Some(false), fail_streak: SUSTAINED_FAILURES, ..i });
        assert_eq!(r.overall(), Check::Fail);
    }

    #[test]
    fn platform_ban_only_for_evonodes() {
        let mut i = ready_input();
        assert_eq!(evaluate(&i).platform_ban, None);
        i.evo = true;
        assert_eq!(evaluate(&i).platform_ban, Some(Check::Unknown));
        i.platform_banned = Some(false);
        assert_eq!(evaluate(&i).overall(), Check::Pass);
        i.platform_banned = Some(true);
        assert_eq!(evaluate(&i).overall(), Check::Fail);
        assert_eq!(evaluate(&i).json()["checks"]["platform_ban"], "fail");
    }

    #[test]
    fn tally_counts() {
        let mut t = Tally::default();
        t.add(&evaluate(&ready_input()));
        t.add(&evaluate(&Input {
            handshake: None,
            reachable: Some(false),
            fail_streak: SUSTAINED_FAILURES,
            evo: true,
            platform_banned: Some(false),
        }));
        let mut old = ready_input();
        old.handshake.as_mut().unwrap().protocol = Some(MIN - 2);
        t.add(&evaluate(&old));
        // Relay not known yet: fails the version, but we can't say it's the only problem.
        old.handshake.as_mut().unwrap().relay = None;
        t.add(&evaluate(&old));
        old.handshake.as_mut().unwrap().relay = Some(false);
        t.add(&evaluate(&old));
        let v = t.to_json();
        assert_eq!(v["masternodes"], 5);
        assert_eq!(v["version_only"], 1);
        // The unreachable one and the one with relay off.
        assert_eq!(v["beyond_version"], 2);
        assert_eq!(v["overall"], json!({"pass": 1, "fail": 4, "unknown": 0}));
        assert_eq!(v["checks"]["protocol"], json!({"pass": 1, "fail": 3, "unknown": 1}));
        assert_eq!(v["checks"]["relay"], json!({"pass": 2, "fail": 1, "unknown": 2}));
        assert_eq!(v["checks"]["platform_ban"], json!({"pass": 1, "fail": 0, "unknown": 0}));
        assert_eq!(v["rules"]["min_protocol"], MIN);
    }
}
