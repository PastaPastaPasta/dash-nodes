//! Public JSON API and static pages.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::addr::net_of;
use crate::collector::{payments_range, pose_status};
use crate::crawler::network_stats;
use crate::db::{self, day_of, get_meta, query_all};
use crate::network::Network;
use crate::readiness::{self, Handshake, Input, Readiness, Tally, evaluate};
use crate::rewards;
use crate::state::NetCtx;

pub type Nets = Arc<HashMap<Network, Arc<NetCtx>>>;

pub fn router(nets: Nets, web: &Path) -> Router {
    let not_found: &'static str = std::fs::read_to_string(web.join("404.html")).unwrap_or_default().leak();
    let api = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/{net}/summary", get(summary))
        .route("/api/v1/{net}/history", get(history))
        .route("/api/v1/{net}/geo", get(geo))
        .route("/api/v1/{net}/masternodes", get(masternodes))
        .route("/api/v1/{net}/masternodes/{protx}", get(masternode))
        .route("/api/v1/{net}/masternodes/{protx}/payments", get(payments))
        .route("/api/v1/{net}/changes", get(changes))
        .route("/api/v1/{net}/platform", get(platform_rewards))
        .route("/api/v1/{net}/reference", get(reference))
        .with_state(nets)
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=60"),
        ))
        .layer(SetResponseHeaderLayer::overriding(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*")));
    let page = |name: &str| ServeFile::new(web.join(name));
    let pages = Router::new()
        .route_service("/", page("index.html"))
        .route_service("/masternodes", page("masternodes.html"))
        .route_service("/changes", page("changes.html"))
        .route_service("/map", page("map.html"))
        .route_service("/platform", page("platform.html"))
        .route_service("/mn/{protx}", page("mn.html"))
        .route_service("/api", page("api.html"))
        .nest_service("/assets", ServeDir::new(web.join("assets")))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=300"),
        ));
    let api = api.layer(middleware::from_fn_with_state(ResponseCache::default(), cache_responses));
    Router::new()
        .merge(api)
        .merge(pages)
        .route("/healthz", get(|| async { "ok" }))
        .fallback(move || async move { (StatusCode::NOT_FOUND, Html(not_found)) })
        .layer(middleware::map_response(harden))
        .layer(CompressionLayer::new())
}

// Everything, including the map's basemap, fonts and libraries, is served from this origin.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
    connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

async fn harden(mut r: Response) -> Response {
    let ok = r.status().is_success();
    let h = r.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    // Errors (including axum's own extractor rejections) must not be cached by Cloudflare.
    if !ok {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    r
}

/// Successful API GETs, kept for a few seconds so bursts (or a cold Cloudflare cache) cost one
/// query each. Keyed by path and query.
#[derive(Clone, Default)]
struct ResponseCache(Arc<Mutex<HashMap<String, CachedResponse>>>);

type CachedResponse = (Instant, HeaderMap, Bytes);

const CACHE_TTL: Duration = Duration::from_secs(20);
const CACHE_ENTRIES: usize = 512;

async fn cache_responses(State(cache): State<ResponseCache>, req: Request, next: Next) -> Response {
    let key = req.uri().to_string();
    if let Some((at, headers, body)) = cache.0.lock().unwrap().get(&key).cloned()
        && at.elapsed() < CACHE_TTL
    {
        let mut r = Response::new(Body::from(body));
        *r.headers_mut() = headers;
        return r;
    }
    let r = next.run(req).await;
    if r.status() != StatusCode::OK {
        return r;
    }
    let (parts, body) = r.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 32 << 20).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut map = cache.0.lock().unwrap();
    if map.len() >= CACHE_ENTRIES {
        map.retain(|_, (at, _, _)| at.elapsed() < CACHE_TTL);
        if map.len() >= CACHE_ENTRIES {
            map.clear();
        }
    }
    map.insert(key, (Instant::now(), parts.headers.clone(), bytes.clone()));
    Response::from_parts(parts, Body::from(bytes))
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

fn indexed_json(range: Option<(u32, u32)>) -> Value {
    range.map_or(Value::Null, |(lo, hi)| json!({"from": lo, "to": hi}))
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
                "payments_indexed": indexed_json(range),
                "geo_loaded": ctx.geo.loaded(),
            }),
        );
    }
    // Where the crawler runs, for people ("United States (New York)"); from the deployment.
    let location = std::env::var("CRAWLER_LOCATION").ok().filter(|l| !l.is_empty());
    Ok(Json(json!({
        "networks": out, "snapshots": crate::snapshot::status(), "crawler_location": location,
        "version": env!("CARGO_PKG_VERSION"),
    })))
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

/// Latest crawl and every distribution from it: `dists[scope][dim] = [[key, count], ...]`, largest
/// first, plus per address network counts (`networks`, see crawler::network_stats).
async fn summary(State(nets): State<Nets>, UrlPath(net): UrlPath<String>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let body = ctx
        .db
        .read(|c| {
            // One snapshot, so a crawl recorded meanwhile can't mix into this answer.
            let c = &c.unchecked_transaction()?;
            let Some(id) = latest_crawl(c)? else {
                return Ok(json!({"crawl": null, "dists": {}, "networks": network_stats(c, 0, db::now(), None)?}));
            };
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
            let stored: Option<String> =
                c.query_row("SELECT networks FROM crawls WHERE id = ?1", [id], |r| r.get(0))?;
            let networks = match stored.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                Some(v) => v,
                // Crawls from before per-network stats were recorded.
                None => json!(network_stats(c, id, started, None)?),
            };
            let returns = network_returns(c)?;
            let platform = platform_summary(c)?;
            let dkg_readiness = readiness_summary(c, id)?;
            Ok(json!({
                "crawl": crawl, "day_ago": day_ago, "dists": dists, "networks": networks, "returns": returns,
                "platform": platform, "dkg_readiness": dkg_readiness,
            }))
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
    const DIMS: &[&str] =
        &["totals", "ua", "version", "client", "protocol", "country", "asn", "network", "reachable", "status", "type"];
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
                // Today isn't rolled up yet: average its crawls so far, so the chart reaches today.
                let today = db::now() - db::now() % 86_400;
                let crawls_today: i64 = c.query_row(
                    "SELECT count(*) FROM crawls WHERE finished IS NOT NULL AND started >= ?1",
                    [today],
                    |r| r.get(0),
                )?;
                if crawls_today > 0 && !index.contains_key(&day_of(today)) {
                    let i = t.len();
                    t.push(today);
                    let mut st = c.prepare(
                        "SELECT key, CAST(sum(count) AS REAL) / ?4 FROM crawl_agg WHERE scope = ?1 AND dim = ?2
                         AND crawl_id IN (SELECT id FROM crawls WHERE finished IS NOT NULL AND started >= ?3) GROUP BY key",
                    )?;
                    let mut rows = st.query(params![scope, dim, today, crawls_today])?;
                    while let Some(r) = rows.next()? {
                        points.push((i, r.get(0)?, r.get(1)?));
                    }
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
    n.last_error, n.start_height, n.continent, n.city, n.lat, n.lon, n.services, n.relay, n.fail_streak,
    m.platform_banned, m.removed";

/// A 0/1 column as a bool.
fn flag(r: &Row, col: &str) -> rusqlite::Result<Option<bool>> {
    Ok(r.get::<_, Option<i64>>(col)?.map(|x| x != 0))
}

/// Whether the row's address answered crawl `latest` (None: not checked in it).
fn reachable_in(r: &Row, latest: Option<i64>) -> rusqlite::Result<Option<bool>> {
    match (r.get::<_, Option<i64>>("last_crawl")?, latest) {
        (Some(a), Some(b)) if a == b => Ok(Some(r.get::<_, i64>("last_ok")? == 1)),
        _ => Ok(None),
    }
}

/// DKG readiness (see readiness.rs) from a row with MN_COLUMNS and its latest port check.
fn mn_readiness(r: &Row, reachable: Option<bool>, now: i64) -> rusqlite::Result<Readiness> {
    // Like the crawler's per-crawl counts, trust a handshake from the past week only; older, we don't
    // know what it runs now.
    let recent = r.get::<_, Option<i64>>("last_success")?.is_some_and(|t| now - t < crate::crawler::ACTIVE_WINDOW);
    let handshake = if recent {
        Some(Handshake { protocol: r.get("protocol")?, services: r.get("services")?, relay: flag(r, "relay")? })
    } else {
        None
    };
    Ok(evaluate(&Input {
        handshake,
        reachable,
        fail_streak: r.get::<_, Option<i64>>("fail_streak")?.unwrap_or(0),
        evo: r.get::<_, String>("type")? == "Evo",
        platform_banned: flag(r, "platform_banned")?,
    }))
}

fn mn_json(r: &Row, latest: Option<i64>) -> rusqlite::Result<Value> {
    let ban: Option<i64> = r.get("pose_ban_height")?;
    let reachable = reachable_in(r, latest)?;
    let ua: Option<String> = r.get("user_agent")?;
    let service: Option<String> = r.get("service")?;
    Ok(json!({
        "protx": r.get::<_, String>("protx")?,
        "type": r.get::<_, String>("type")?,
        "status": pose_status(ban),
        "network": service.as_deref().and_then(net_of),
        "service": service,
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
        "continent": r.get::<_, Option<String>>("continent")?,
        "city": r.get::<_, Option<String>>("city")?,
        "lat": r.get::<_, Option<f64>>("lat")?,
        "lon": r.get::<_, Option<f64>>("lon")?,
        // A removed masternode isn't in any quorum; its old flags say nothing.
        "dkg_readiness": match r.get::<_, Option<i64>>("removed")? {
            Some(_) => Value::Null,
            None => mn_readiness(r, reachable, db::now())?.json(),
        },
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
    let view = rewards::view(&ctx);
    let body = ctx
        .db
        .read(move |c| {
            let latest = latest_crawl(c)?;
            let now = db::now();
            let mut platform = rewards::list_fields(c, &view)?;
            let sql = format!(
                "SELECT {MN_COLUMNS}, u7.ok AS ok7, u7.a AS a7, u30.ok AS ok30, u30.a AS a30
                 FROM masternodes m LEFT JOIN nodes n ON n.addr = m.service AND m.removed IS NULL
                 LEFT JOIN (SELECT protx, sum(ok) ok, sum(attempts) a FROM mn_daily WHERE day >= ?1 GROUP BY protx) u7
                    ON u7.protx = m.protx
                 LEFT JOIN (SELECT protx, sum(ok) ok, sum(attempts) a FROM mn_daily WHERE day >= ?2 GROUP BY protx) u30
                    ON u30.protx = m.protx
                 WHERE m.removed IS NULL ORDER BY m.registered_height, m.protx"
            );
            let list: Vec<Value> = query_all(c, &sql, [day_of(now - 6 * 86_400), day_of(now - 29 * 86_400)], |r| {
                let mut v = mn_json(r, latest)?;
                v["uptime_7d"] = uptime(r.get("ok7")?, r.get("a7")?);
                v["uptime_30d"] = uptime(r.get("ok30")?, r.get("a30")?);
                if let Some(p) = v["protx"].as_str().and_then(|p| platform.remove(p)) {
                    v["platform"] = p;
                }
                Ok(v)
            })?;
            let tip: Option<i64> = c.query_row("SELECT max(height) FROM blocks", [], |r| r.get(0))?;
            Ok(json!({"crawl_id": latest, "tip": tip, "masternodes": list}))
        })
        .await?;
    Ok(Json(body))
}

/// Validates a ProTx hash from the URL and lowercases it, the form it's stored in.
fn parse_protx(s: &str) -> Result<String, ApiError> {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err(ApiError(StatusCode::BAD_REQUEST, "protx must be 64 hex characters".into()))
    }
}

async fn masternode(State(nets): State<Nets>, UrlPath((net, protx)): UrlPath<(String, String)>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let protx = parse_protx(&protx)?;
    let range = payments_range(&ctx).await?;
    let view = rewards::view(&ctx);
    let body = ctx
        .db
        .read(move |c| {
            let latest = latest_crawl(c)?;
            let sql = format!(
                "SELECT {MN_COLUMNS}, m.core_p2p, m.collateral, m.collateral_address, m.platform_p2p, m.platform_https,
                    m.operator_payout_address, m.owner_address, m.voting_address, m.pubkey_operator,
                    m.consecutive_payments, m.pose_revived_height, m.revocation_reason, m.first_seen
                 FROM masternodes m
                 -- A removed masternode's old address may belong to someone else now: don't show its live data.
                 LEFT JOIN nodes n ON n.addr = m.service AND m.removed IS NULL
                 WHERE m.protx = ?1"
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
                    v["relay"] = json!(flag(r, "relay")?);
                    v["platform_banned"] = json!(flag(r, "platform_banned")?);
                    // Every core P2P address (v24 ProTx can list several, on any network), each with its network.
                    let core_p2p: Vec<String> = r
                        .get::<_, Option<String>>("core_p2p")?
                        .and_then(|j| serde_json::from_str(&j).ok())
                        .unwrap_or_else(|| v["service"].as_str().map(|s| vec![s.to_owned()]).unwrap_or_default());
                    v["core_p2p"] = core_p2p.iter().map(|a| json!({"address": a, "network": net_of(a)})).collect();
                    Ok(v)
                })
                .optional()?
            else {
                return Ok(None);
            };
            let versions: Vec<Value> = query_all(
                c,
                "SELECT ts, user_agent, protocol FROM mn_versions WHERE protx = ?1 ORDER BY ts DESC LIMIT 100",
                [&protx],
                |r| Ok(json!({"ts": r.get::<_, i64>(0)?, "user_agent": r.get::<_, String>(1)?, "protocol": r.get::<_, i64>(2)?})),
            )?;
            let uptime_daily: Vec<Value> = query_all(
                c,
                "SELECT day, attempts, ok FROM mn_daily WHERE protx = ?1 AND day >= ?2 ORDER BY day",
                params![&protx, day_of(db::now() - 89 * 86_400)],
                |r| Ok(json!({"day": r.get::<_, String>(0)?, "attempts": r.get::<_, i64>(1)?, "ok": r.get::<_, i64>(2)?})),
            )?;
            let (count, total, first): (i64, Option<i64>, Option<i64>) = c.query_row(
                "SELECT count(*), sum(amount), min(height) FROM payments WHERE protx = ?1",
                [&protx],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            mn["payments_summary"] = json!({
                "count": count, "total_duffs": total.unwrap_or(0), "first_height": first,
                "indexed": indexed_json(range),
            });
            mn["payments"] = json!(recent_payments(c, &protx, None, 25)?);
            let evo = mn["type"].as_str() == Some("Evo");
            mn["returns"] = mn_returns(c, &protx, evo, mn["removed"].as_i64().is_some())?;
            if evo {
                let latest = c
                    .query_row("SELECT * FROM platform_status WHERE protx = ?1", [&protx], crate::platform::status_json)
                    .optional()?;
                let daily: Vec<Value> = query_all(
                    c,
                    "SELECT day, attempts, ok FROM platform_daily WHERE protx = ?1 AND day >= ?2 ORDER BY day",
                    params![&protx, day_of(db::now() - 29 * 86_400)],
                    |r| Ok(json!({"day": r.get::<_, String>(0)?, "attempts": r.get::<_, i64>(1)?, "ok": r.get::<_, i64>(2)?})),
                )?;
                mn["platform"] = json!({"latest": latest, "daily": daily});
                let window = mn["returns"]["window_days"].as_i64().unwrap_or(RETURN_WINDOW);
                mn["platform_rewards"] = rewards::mn_json(c, &protx, &view, window)?;
            }
            let changes: Vec<Value> = query_all(
                c,
                "SELECT ts, height, field, old, new FROM mn_changes WHERE protx = ?1 ORDER BY id DESC LIMIT 50",
                [&protx],
                change_json,
            )?;
            mn["changes"] = json!(changes);
            // The thresholds, so the page can quote them.
            if let Some(d) = mn["dkg_readiness"].as_object_mut() {
                d.insert("rules".into(), readiness::rules_json());
            }
            mn["versions"] = json!(versions);
            mn["uptime_daily"] = json!(uptime_daily);
            Ok(Some(mn))
        })
        .await?;
    body.map(Json).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown protx".into()))
}

// ---- realized returns ----------------------------------------------------------------------------
//
// What masternodes actually earned, from the payments index: everything a block paid a masternode's
// addresses (owner and operator shares; not the platform share, which goes to Platform's credit pool),
// against its collateral. The yield is
// annualized over the last RETURN_WINDOW days, or since registration for a newer masternode.

const RETURN_WINDOW: i64 = 90;
const DAY: i64 = 86_400;
const COIN: f64 = 1e8;

fn collateral_duffs(evo: bool) -> f64 {
    if evo { 4_000.0 * COIN } else { 1_000.0 * COIN }
}

/// `paid` duffs over `days` days, as a yearly share of the collateral.
fn annualized(paid: i64, days: i64, evo: bool) -> f64 {
    paid as f64 / days as f64 * 365.0 / collateral_duffs(evo)
}

/// The first indexed block at or after `ts` (None: nothing indexed that late).
fn height_at(c: &Connection, ts: i64) -> Result<Option<i64>> {
    // Written so SQLite walks the blocks_time index (a min() over a range would scan by height).
    Ok(c.query_row("SELECT height FROM blocks WHERE time >= ?1 ORDER BY time, height LIMIT 1", [ts], |r| r.get(0))
        .optional()?)
}

/// When the payments index starts (the time of its lowest block), if it has any.
fn index_start(c: &Connection) -> Result<Option<i64>> {
    Ok(c.query_row("SELECT time FROM blocks ORDER BY height LIMIT 1", [], |r| r.get(0)).optional()?)
}

/// What `protx` was paid from height `from` on.
fn paid_since(c: &Connection, protx: &str, from: Option<i64>) -> Result<i64> {
    let Some(from) = from else { return Ok(0) };
    Ok(c.query_row(
        "SELECT coalesce(sum(amount), 0) FROM payments WHERE protx = ?1 AND height >= ?2",
        params![protx, from],
        |r| r.get(0),
    )?)
}

/// Paid in the last 30, 90 and 365 days (as far as the payments index goes back), and the annualized
/// yield over the last 90 days, or since registration or the start of the index if later (none under
/// two weeks: too few payments to say).
fn mn_returns(c: &Connection, protx: &str, evo: bool, removed: bool) -> Result<Value> {
    let now = db::now();
    let paid = |days: i64| -> Result<i64> { paid_since(c, protx, height_at(c, now - days * DAY)?) };
    let registered: Option<i64> = c
        .query_row(
            "SELECT b.time FROM masternodes m JOIN blocks b ON b.height = m.registered_height WHERE m.protx = ?1",
            [protx],
            |r| r.get(0),
        )
        .optional()?;
    let indexed_since = index_start(c)?;
    let from = [Some(now - RETURN_WINDOW * DAY), registered, indexed_since].into_iter().flatten().max().unwrap_or(now);
    // Whole days, and payments summed over exactly those days.
    let window = (now - from) / DAY;
    // A masternode that left the list stopped being paid then: a yield up to now would mean nothing.
    let annual_yield = (window >= 14 && !removed)
        .then(|| -> Result<i64> { paid_since(c, protx, height_at(c, now - window * DAY)?) })
        .transpose()?
        .map(|p| round4(annualized(p, window, evo)));
    Ok(json!({
        "paid_30d": paid(30)?, "paid_90d": paid(RETURN_WINDOW)?, "paid_365d": paid(365)?,
        "window_days": window, "annual_yield": annual_yield, "indexed_since": indexed_since,
    }))
}

/// `x` rounded to `places` decimals.
pub(crate) fn round_to(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

pub(crate) fn round4(x: f64) -> f64 {
    round_to(x, 4)
}

/// The median of `ys` (the upper one of two middles), to four decimals; None when there are none.
pub(crate) fn median4(mut ys: Vec<f64>) -> Option<f64> {
    ys.sort_by(f64::total_cmp);
    (!ys.is_empty()).then(|| round4(ys[ys.len() / 2]))
}

/// The median annualized yield of enabled masternodes registered for the whole window, by type, and
/// how many that is. None until the payments index covers the window.
fn network_returns(c: &Connection) -> Result<Value> {
    let since = db::now() - RETURN_WINDOW * DAY;
    let covered = index_start(c)?.is_some_and(|t| t <= since);
    let from = if covered { height_at(c, since)? } else { None };
    let rows: Vec<(String, i64, String)> = match from {
        None => Vec::new(),
        Some(from) => query_all(
            c,
            // A registration with no indexed block predates the index (and so the window).
            "SELECT m.type, coalesce((SELECT sum(p.amount) FROM payments p WHERE p.protx = m.protx AND p.height >= ?2), 0), m.protx
             FROM masternodes m LEFT JOIN blocks rb ON rb.height = m.registered_height
             WHERE m.removed IS NULL AND (m.pose_ban_height IS NULL OR m.pose_ban_height <= 0)
               AND (rb.time < ?1 OR (rb.time IS NULL AND m.registered_height < (SELECT min(height) FROM blocks)))",
            params![since, from],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?,
    };
    let core = |evo: bool| {
        rows.iter()
            .filter(move |(t, _, _)| (t == "Evo") == evo)
            .map(move |(_, paid, p)| (p, annualized(*paid, RETURN_WINDOW, evo)))
    };
    // Evonodes also earn Platform rewards: annualized over the finalized epochs in the same window.
    let (platform, span) = rewards::platform_yields(c, since)?;
    let with_platform = span > 0.0;
    let regular: Vec<f64> = core(false).map(|(_, y)| y).collect();
    // (Core yield, Platform yield) of each evonode.
    let evo: Vec<(f64, f64)> = core(true).map(|(p, y)| (y, platform.get(p).copied().unwrap_or(0.0))).collect();
    Ok(json!({
        "window_days": RETURN_WINDOW,
        "regular": {"masternodes": regular.len(), "median_annual_yield": median4(regular)},
        "evo": {
            "masternodes": evo.len(), "median_annual_yield": median4(evo.iter().map(|y| y.0).collect()),
            "median_platform_yield": if with_platform { median4(evo.iter().map(|y| y.1).collect()) } else { None },
            "median_total_yield": if with_platform { median4(evo.iter().map(|y| y.0 + y.1).collect()) } else { None },
            "platform_span_days": with_platform.then(|| round_to(span, 1)),
        },
    }))
}

/// Evonode Platform health across the network, from the latest checks (see platform.rs): how many
/// answered on each endpoint, which software they run, and certificates about to expire.
fn platform_summary(c: &Connection) -> Result<Value> {
    // Only evonodes still registered and not banned; a check older than a day is stale (the checker stopped).
    const LIVE: &str = "FROM platform_status s JOIN masternodes m ON m.protx = s.protx
        WHERE m.removed IS NULL AND m.type = 'Evo' AND (m.pose_ban_height IS NULL OR m.pose_ban_height <= 0)
          AND s.ts > ?1";
    let since = db::now() - 86_400;
    let (checked, p2p_ok, https_ok, healthy, expiring, height, ts): (i64, i64, i64, i64, i64, Option<i64>, Option<i64>) =
        c.query_row(
            &format!(
                "SELECT count(*), coalesce(sum(s.p2p_error IS NULL), 0), coalesce(sum(s.https_error IS NULL), 0),
                    coalesce(sum(s.p2p_error IS NULL AND s.https_error IS NULL AND (s.node_protx IS NULL OR s.node_protx = s.protx)), 0),
                    coalesce(sum(s.cert_expires IS NOT NULL AND s.cert_expires < ?2), 0), max(s.height), max(s.ts) {LIVE}"
            ),
            params![since, db::now() + 3 * 86_400],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )?;
    let dist = |col: &str| -> Result<Value> {
        let rows: Vec<(String, i64)> = query_all(
            c,
            &format!("SELECT coalesce(s.{col}, 'Unknown'), count(*) {LIVE} GROUP BY 1 ORDER BY 2 DESC"),
            [since],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(json!(rows))
    };
    Ok(json!({
        "checked": checked, "p2p_ok": p2p_ok, "https_ok": https_ok, "healthy": healthy,
        "cert_expiring_3d": expiring, "max_height": height, "updated": ts,
        "drive": dist("drive")?, "tenderdash": dist("tenderdash")?, "dapi": dist("dapi")?,
    }))
}

/// DKG readiness across every enabled masternode (see readiness.rs), as of the latest crawl `id`.
fn readiness_summary(c: &Connection, id: i64) -> Result<Value> {
    let mut tally = Tally::default();
    let now = db::now();
    let mut st = c.prepare(&format!(
        "SELECT {MN_COLUMNS} FROM masternodes m LEFT JOIN nodes n ON n.addr = m.service
         WHERE m.removed IS NULL AND coalesce(m.pose_ban_height, -1) = -1"
    ))?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        tally.add(&mn_readiness(r, reachable_in(r, Some(id))?, now)?);
    }
    Ok(tally.to_json())
}

fn change_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "ts": r.get::<_, i64>("ts")?, "height": r.get::<_, Option<i64>>("height")?, "field": r.get::<_, String>("field")?,
        "old": r.get::<_, Option<String>>("old")?, "new": r.get::<_, Option<String>>("new")?,
    }))
}

/// Recent registration changes across the network, newest first: registrations, removals, PoSe bans
/// and revivals, and changed addresses or keys (see collector.rs).
async fn changes(State(nets): State<Nets>, UrlPath(net): UrlPath<String>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let body = ctx
        .db
        .read(|c| {
            let changes: Vec<Value> = query_all(
                c,
                "SELECT ch.ts, ch.height, ch.field, ch.old, ch.new, ch.protx, m.type
                 FROM mn_changes ch LEFT JOIN masternodes m ON m.protx = ch.protx
                 ORDER BY ch.id DESC LIMIT 300",
                [],
                |r| {
                    let mut v = change_json(r)?;
                    v["protx"] = json!(r.get::<_, String>("protx")?);
                    v["type"] = json!(r.get::<_, Option<String>>("type")?);
                    Ok(v)
                },
            )?;
            let since = get_meta(c, "changes_since")?.and_then(|v| v.parse::<i64>().ok());
            Ok(json!({"since": since, "changes": changes}))
        })
        .await?;
    Ok(Json(body))
}

/// Platform rewards across the network (see rewards.rs): epochs, the upgrade vote, the proposer
/// schedule and one row per evonode.
async fn platform_rewards(State(nets): State<Nets>, UrlPath(net): UrlPath<String>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let view = rewards::view(&ctx);
    let network = ctx.net;
    let body = ctx.db.read(move |c| rewards::network_json(c, &view, network)).await?;
    Ok(Json(body))
}

/// Where the latest crawl's reachable nodes are, by city coordinates, without their addresses.
/// Onion and I2P nodes (and any IP without a location) are only counted in `unlocated`.
async fn geo(State(nets): State<Nets>, UrlPath(net): UrlPath<String>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let body = ctx
        .db
        .read(|c| {
            let Some(id) = latest_crawl(c)? else {
                return Ok(json!({"crawl_id": null, "points": [], "unlocated": 0}));
            };
            let points: Vec<Value> = query_all(
                c,
                "SELECT lat, lon, max(city), max(country), count(*), sum(addr IN (SELECT addr FROM mn_addrs))
                 FROM nodes WHERE last_crawl = ?1 AND last_ok = 1 AND lat IS NOT NULL AND lon IS NOT NULL
                 GROUP BY lat, lon ORDER BY count(*) DESC, lat, lon",
                [id],
                |r| {
                    Ok(json!({
                        "lat": r.get::<_, f64>(0)?, "lon": r.get::<_, f64>(1)?,
                        "city": r.get::<_, Option<String>>(2)?, "country": r.get::<_, Option<String>>(3)?,
                        "nodes": r.get::<_, i64>(4)?, "masternodes": r.get::<_, i64>(5)?,
                    }))
                },
            )?;
            let unlocated: i64 = c.query_row(
                "SELECT count(*) FROM nodes WHERE last_crawl = ?1 AND last_ok = 1 AND (lat IS NULL OR lon IS NULL)",
                [id],
                |r| r.get(0),
            )?;
            Ok(json!({"crawl_id": id, "points": points, "unlocated": unlocated}))
        })
        .await?;
    Ok(Json(body))
}

fn recent_payments(c: &Connection, protx: &str, before: Option<i64>, limit: i64) -> Result<Vec<Value>> {
    Ok(query_all(
        c,
        "SELECT p.height, p.amount, b.time, b.hash FROM payments p JOIN blocks b ON b.height = p.height
         WHERE p.protx = ?1 AND p.height < ?2 ORDER BY p.height DESC LIMIT ?3",
        params![protx, before.unwrap_or(i64::MAX), limit],
        |r| {
            Ok(json!({
                "height": r.get::<_, i64>(0)?, "amount_duffs": r.get::<_, i64>(1)?,
                "time": r.get::<_, i64>(2)?, "block_hash": r.get::<_, String>(3)?,
            }))
        },
    )?)
}

/// Block hashes for the bench to check headers against (see reference.rs).
async fn reference(State(nets): State<Nets>, UrlPath(net): UrlPath<String>) -> ApiResult {
    let ctx = net_ctx(&nets, &net)?;
    let r = ctx.reference.read().unwrap().clone();
    let r = r.ok_or_else(|| ApiError(StatusCode::SERVICE_UNAVAILABLE, "reference not ready".into()))?;
    Ok(Json(json!(r)))
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
    let protx = parse_protx(&protx)?;
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
    fn evonode_returns_add_platform_rewards() {
        let dir = std::env::temp_dir().join(format!("dash-nodes-returns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.sqlite");
        let _ = std::fs::remove_file(&path);
        drop(db::Db::open(&path).unwrap());
        let c = Connection::open(&path).unwrap();
        let (now, day) = (db::now(), DAY);
        // Three evonodes and a regular masternode, all registered before the 90-day window.
        c.execute_batch(&format!(
            "INSERT INTO blocks VALUES (1, 'a', '', {}, 0), (2, 'b', '', {}, 0), (3, 'c', '', {}, 0);
             INSERT INTO masternodes (protx, type, registered_height, first_seen, updated) VALUES
                ('a', 'Evo', 1, 0, 0), ('b', 'Evo', 1, 0, 0), ('c', 'Evo', 1, 0, 0), ('r', 'Regular', 1, 0, 0);
             INSERT INTO payments VALUES (2, 'a', 1000000000), (3, 'c', 2000000000), (3, 'r', 100000000);
             -- Two finished epochs in the window, 60 days in all; the current one started now.
             INSERT INTO platform_epochs VALUES (10, 1, {}, 0, 0, 10, 0, 0, 0, 13, 2), (11, 2, {}, 0, 0, 10, 0, 0, 0, 13, 2);
             INSERT INTO platform_payouts VALUES (10, 'a', 5, 2000000000000), (11, 'a', 5, 2000000000000), (11, 'c', 5, 8000000000000);
             INSERT INTO meta VALUES ('platform_now', '{{\"epoch\": 12, \"start\": {now}}}');",
            now - 100 * day,
            now - 80 * day,
            now - day,
            now - 60 * day,
            now - 30 * day,
        ))
        .unwrap();
        let r = network_returns(&c).unwrap();
        // Core: 10 and 20 DASH in 90 days; Platform: 40 and 80 DASH over 60 days, on 4,000 DASH.
        assert_eq!(r["evo"]["masternodes"], 3);
        assert_eq!(r["evo"]["median_annual_yield"], json!(round4(10.0 / 90.0 * 365.0 / 4000.0)));
        assert_eq!(r["evo"]["median_platform_yield"], json!(round4(40.0 / 60.0 * 365.0 / 4000.0)));
        assert_eq!(r["evo"]["median_total_yield"], json!(round4((10.0 / 90.0 + 40.0 / 60.0) * 365.0 / 4000.0)));
        assert_eq!(r["evo"]["platform_span_days"], json!(60.0));
        assert_eq!(r["regular"]["masternodes"], 1);
        drop(c);
        std::fs::remove_dir_all(&dir).unwrap();
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
