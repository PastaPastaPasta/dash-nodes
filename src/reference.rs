//! Reference block hashes for the bench (`/api/v1/{net}/reference`): our dashd's hashes at
//! `tip - 50,000`, then every 2,000 blocks up to `tip`. A bench downloads the headers after the first
//! one, and every 2,000-header batch it gets back ends on one of these heights, so a node serving
//! another chain is caught within a batch.
//!
//! `tip` sits a few blocks below dashd's tip, so a reorg of the newest blocks can't make honest
//! nodes look wrong. The hashes are refreshed when the tip has moved 100 blocks.
//!
//! Document: `{"tip": H, "tip_hash": "<hex>", "hashes": {"<height>": "<hex>", ...}}` (RPC byte order).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::NetCtx;

/// Headers a bench may download, and the spacing of the reference heights.
pub const DEPTH: u32 = 50_000;
pub const STEP: u32 = 2_000;
const CONFIRMATIONS: i64 = 6;
const REFRESH_BLOCKS: i64 = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reference {
    pub tip: u32,
    pub tip_hash: String,
    /// serde_json writes the integer keys as strings, in numeric order (and reads them back).
    pub hashes: BTreeMap<u32, String>,
}

/// The heights a reference at `tip` lists.
pub fn heights(tip: u32) -> Vec<u32> {
    let start = tip.saturating_sub(DEPTH);
    (0..=DEPTH / STEP).map(|k| start + k * STEP).filter(|&h| h <= tip).collect()
}

pub async fn run(ctx: Arc<NetCtx>) {
    loop {
        if let Err(e) = refresh(&ctx).await {
            tracing::warn!(net = ctx.net.name(), "reference: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn refresh(ctx: &NetCtx) -> Result<()> {
    let (blocks, synced) = {
        let s = ctx.status.read().unwrap();
        (s.blocks, s.synced)
    };
    let tip = blocks - CONFIRMATIONS;
    let current = ctx.reference.read().unwrap().as_ref().map(|r| r.tip);
    if !synced || tip < i64::from(DEPTH) || current.is_some_and(|c| tip - i64::from(c) < REFRESH_BLOCKS) {
        return Ok(());
    }
    let tip = tip as u32;
    let heights = heights(tip);
    let calls: Vec<(&str, Value)> = heights.iter().map(|h| ("getblockhash", json!([h]))).collect();
    let hashes = ctx.rpc.batch(&calls).await?;
    let mut map = BTreeMap::new();
    for (h, v) in heights.iter().zip(hashes) {
        let Some(hash) = v.as_str().filter(|s| s.len() == 64) else { bail!("getblockhash {h}: {v}") };
        map.insert(*h, hash.to_owned());
    }
    let reference = Reference { tip, tip_hash: map[&tip].clone(), hashes: map };
    tracing::info!(net = ctx.net.name(), "reference at {tip}");
    *ctx.reference.write().unwrap() = Some(reference);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_heights() {
        let h = heights(2_563_713);
        assert_eq!(h.len(), 26);
        assert_eq!(h[0], 2_513_713);
        assert_eq!(h[1], 2_515_713);
        assert_eq!(*h.last().unwrap(), 2_563_713);
        let r = Reference { tip: 3, tip_hash: "c".into(), hashes: BTreeMap::from([(3, "c".into()), (12, "x".into())]) };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"tip":3,"tip_hash":"c","hashes":{"3":"c","12":"x"}}"#);
    }
}
