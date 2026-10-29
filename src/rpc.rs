//! Minimal dashd JSON-RPC client.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    url: String,
    user: String,
    password: String,
}

impl Rpc {
    pub fn new(url: String, user: String, password: String) -> Rpc {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(300)).build().expect("http client");
        Rpc { client, url, user, password }
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let body = json!({"jsonrpc": "1.0", "id": 0, "method": method, "params": params});
        let resp: Value = self.post(&body).await.with_context(|| format!("rpc {method}"))?;
        let result = unwrap_response(resp).with_context(|| format!("rpc {method}"))?;
        serde_json::from_value(result).with_context(|| format!("rpc {method}: decode"))
    }

    /// One HTTP round trip for many calls; results come back in request order.
    pub async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let body: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (m, p))| json!({"jsonrpc": "1.0", "id": i, "method": m, "params": p}))
            .collect();
        let resp: Value = self.post(&Value::Array(body)).await.context("rpc batch")?;
        let Value::Array(items) = resp else { bail!("rpc batch: not an array") };
        let mut out = vec![Value::Null; calls.len()];
        for item in items {
            let id = item["id"].as_u64().ok_or_else(|| anyhow!("rpc batch: missing id"))? as usize;
            let slot = out.get_mut(id).ok_or_else(|| anyhow!("rpc batch: bad id {id}"))?;
            *slot = unwrap_response(item).with_context(|| format!("rpc batch {}", calls[id].0))?;
        }
        Ok(out)
    }

    async fn post(&self, body: &Value) -> Result<Value> {
        let resp = self.client.post(&self.url).basic_auth(&self.user, Some(&self.password)).json(body).send().await?;
        // dashd answers RPC errors with HTTP 500 and a JSON body, so don't bail on the status alone.
        let status = resp.status();
        let text = resp.text().await?;
        serde_json::from_str(&text)
            .map_err(|_| anyhow!("HTTP {status}: {}", text.chars().take(200).collect::<String>()))
    }
}

fn unwrap_response(mut resp: Value) -> Result<Value> {
    if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
        bail!("{} (code {})", err["message"].as_str().unwrap_or("?"), err["code"]);
    }
    Ok(resp["result"].take())
}
