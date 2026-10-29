//! City (with country, continent and coordinates) and ASN lookups from the free DB-IP Lite
//! databases (CC BY 4.0, attribution on every page). They're refreshed monthly; DB-IP publishes
//! the new month on the 1st.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use maxminddb::Reader;
use serde::Deserialize;

#[derive(Clone, Default)]
pub struct Geo {
    inner: Arc<RwLock<Dbs>>,
}

#[derive(Default)]
struct Dbs {
    city: Option<Reader<Vec<u8>>>,
    asn: Option<Reader<Vec<u8>>>,
}

#[derive(Clone, Debug, Default)]
pub struct GeoInfo {
    pub country: Option<String>,
    pub continent: Option<String>,
    pub city: Option<String>,
    /// City-level coordinates, rounded to about a kilometre.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub asn: Option<u32>,
    pub as_org: Option<String>,
}

#[derive(Deserialize)]
struct CityRecord<'a> {
    #[serde(borrow)]
    country: Option<IsoCode<'a>>,
    #[serde(borrow)]
    continent: Option<ContinentCode<'a>>,
    #[serde(borrow)]
    city: Option<Names<'a>>,
    location: Option<Location>,
}

#[derive(Deserialize)]
struct IsoCode<'a> {
    iso_code: Option<&'a str>,
}

#[derive(Deserialize)]
struct ContinentCode<'a> {
    code: Option<&'a str>,
}

#[derive(Deserialize)]
struct Names<'a> {
    #[serde(borrow)]
    names: Option<English<'a>>,
}

#[derive(Deserialize)]
struct English<'a> {
    en: Option<&'a str>,
}

#[derive(Deserialize)]
struct Location {
    latitude: Option<f64>,
    longitude: Option<f64>,
}

#[derive(Deserialize)]
struct AsnRecord<'a> {
    autonomous_system_number: Option<u32>,
    autonomous_system_organization: Option<&'a str>,
}

const KINDS: [&str; 2] = ["city", "asn"];

fn round(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

impl Geo {
    pub fn lookup(&self, ip: IpAddr) -> GeoInfo {
        let dbs = self.inner.read().unwrap();
        let mut info = GeoInfo::default();
        if let Some(db) = &dbs.city
            && let Ok(Some(rec)) = db.lookup::<CityRecord>(ip)
        {
            info.country = rec.country.and_then(|c| c.iso_code).map(str::to_owned);
            info.continent = rec.continent.and_then(|c| c.code).map(str::to_owned);
            info.city = rec.city.and_then(|c| c.names).and_then(|n| n.en).map(str::to_owned);
            if let Some(Location { latitude: Some(lat), longitude: Some(lon) }) = rec.location {
                info.lat = Some(round(lat));
                info.lon = Some(round(lon));
            }
        }
        if let Some(db) = &dbs.asn
            && let Ok(Some(rec)) = db.lookup::<AsnRecord>(ip)
        {
            info.asn = rec.autonomous_system_number;
            info.as_org = rec.autonomous_system_organization.map(str::to_owned);
        }
        info
    }

    pub fn loaded(&self) -> bool {
        self.inner.read().unwrap().city.is_some()
    }

    /// Loads whatever is on disk, then keeps the files fresh. Never returns.
    pub async fn run(self, dir: PathBuf) {
        if let Err(e) = self.load(&dir) {
            tracing::info!("geo: nothing usable on disk yet ({e:#})");
        }
        loop {
            match refresh(&dir).await {
                Ok(true) => match self.load(&dir) {
                    Ok(()) => tracing::info!("geo: databases reloaded"),
                    Err(e) => tracing::warn!("geo: reload failed: {e:#}"),
                },
                Ok(false) => {}
                Err(e) => tracing::warn!("geo: refresh failed: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    }

    fn load(&self, dir: &Path) -> Result<()> {
        let open = |kind: &str| -> Result<Reader<Vec<u8>>> {
            let path = dir.join(format!("dbip-{kind}-lite.mmdb"));
            Reader::open_readfile(&path).with_context(|| format!("open {}", path.display()))
        };
        let city = open("city")?;
        let asn = open("asn")?;
        *self.inner.write().unwrap() = Dbs { city: Some(city), asn: Some(asn) };
        Ok(())
    }
}

/// Downloads this month's databases if the ones on disk are older. Returns true if anything changed.
async fn refresh(dir: &Path) -> Result<bool> {
    let month = crate::db::day_of(crate::db::now())[..7].to_string();
    let stamp = dir.join("dbip-month");
    let have_all = KINDS.iter().all(|k| dir.join(format!("dbip-{k}-lite.mmdb")).exists());
    if have_all && std::fs::read_to_string(&stamp).ok().as_deref() == Some(month.as_str()) {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(600)).build()?;
    for kind in KINDS {
        let url = format!("https://download.db-ip.com/free/dbip-{kind}-lite-{month}.mmdb.gz");
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            // Early on the 1st the new month may not be published yet; try again later.
            bail!("{url}: HTTP {}", resp.status());
        }
        let gz = resp.bytes().await?;
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut raw)
            .with_context(|| format!("gunzip {url}"))?;
        let path = dir.join(format!("dbip-{kind}-lite.mmdb"));
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &raw)?;
        Reader::open_readfile(&tmp).with_context(|| format!("{url}: not an mmdb"))?;
        std::fs::rename(&tmp, &path)?;
    }
    // Before 2026-11 the country database was used; the city one supersedes it.
    let _ = std::fs::remove_file(dir.join("dbip-country-lite.mmdb"));
    std::fs::write(&stamp, &month)?;
    Ok(true)
}
