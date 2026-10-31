//! Turns the result history into the Worker's documents and uploads the changed ones through the
//! Worker's ingest, which limits each vantage's token to its own documents (an R2 token can't be
//! limited to a key prefix): `PUT {TARGET}/_ingest/<key>`, bearer token, gzip body with
//! `x-body-encoding: gzip`, retried with backoff, skipped when the content hasn't changed.
//!
//! - `v1/{net}/bench/{vantage}/{shard}.json`: `{"<protx>": [result, ...]}`, newest first, all 64
//!   shards (empty ones as `{}`); shard = first ProTx byte * 64 >> 8, two lowercase hex digits.
//! - `v1/{net}/bench/{vantage}/summary.json`: distributions of the latest result per masternode.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::schedule::tested_since;
use super::store::{History, Store};
use crate::network::Network;

const SHARDS: usize = 64;

/// Same as worker/src/api.js `shardOf`: the shard index, named by two hex digits.
fn shard_of(protx: &str) -> usize {
    let first = u8::from_str_radix(protx.get(..2).unwrap_or("00"), 16).unwrap_or(0);
    (first as usize * SHARDS) >> 8
}

/// Where this vantage's documents for `net` live.
fn prefix(net: Network, vantage: &str) -> String {
    format!("v1/{}/bench/{vantage}", net.name())
}

pub struct Vantage<'a> {
    pub id: &'a str,
    pub location: &'a str,
    pub cycle_days: f64,
}

/// Every document for one network: the 64 shards, then the summary.
pub fn documents(history: &History, net: Network, v: &Vantage, now: i64) -> Vec<(String, Value)> {
    let prefix = prefix(net, v.id);
    let mut shards: Vec<serde_json::Map<String, Value>> = vec![serde_json::Map::new(); SHARDS];
    let cycle_start = now - super::cycle_secs(v.cycle_days);
    let (mut scores, mut per_sec, mut ping) = (Vec::new(), Vec::new(), Vec::new());
    let mut grades: BTreeMap<&str, usize> = ["A", "B", "C", "D", "F"].into_iter().map(|l| (l, 0)).collect();
    let (mut tested, mut failed, mut updated) = (0, 0, 0);
    for ((n, protx), results) in history {
        if *n != net || results.is_empty() {
            continue;
        }
        shards[shard_of(protx)].insert(protx.clone(), json!(results));
        let latest = &results[0];
        updated = updated.max(latest.ts);
        if !tested_since(results, cycle_start) {
            continue;
        }
        tested += 1;
        let Some(grade) = latest.grade.as_ref().filter(|_| latest.ok) else {
            failed += 1;
            continue;
        };
        scores.push(grade.score);
        if let Some(n) = grades.get_mut(grade.letter.as_str()) {
            *n += 1;
        }
        if let Some(h) = &latest.headers
            && h.served > 0
        {
            per_sec.push(h.per_sec);
        }
        if let Some(p) = latest.ping_ms.as_ref().and_then(|p| p.median) {
            ping.push(p);
        }
    }
    for v in [&mut scores, &mut per_sec, &mut ping] {
        v.sort_by(f64::total_cmp);
    }
    let mut docs: Vec<(String, Value)> =
        shards.into_iter().enumerate().map(|(i, m)| (format!("{prefix}/{i:02x}.json"), Value::Object(m))).collect();
    docs.push((
        format!("{prefix}/summary.json"),
        json!({
            "vantage": v.id,
            "location": v.location,
            "cycle_days": v.cycle_days,
            // Time of the newest result (null before the first): unchanged data, unchanged document.
            "updated": (updated > 0).then_some(updated),
            "tested": tested,
            "scores": scores,
            "headers_per_sec": per_sec,
            "ping_ms": ping,
            "grades": grades,
            "failed": failed,
        }),
    ));
    docs
}

pub struct Uploader {
    client: reqwest::Client,
    target: String,
    token: Option<String>,
    token_file: Option<PathBuf>,
}

impl Uploader {
    pub fn new(target: &str, token: Option<String>, token_file: Option<PathBuf>) -> Uploader {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("http client");
        Uploader { client, target: target.trim_end_matches('/').to_owned(), token, token_file }
    }

    /// BENCH_TOKEN, else the contents of BENCH_TOKEN_FILE, read fresh each time so a token put in
    /// place later is picked up without a restart.
    pub fn token(&self) -> Option<String> {
        if let Some(t) = self.token.as_ref().filter(|t| !t.is_empty()) {
            return Some(t.clone());
        }
        let t = std::fs::read_to_string(self.token_file.as_ref()?).ok()?;
        Some(t.trim().to_owned()).filter(|t| !t.is_empty())
    }

    /// Uploads the documents that changed since their last upload (all of them with `force`, which
    /// also repairs anything lost or overwritten on the Worker's side); returns how many went up.
    /// Stops at the first failure, so an unreachable Worker doesn't hold up the tests for long; the
    /// rest go next time.
    pub async fn publish(&self, store: &Store, docs: &[(String, Value)], force: bool) -> Result<usize> {
        let Some(token) = self.token() else { bail!("no bench token") };
        let mut sent = 0;
        for (key, doc) in docs {
            let body = serde_json::to_vec(doc)?;
            let hash = format!("{:x}", Sha256::digest(&body));
            if !force && store.published(key)?.as_deref() == Some(hash.as_str()) {
                continue;
            }
            self.put(&token, key, &body).await.with_context(|| format!("{key} ({sent} uploaded before it)"))?;
            store.set_published(key, &hash)?;
            sent += 1;
        }
        Ok(sent)
    }

    async fn put(&self, token: &str, key: &str, body: &[u8]) -> Result<()> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(body)?;
        let gz = gz.finish()?;
        let url = format!("{}/_ingest/{key}", self.target);
        let mut last = String::new();
        for attempt in 1..=3u32 {
            let resp = self
                .client
                .put(&url)
                .bearer_auth(token)
                .header("x-body-encoding", "gzip")
                .header("content-type", "application/json")
                .body(gz.clone())
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => return Ok(()),
                // Refused for good (bad token, key not allowed): retrying won't help.
                Ok(r) if matches!(r.status().as_u16(), 400 | 401 | 403 | 404 | 413) => {
                    bail!(
                        "HTTP {}: {}",
                        r.status(),
                        r.text().await.unwrap_or_default().chars().take(200).collect::<String>()
                    )
                }
                Ok(r) => last = format!("HTTP {}", r.status()),
                Err(e) => last = format!("{e}"),
            }
            if attempt < 3 {
                tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
            }
        }
        bail!("{last}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::grade::Grade;
    use super::super::probe::{HeaderStats, PingStats};
    use super::super::store::{remember, tests::result};
    use super::*;

    #[test]
    fn shards_match_the_worker() {
        // shardOf(protx) = parseInt(protx.slice(0, 2), 16) * 64 >> 8
        assert_eq!(shard_of("00ab"), 0);
        assert_eq!(shard_of("03ff"), 0);
        assert_eq!(shard_of("04"), 1);
        assert_eq!(shard_of("7f"), 0x1f);
        assert_eq!(shard_of("80"), 0x20);
        assert_eq!(shard_of("ff"), 0x3f);
        assert_eq!(shard_of("FC"), 0x3f);
    }

    fn graded(ts: i64, score: f64, letter: &str, per_sec: f64, ping: f64) -> super::super::probe::BenchResult {
        let mut r = result(ts, true);
        r.grade = Some(Grade {
            score,
            letter: letter.into(),
            completeness: 100.0,
            throughput: 100.0,
            reliability: 100.0,
            latency: None,
            connectivity: None,
        });
        r.headers = Some(HeaderStats {
            start_height: 0,
            requested: 10,
            served: 10,
            valid: true,
            seconds: 1.0,
            per_sec,
            bytes: 810,
            stalls: 0,
        });
        r.ping_ms = Some(PingStats { min: Some(ping), median: Some(ping), max: Some(ping), samples: 1 });
        r
    }

    #[test]
    fn documents_and_summary() {
        let now = 100 * 86_400;
        let mut h = History::new();
        let a = "0".repeat(64);
        let b = format!("ff{}", "1".repeat(62));
        let c = format!("80{}", "2".repeat(62));
        let d = format!("81{}", "3".repeat(62));
        remember(&mut h, Network::Mainnet, &a, graded(now - 100, 92.0, "A", 3000.0, 40.0));
        remember(&mut h, Network::Mainnet, &a, graded(now - 50 * 86_400, 10.0, "F", 1.0, 900.0));
        remember(&mut h, Network::Mainnet, &b, graded(now - 200, 55.0, "D", 700.0, 120.0));
        remember(&mut h, Network::Mainnet, &c, result(now - 300, false));
        // Outside the cycle: published in its shard, not counted.
        remember(&mut h, Network::Mainnet, &d, graded(now - 20 * 86_400, 80.0, "B", 1.0, 1.0));
        remember(&mut h, Network::Testnet, &a, graded(now, 1.0, "F", 1.0, 1.0));
        let v = Vantage { id: "nyc", location: "New York", cycle_days: 14.0 };
        let docs = documents(&h, Network::Mainnet, &v, now);
        assert_eq!(docs.len(), SHARDS + 1);
        assert_eq!(docs[0].0, "v1/mainnet/bench/nyc/00.json");
        assert_eq!(docs[63].0, "v1/mainnet/bench/nyc/3f.json");
        assert_eq!(docs[0].1[&a].as_array().unwrap().len(), 2);
        assert_eq!(docs[0].1[&a][0]["grade"]["score"], 92.0);
        assert_eq!(docs[0x3f].1[&b][0]["ok"], true);
        assert_eq!(docs[0x20].1.as_object().unwrap().len(), 2);
        assert_eq!(docs[1].1, json!({}));
        let (key, s) = &docs[SHARDS];
        assert_eq!(key, "v1/mainnet/bench/nyc/summary.json");
        assert_eq!(
            *s,
            json!({
                "vantage": "nyc", "location": "New York", "cycle_days": 14.0, "updated": now - 100,
                "tested": 3, "scores": [55.0, 92.0], "headers_per_sec": [700.0, 3000.0], "ping_ms": [40.0, 120.0],
                "grades": {"A": 1, "B": 0, "C": 0, "D": 1, "F": 0}, "failed": 1,
            })
        );
    }

    #[test]
    fn empty_network_still_has_every_document() {
        let v = Vantage { id: "nyc", location: "", cycle_days: 14.0 };
        let docs = documents(&History::new(), Network::Testnet, &v, 1);
        assert_eq!(docs.len(), SHARDS + 1);
        assert!(docs[..SHARDS].iter().all(|(_, d)| *d == json!({})));
        assert_eq!(docs[SHARDS].1["updated"], Value::Null);
        assert_eq!(docs[SHARDS].1["tested"], 0);
    }

    #[test]
    fn token_from_env_or_file() {
        let path = std::env::temp_dir().join(format!("bench-token-{}", std::process::id()));
        let u = Uploader::new("http://x/", None, Some(path.clone()));
        assert_eq!(u.target, "http://x");
        assert_eq!(u.token(), None);
        std::fs::write(&path, "").unwrap();
        assert_eq!(u.token(), None);
        std::fs::write(&path, "secret\n").unwrap();
        assert_eq!(u.token().as_deref(), Some("secret"));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(Uploader::new("http://x", Some("t".into()), None).token().as_deref(), Some("t"));
    }
}
