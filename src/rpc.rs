//! JSON-RPC helper for zebra and the resolver.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub async fn json_rpc(url: &str, method: &str, params: Value) -> Result<Value> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let resp = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("{method} POST {url}"))?;
    let envelope: Value = resp.json().await.context("decode json-rpc response")?;
    if let Some(err) = envelope.get("error").filter(|e| !e.is_null()) {
        bail!("json-rpc error from {method}: {err}");
    }
    Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
}
