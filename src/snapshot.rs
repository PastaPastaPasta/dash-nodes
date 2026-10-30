//! Snapshots of the state a host can't rebuild (the crawler's SQLite databases, a vantage's
//! speed-test results), kept in the Worker's private state bucket so that the host itself holds
//! nothing that matters: a new host restores them on first start, everything else it re-derives.
//!
//! A snapshot is a consistent copy of the database (`VACUUM INTO`), gzip-compressed and cut into
//! parts of at most PART bytes (`<name>/<stamp>/<nnn>`), then a manifest (`<name>/latest.json`)
//! naming the parts, the SHA-256 of their concatenation and the snapshot it descends from. The
//! Worker only accepts a manifest whose parts are all there and whose parent is the current latest,
//! and keeps the one before it (`previous.json`).
//!
//! The parent rule is what keeps history safe: next to each database, `<db>.snapshot-stamp` names
//! the snapshot it was restored from or last saved as. A database started empty while snapshots
//! exist has none, so its uploads are refused instead of replacing the history; so are those of a
//! second host once the first has moved on.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Largest part uploaded in one request (the Worker's request body limit is 100 MB).
const PART: usize = 64 << 20;
const ATTEMPTS: u32 = 4;

#[derive(Serialize, Deserialize)]
struct Manifest {
    stamp: i64,
    parts: usize,
    /// Size and SHA-256 hex of the compressed snapshot (all parts in order).
    bytes: u64,
    sha256: String,
    /// The snapshot this one descends from (None: the first).
    #[serde(default)]
    parent: Option<i64>,
}

/// An answer that retrying won't change (a 4xx).
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// The Worker's state store, as one writer (`token`) sees it.
pub struct Remote {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Remote {
    /// `target` is the Worker's origin; state lives under `{target}/_state/`.
    pub fn new(target: &str, token: String) -> Result<Remote> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .user_agent(crate::p2p::user_agent())
            .build()?;
        Ok(Remote { client, base: format!("{}/_state", target.trim_end_matches('/')), token })
    }

    /// The object at `key`, or None if the Worker says there is no such snapshot (a 404 from
    /// anything else, a wrong target say, is an error).
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        retry(key, || async {
            let resp = self.client.get(format!("{}/{key}", self.base)).bearer_auth(&self.token).send().await?;
            let status = resp.status();
            if status.as_u16() == 404 && resp.headers().get("x-state").is_some_and(|v| v == "none") {
                return Ok(None);
            }
            if status.is_success() {
                return Ok(Some(resp.bytes().await?.to_vec()));
            }
            Err(http_error(status, resp).await)
        })
        .await
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<()> {
        retry(key, || async {
            let resp = self
                .client
                .put(format!("{}/{key}", self.base))
                .bearer_auth(&self.token)
                .body(body.clone())
                .send()
                .await?;
            let status = resp.status();
            if status.is_success() { Ok(()) } else { Err(http_error(status, resp).await) }
        })
        .await
    }

    /// Uploads a snapshot of the SQLite database at `db` as `name`.
    pub async fn save(&self, name: &str, db: &Path) -> Result<()> {
        let started = std::time::Instant::now();
        let parent = read_stamp(db);
        let (db2, gz) = (db.to_owned(), tmp_path(db, "snapshot.gz"));
        let gz2 = gz.clone();
        let compressed = tokio::task::spawn_blocking(move || compress_copy(&db2, &gz2)).await?;
        let result = async {
            let (bytes, sha256) = compressed?;
            let stamp = crate::db::now();
            let mut file = std::fs::File::open(&gz)?;
            let mut parts = 0;
            loop {
                let mut buf = Vec::with_capacity(PART);
                (&mut file).take(PART as u64).read_to_end(&mut buf)?;
                if buf.is_empty() {
                    break;
                }
                self.put(&format!("{name}/{stamp}/{parts:03}"), buf).await?;
                parts += 1;
            }
            let manifest = Manifest { stamp, parts, bytes, sha256, parent };
            self.put(&format!("{name}/latest.json"), serde_json::to_vec(&manifest)?).await?;
            write_stamp(db, stamp)?;
            Ok::<_, anyhow::Error>(bytes)
        }
        .await;
        let _ = std::fs::remove_file(&gz);
        let bytes = result?;
        tracing::info!(
            "snapshot {name}: {:.0} MB saved in {:.0} s",
            bytes as f64 / 1e6,
            started.elapsed().as_secs_f64()
        );
        Ok(())
    }

    /// Restores the latest snapshot of `name` to `dest` (which must not exist yet), or the one before
    /// it if the latest doesn't check out. Ok(false) when there is none; an error when there is one
    /// that couldn't be restored, so the caller doesn't start empty.
    pub async fn restore(&self, name: &str, dest: &Path) -> Result<bool> {
        let Some(latest) = self.get(&format!("{name}/latest.json")).await? else { return Ok(false) };
        let latest: Manifest = serde_json::from_slice(&latest).context("snapshot manifest")?;
        let first = self.restore_from(name, &latest, dest).await;
        let used = match first {
            Ok(()) => latest.stamp,
            Err(e) => {
                tracing::warn!("snapshot {name} ({}): {e:#}; trying the one before", latest.stamp);
                let prev = self.get(&format!("{name}/previous.json")).await?.ok_or(e)?;
                let prev: Manifest = serde_json::from_slice(&prev).context("snapshot manifest")?;
                self.restore_from(name, &prev, dest).await?;
                prev.stamp
            }
        };
        // Uploads from here on replace the latest, whichever of the two this came from.
        write_stamp(dest, latest.stamp)?;
        tracing::info!("snapshot {name}: restored the one from {used}");
        Ok(true)
    }

    async fn restore_from(&self, name: &str, m: &Manifest, dest: &Path) -> Result<()> {
        let gz = tmp_path(dest, "restore.gz");
        let fetched = async {
            let mut out = BufWriter::new(std::fs::File::create(&gz)?);
            let mut hash = Sha256::new();
            for i in 0..m.parts {
                let key = format!("{name}/{}/{i:03}", m.stamp);
                let part = self.get(&key).await?.ok_or_else(|| anyhow!("snapshot part {key} is missing"))?;
                hash.update(&part);
                out.write_all(&part)?;
            }
            out.flush()?;
            if hex(&hash.finalize()) != m.sha256 {
                bail!("snapshot {name} ({}) fails its checksum", m.stamp);
            }
            Ok(())
        }
        .await;
        let unpacked = match fetched {
            Ok(()) => {
                let (gz2, dest2) = (gz.clone(), dest.to_owned());
                tokio::task::spawn_blocking(move || unpack(&gz2, &dest2)).await?
            }
            Err(e) => Err(e),
        };
        let _ = std::fs::remove_file(&gz);
        unpacked
    }
}

async fn http_error(status: reqwest::StatusCode, resp: reqwest::Response) -> anyhow::Error {
    let text: String = resp.text().await.unwrap_or_default().chars().take(200).collect();
    let msg = format!("HTTP {status}: {text}");
    if status.is_client_error() { Refused(msg).into() } else { anyhow!(msg) }
}

/// Unpacks a compressed snapshot to `dest`, checks it, and only then moves it into place (along
/// with dropping any WAL or journal a previous database there left behind, which SQLite would
/// otherwise apply to the restored one).
fn unpack(gz: &Path, dest: &Path) -> Result<()> {
    let tmp = tmp_path(dest, "restoring");
    let result = (|| -> Result<()> {
        let mut src = GzDecoder::new(BufReader::new(std::fs::File::open(gz)?));
        let mut out = BufWriter::new(std::fs::File::create(&tmp)?);
        std::io::copy(&mut src, &mut out)?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        let check: String = Connection::open_with_flags(&tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)?.query_row(
            "PRAGMA quick_check",
            [],
            |r| r.get(0),
        )?;
        if check != "ok" {
            bail!("quick_check: {check}");
        }
        for side in ["-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(dest.with_file_name(side_name(dest, side)));
        }
        std::fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// `mainnet.sqlite` + `-wal` → `mainnet.sqlite-wal` (SQLite's names, no dot).
fn side_name(db: &Path, suffix: &str) -> String {
    format!("{}{suffix}", db.file_name().map(|f| f.to_string_lossy()).unwrap_or_default())
}

/// `VACUUM INTO` a consistent copy of `db` (while others keep writing it), gzip it to `gz`, and
/// return the compressed size and SHA-256.
fn compress_copy(db: &Path, gz: &Path) -> Result<(u64, String)> {
    let copy = tmp_path(db, "snapshot");
    let _ = std::fs::remove_file(&copy);
    let result = (|| -> Result<(u64, String)> {
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_secs(60))?;
        conn.execute("VACUUM INTO ?1", [copy.to_string_lossy()]).context("VACUUM INTO")?;
        drop(conn);
        let mut enc = GzEncoder::new(BufWriter::new(std::fs::File::create(gz)?), Compression::new(6));
        std::io::copy(&mut BufReader::new(std::fs::File::open(&copy)?), &mut enc)?;
        enc.finish()?.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        let mut hash = Sha256::new();
        let bytes = std::io::copy(&mut std::fs::File::open(gz)?, &mut hash)?;
        Ok((bytes, hex(&hash.finalize())))
    })();
    let _ = std::fs::remove_file(&copy);
    result
}

fn stamp_path(db: &Path) -> PathBuf {
    tmp_path(db, "snapshot-stamp")
}

fn read_stamp(db: &Path) -> Option<i64> {
    std::fs::read_to_string(stamp_path(db)).ok()?.trim().parse().ok()
}

fn write_stamp(db: &Path, stamp: i64) -> Result<()> {
    let (path, tmp) = (stamp_path(db), tmp_path(db, "snapshot-stamp.tmp"));
    std::fs::write(&tmp, stamp.to_string())?;
    std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))
}

fn tmp_path(next_to: &Path, suffix: &str) -> PathBuf {
    let mut p = next_to.as_os_str().to_owned();
    p.push(format!(".{suffix}"));
    PathBuf::from(p)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs `f` up to ATTEMPTS times, unless it was refused (a 4xx: retrying won't change that).
async fn retry<T, F, Fut>(what: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < ATTEMPTS && !e.is::<Refused>() => {
                tracing::warn!("snapshot {what}: {e:#}; retrying");
                tokio::time::sleep(Duration::from_secs(10 * u64::from(attempt))).await;
            }
            Err(e) => return Err(e.context(format!("snapshot {what}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_compresses_and_unpacks_over_a_stale_wal() {
        let dir = std::env::temp_dir().join(format!("dn-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("x.sqlite");
        let c = Connection::open(&db).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1), (2), (3);")
            .unwrap();
        let gz = dir.join("x.gz");
        let (bytes, sha) = compress_copy(&db, &gz).unwrap();
        assert_eq!(std::fs::metadata(&gz).unwrap().len(), bytes);
        assert_eq!(sha.len(), 64);
        // A WAL left behind by another database where the restored one goes must not be applied.
        let out = dir.join("y.sqlite");
        std::fs::write(dir.join("y.sqlite-wal"), b"stale").unwrap();
        unpack(&gz, &out).unwrap();
        assert!(!dir.join("y.sqlite-wal").exists());
        let n: i64 = Connection::open(&out).unwrap().query_row("SELECT sum(v) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 6);
        write_stamp(&out, 42).unwrap();
        assert_eq!(read_stamp(&out), Some(42));
        drop(c);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
