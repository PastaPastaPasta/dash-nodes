//! Public JSON API and static pages.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::collector::payments_range;
use crate::db::{self, day_of};
use crate::network::Network;
use crate::state::NetCtx;

pub type Nets = Arc<HashMap<Network, Arc<NetCtx>>>;

pub fn router(nets: Nets, web: &Path) -> Router {
    let not_found: &'static str = std::fs::read_to_string(web.join("404.html")).unwrap_or_default().leak();
    let api = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/{net}/summary", get(summary))
        .route("/api/v1/{net}/history", get(history))
        .route("/api/v1/{net}/masternodes", get(masternodes))
        .route("/api/v1/{net}/masternodes/{protx}", get(masternode))
        .route("/api/v1/{net}/masternodes/{protx}/payments", get(payments))
        .with_state(nets)
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=60"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ));
    let page = |name: &str| ServeFile::new(web.join(name));
    let pages = Router::new()
        .route_service("/", page("index.html"))
        .route_service("/masternodes", page("masternodes.html"))
        .route_service("/mn/{protx}", page("mn.html"))
        .route_service("/api", page("api.html"))
        .nest_service("/assets", ServeDir::new(web.join("assets")))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=300"),
        ));
    Router::new()
        .merge(api)
        .merge(pages)
        .route("/healthz", get(|| async { "ok" }))
        .fallback(move || async move { (StatusCode::NOT_FOUND, Html(not_found)) })
        .layer(CompressionLayer::new())
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (self.0, Json(json!({"error": self.1}))).into_response();
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::warn!("api: {e:#}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn net_ctx(nets: &Nets, net: &str) -> Result<Arc<NetCtx>, ApiError> {
    Network::parse(net)
        .and_then(|n| nets.get(&n).cloned())
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown network {net}")))
}

async fn status(State(nets): State<Nets>) -> ApiResult {
    let mut out = serde_json::Map::new();
    for net in Network::ALL {
        let Some(ctx) = nets.get(&net) else { continue };
        let range = payments_range(ctx).await?;
        out.insert(
            net.name().into(),
            json!({
                "chain": *ctx.status.read().unwrap(),
                "crawl": *ctx.crawl_status.read().unwrap(),
                "payments_indexed": range.map(|(lo, hi)| json!({"from": lo, "to": hi})),
                "geo_loaded": ctx.geo.loaded(),
            }),
        );
    }
    Ok(Json(json!({"networks": out, "version": env!("CARGO_PKG_VERSION")})))
}

fn crawl_json(r: &Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, i64>("id")?,
        "started": r.get::<_, i64>("started")?,
        "finished": r.get::<_, Option<i64>>("finished")?,
        "tip_height": r.get::<_, Option<i64>>("tip_height")?,
        "tried": r.get::<_, Option<i64>>("tried")?,
        "reachable": r.get::<_, Option<i64>>("reachable")?,
        "discovered": r.get::<_, Option<i64>>("discovered")?,
        "mn_total": r.get::<_, Option<i64>>("mn_total")?,
        "mn_reachable": r.get::<_, Option<i64>>("mn_reachable")?,
    }))
}

fn latest_crawl(c: &Connection) -> rusqlite::Result<Option<i64>> {
    c.query_row("SELECT max(id) FROM crawls WHERE finished IS NOT NULL", [], |r| r.get(0))
}

/// Latest crawl and every distribution from it: `dists[scope][dim] = [[key, count], ...]`, largest first.
async fn summary(State(nets): State<Nets>, UrlPath(net): UrlPath<String>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let body = ctx
        .db
        .read(|c| {
            let Some(id) = latest_crawl(c)? else { return Ok(json!({"crawl": null, "dists": {}})) };
            let crawl = c.query_row("SELECT * FROM crawls WHERE id = ?1", [id], crawl_json)?;
            // The crawl closest to 24h earlier, for deltas.
            let started: i64 = crawl["started"].as_i64().unwrap_or(0);
            let day_ago = c
                .query_row(
                    "SELECT * FROM crawls WHERE finished IS NOT NULL AND started <= ?1 ORDER BY started DESC LIMIT 1",
                    [started - 86_400],
                    crawl_json,
                )
                .optional()?;
            let mut dists: BTreeMap<String, BTreeMap<String, Vec<(String, i64)>>> = BTreeMap::new();
            let mut st = c.prepare("SELECT scope, dim, key, count FROM crawl_agg WHERE crawl_id = ?1")?;
            let mut rows = st.query([id])?;
            while let Some(r) = rows.next()? {
                dists.entry(r.get(0)?).or_default().entry(r.get(1)?).or_default().push((r.get(2)?, r.get(3)?));
            }
            for dims in dists.values_mut() {
                for v in dims.values_mut() {
                    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                }
            }
            Ok(json!({"crawl": crawl, "day_ago": day_ago, "dists": dists}))
        })
        .await?;
    Ok(Json(body))
}

#[derive(Deserialize)]
struct HistoryQuery {
    scope: Option<String>,
    dim: String,
    days: Option<i64>,
    top: Option<usize>,
}

/// Time series. Up to 14 days back it's one point per crawl; beyond that, daily averages.
/// `dim=totals` gives node and masternode counts instead of a distribution.
async fn history(State(nets): State<Nets>, UrlPath(net): UrlPath<String>, Query(q): Query<HistoryQuery>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let days = q.days.unwrap_or(30).clamp(1, 3650);
    let top = q.top.unwrap_or(8).clamp(1, 50);
    let scope = q.scope.unwrap_or_else(|| "all".into());
    let dim = q.dim;
    const DIMS: &[&str] = &["totals", "ua", "version", "client", "protocol", "country", "asn", "reachable", "status", "type"];
    if !DIMS.contains(&dim.as_str()) {
        return Err(ApiError(StatusCode::BAD_REQUEST, format!("unknown dim {dim}")));
    }
    let body = ctx
        .db
        .read(move |c| {
            let since = db::now() - days * 86_400;
            let per_crawl = days <= 14;
            if dim == "totals" {
                return totals_history(c, since, per_crawl);
            }
            // (point index, key, value)
            let mut t: Vec<i64> = Vec::new();
            let mut points: Vec<(usize, String, f64)> = Vec::new();
            if per_crawl {
                let mut index = HashMap::new();
                let mut st = c.prepare("SELECT id, started FROM crawls WHERE finished IS NOT NULL AND started >= ?1 ORDER BY started")?;
                let mut rows = st.query([since])?;
                while let Some(r) = rows.next()? {
                    index.insert(r.get::<_, i64>(0)?, t.len());
                    t.push(r.get(1)?);
                }
                let mut st = c.prepare(
                    "SELECT crawl_id, key, count FROM crawl_agg WHERE scope = ?1 AND dim = ?2
                     AND crawl_id >= (SELECT coalesce(min(id), 0) FROM crawls WHERE started >= ?3)",
                )?;
                let mut rows = st.query(params![scope, dim, since])?;
                while let Some(r) = rows.next()? {
                    if let Some(&i) = index.get(&r.get::<_, i64>(0)?) {
                        points.push((i, r.get(1)?, r.get::<_, i64>(2)? as f64));
                    }
                }
            } else {
                let mut index = HashMap::new();
                let mut st = c.prepare(
                    "SELECT day, key, avg FROM daily_agg WHERE scope = ?1 AND dim = ?2 AND day >= ?3 ORDER BY day",
                )?;
                let mut rows = st.query(params![scope, dim, day_of(since)])?;
                while let Some(r) = rows.next()? {
                    let day: String = r.get(0)?;
                    let i = *index.entry(day.clone()).or_insert_with(|| {
                        t.push(day_start(&day));
                        t.len() - 1
                    });
                    points.push((i, r.get(1)?, r.get(2)?));
                }
            }
            Ok(top_series(t, points, top, per_crawl))
        })
        .await?;
    Ok(Json(body))
}

fn day_start(day: &str) -> i64 {
    // Inverse of day_of for YYYY-MM-DD (days-from-civil).
    let p: Vec<i64> = day.split('-').filter_map(|x| x.parse().ok()).collect();
    let (y, m, d) = (p[0] - i64::from(p[1] <= 2), p[1], p[2]);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400
}

/// Keeps the `top` keys by total over the window; folds the rest into "Other".
fn top_series(t: Vec<i64>, points: Vec<(usize, String, f64)>, top: usize, per_crawl: bool) -> Value {
    let mut totals: HashMap<&str, f64> = HashMap::new();
    for (_, k, v) in &points {
        *totals.entry(k).or_default() += v;
    }
    let mut keys: Vec<(&str, f64)> = totals.into_iter().collect();
    keys.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let kept: Vec<&str> = keys.iter().take(top).map(|k| k.0).collect();
    let mut series: Vec<Vec<f64>> = vec![vec![0.0; t.len()]; kept.len()];
    let mut other = vec![0.0; t.len()];
    let has_other = keys.len() > top;
    for (i, k, v) in &points {
        match kept.iter().position(|x| x == k) {
            Some(j) => series[j][*i] += v,
            None => other[*i] += v,
        }
    }
    let mut out: Vec<Value> = kept.iter().zip(series).map(|(k, v)| json!({"key": k, "values": v})).collect();
    if has_other {
        out.push(json!({"key": "Other", "values": other}));
    }
    json!({"resolution": if per_crawl { "crawl" } else { "day" }, "t": t, "series": out})
}

fn totals_history(c: &Connection, since: i64, per_crawl: bool) -> Result<Value> {
    let sql = if per_crawl {
        "SELECT started, reachable, mn_total, mn_reachable FROM crawls
         WHERE finished IS NOT NULL AND started >= ?1 ORDER BY started"
    } else {
        "SELECT started - started % 86400 AS d, avg(reachable), avg(mn_total), avg(mn_reachable) FROM crawls
         WHERE finished IS NOT NULL AND started >= ?1 GROUP BY d ORDER BY d"
    };
    let mut t = Vec::new();
    let mut cols: [Vec<f64>; 3] = Default::default();
    let mut st = c.prepare(sql)?;
    let mut rows = st.query([since])?;
    while let Some(r) = rows.next()? {
        t.push(r.get::<_, i64>(0)?);
        for (i, col) in cols.iter_mut().enumerate() {
            col.push(r.get::<_, Option<f64>>(i + 1)?.unwrap_or(0.0));
        }
    }
    let [reachable, mn_total, mn_reachable] = cols;
    Ok(json!({
        "resolution": if per_crawl { "crawl" } else { "day" },
        "t": t,
        "series": [
            {"key": "reachable_nodes", "values": reachable},
            {"key": "masternodes", "values": mn_total},
            {"key": "masternodes_reachable", "values": mn_reachable},
        ],
    }))
}

const MN_COLUMNS: &str = "m.protx, m.type, m.service, m.payout_address, m.registered_height, m.last_paid_height,
    m.pose_penalty, m.pose_ban_height, m.operator_reward, m.platform_node_id,
    n.user_agent, n.protocol, n.last_crawl, n.last_ok, n.last_success, n.latency_ms, n.country, n.asn, n.as_org,
    n.last_error, n.start_height";

fn mn_json(r: &Row, latest: Option<i64>) -> rusqlite::Result<Value> {
    let ban: Option<i64> = r.get("pose_ban_height")?;
    let last_crawl: Option<i64> = r.get("last_crawl")?;
    let reachable = match (last_crawl, latest) {
        (Some(a), Some(b)) if a == b => Some(r.get::<_, i64>("last_ok")? == 1),
        _ => None,
    };
    let ua: Option<String> = r.get("user_agent")?;
    Ok(json!({
        "protx": r.get::<_, String>("protx")?,
        "type": r.get::<_, String>("type")?,
        "status": if ban.unwrap_or(-1) == -1 { "enabled" } else { "pose_banned" },
        "service": r.get::<_, Option<String>>("service")?,
        "payout_address": r.get::<_, Option<String>>("payout_address")?,
        "registered_height": r.get::<_, Option<i64>>("registered_height")?,
        "last_paid_height": r.get::<_, Option<i64>>("last_paid_height")?,
        "pose_penalty": r.get::<_, Option<i64>>("pose_penalty")?,
        "pose_ban_height": ban,
        "operator_reward": r.get::<_, Option<f64>>("operator_reward")?,
        "platform_node_id": r.get::<_, Option<String>>("platform_node_id")?,
        "version": ua.as_deref().map(crate::ua::version_label),
        "user_agent": ua,
        "protocol": r.get::<_, Option<i64>>("protocol")?,
        "reachable": reachable,
        "last_seen": r.get::<_, Option<i64>>("last_success")?,
        "latency_ms": r.get::<_, Option<i64>>("latency_ms")?,
        "start_height": r.get::<_, Option<i64>>("start_height")?,
        "last_error": if reachable == Some(false) { r.get::<_, Option<String>>("last_error")? } else { None },
        "country": r.get::<_, Option<String>>("country")?,
        "asn": r.get::<_, Option<i64>>("asn")?,
        "as_org": r.get::<_, Option<String>>("as_org")?,
    }))
}

fn uptime(ok: Option<i64>, attempts: Option<i64>) -> Value {
    match (ok, attempts) {
        (Some(ok), Some(a)) if a > 0 => json!((ok as f64 / a as f64 * 10_000.0).round() / 10_000.0),
        _ => Value::Null,
    }
}

async fn masternodes(State(nets): State<Nets>, UrlPath(net): UrlPath<String>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let body = ctx
        .db
        .read(|c| {
            let latest = latest_crawl(c)?;
            let now = db::now();
            let sql = format!(
                "SELECT {MN_COLUMNS}, u7.ok AS ok7, u7.a AS a7, u30.ok AS ok30, u30.a AS a30
                 FROM masternodes m LEFT JOIN nodes n ON n.addr = m.service
                 LEFT JOIN (SELECT protx, sum(ok) ok, sum(attempts) a FROM mn_daily WHERE day >= ?1 GROUP BY protx) u7
                    ON u7.protx = m.protx
                 LEFT JOIN (SELECT protx, sum(ok) ok, sum(attempts) a FROM mn_daily WHERE day >= ?2 GROUP BY protx) u30
                    ON u30.protx = m.protx
                 WHERE m.removed IS NULL ORDER BY m.registered_height, m.protx"
            );
            let mut st = c.prepare(&sql)?;
            let list: Vec<Value> = st
                .query_map([day_of(now - 6 * 86_400), day_of(now - 29 * 86_400)], |r| {
                    let mut v = mn_json(r, latest)?;
                    v["uptime_7d"] = uptime(r.get("ok7")?, r.get("a7")?);
                    v["uptime_30d"] = uptime(r.get("ok30")?, r.get("a30")?);
                    Ok(v)
                })?
                .collect::<rusqlite::Result<_>>()?;
            let tip: Option<i64> = c.query_row("SELECT max(height) FROM blocks", [], |r| r.get(0))?;
            Ok(json!({"crawl_id": latest, "tip": tip, "masternodes": list}))
        })
        .await?;
    Ok(Json(body))
}

fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn masternode(State(nets): State<Nets>, UrlPath((net, protx)): UrlPath<(String, String)>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    if !valid_hash(&protx) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "protx must be 64 hex characters".into()));
    }
    let protx = protx.to_ascii_lowercase();
    let range = payments_range(&ctx).await?;
    let body = ctx
        .db
        .read(move |c| {
            let latest = latest_crawl(c)?;
            let sql = format!(
                "SELECT {MN_COLUMNS}, m.collateral, m.collateral_address, m.platform_p2p, m.platform_https,
                    m.operator_payout_address, m.owner_address, m.voting_address, m.pubkey_operator,
                    m.consecutive_payments, m.pose_revived_height, m.revocation_reason, m.first_seen, m.removed,
                    n.services
                 FROM masternodes m LEFT JOIN nodes n ON n.addr = m.service WHERE m.protx = ?1"
            );
            let Some(mut mn) = c
                .query_row(&sql, [&protx], |r| {
                    let mut v = mn_json(r, latest)?;
                    for k in [
                        "collateral", "collateral_address", "platform_p2p", "platform_https",
                        "operator_payout_address", "owner_address", "voting_address", "pubkey_operator",
                    ] {
                        v[k] = json!(r.get::<_, Option<String>>(k)?);
                    }
                    for k in ["consecutive_payments", "pose_revived_height", "revocation_reason", "first_seen", "removed", "services"] {
                        v[k] = json!(r.get::<_, Option<i64>>(k)?);
                    }
                    Ok(v)
                })
                .optional()?
            else {
                return Ok(None);
            };
            let versions: Vec<Value> = c
                .prepare("SELECT ts, user_agent, protocol FROM mn_versions WHERE protx = ?1 ORDER BY ts DESC LIMIT 100")?
                .query_map([&protx], |r| {
                    Ok(json!({"ts": r.get::<_, i64>(0)?, "user_agent": r.get::<_, String>(1)?, "protocol": r.get::<_, i64>(2)?}))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let uptime_daily: Vec<Value> = c
                .prepare("SELECT day, attempts, ok FROM mn_daily WHERE protx = ?1 AND day >= ?2 ORDER BY day")?
                .query_map(params![&protx, day_of(db::now() - 89 * 86_400)], |r| {
                    Ok(json!({"day": r.get::<_, String>(0)?, "attempts": r.get::<_, i64>(1)?, "ok": r.get::<_, i64>(2)?}))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let (count, total, first): (i64, Option<i64>, Option<i64>) = c.query_row(
                "SELECT count(*), sum(amount), min(height) FROM payments WHERE protx = ?1",
                [&protx],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            mn["payments_summary"] = json!({
                "count": count, "total_duffs": total.unwrap_or(0), "first_height": first,
                "indexed": range.map(|(lo, hi)| json!({"from": lo, "to": hi})),
            });
            mn["payments"] = json!(recent_payments(c, &protx, None, 25)?);
            mn["versions"] = json!(versions);
            mn["uptime_daily"] = json!(uptime_daily);
            Ok(Some(mn))
        })
        .await?;
    match body {
        Some(b) => Ok(Json(b)),
        None => Err(ApiError(StatusCode::NOT_FOUND, "unknown protx".into())),
    }
}

fn recent_payments(c: &Connection, protx: &str, before: Option<i64>, limit: i64) -> Result<Vec<Value>> {
    let mut st = c.prepare(
        "SELECT p.height, p.amount, b.time, b.hash FROM payments p JOIN blocks b ON b.height = p.height
         WHERE p.protx = ?1 AND p.height < ?2 ORDER BY p.height DESC LIMIT ?3",
    )?;
    let rows = st
        .query_map(params![protx, before.unwrap_or(i64::MAX), limit], |r| {
            Ok(json!({
                "height": r.get::<_, i64>(0)?, "amount_duffs": r.get::<_, i64>(1)?,
                "time": r.get::<_, i64>(2)?, "block_hash": r.get::<_, String>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

#[derive(Deserialize)]
struct PaymentsQuery {
    before: Option<i64>,
    limit: Option<i64>,
}

async fn payments(
    State(nets): State<Nets>,
    UrlPath((net, protx)): UrlPath<(String, String)>,
    Query(q): Query<PaymentsQuery>,
) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    if !valid_hash(&protx) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "protx must be 64 hex characters".into()));
    }
    let protx = protx.to_ascii_lowercase();
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let rows = ctx.db.read(move |c| recent_payments(c, &protx, q.before, limit)).await?;
    Ok(Json(json!({"payments": rows})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_start_inverts_day_of() {
        for ts in [0i64, 951_782_400, 1_793_318_400, 4_102_444_800] {
            assert_eq!(day_start(&day_of(ts)), ts);
        }
    }

    #[test]
    fn top_series_folds_other() {
        let points = vec![(0, "a".into(), 5.0), (0, "b".into(), 3.0), (1, "c".into(), 1.0), (1, "a".into(), 4.0)];
        let v = top_series(vec![10, 20], points, 2, true);
        assert_eq!(v["series"][0]["key"], "a");
        assert_eq!(v["series"][0]["values"], json!([5.0, 4.0]));
        assert_eq!(v["series"][2]["key"], "Other");
        assert_eq!(v["series"][2]["values"], json!([0.0, 1.0]));
    }
}
