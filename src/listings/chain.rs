//! Reading payment receipts from the payment chain over JSON-RPC.

use crate::error::{Error, Result};
use ant_listings::{PaymentLog, PaymentReceipt};
use serde_json::{json, Value};
use std::time::Duration;

/// How long one RPC call may take.
const RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// A JSON-RPC client for the payment chain.
pub struct Chain {
    http: reqwest::Client,
    rpc_url: String,
}

impl Chain {
    /// A client for the chain at `rpc_url`.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client can't be built.
    pub fn new(rpc_url: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(RPC_TIMEOUT)
            .build()
            .map_err(|e| Error::Network(format!("listings RPC client: {e}")))?;
        Ok(Self { http, rpc_url })
    }

    /// The receipt of `tx_hash` and its block's timestamp, or `None` if the
    /// chain doesn't know the transaction (yet).
    ///
    /// # Errors
    ///
    /// Returns an error if a call fails or the answer is malformed.
    pub async fn payment(&self, tx_hash: &[u8; 32]) -> Result<Option<(PaymentReceipt, u64)>> {
        let hash = format!("0x{}", hex::encode(tx_hash));
        let receipt = self
            .call("eth_getTransactionReceipt", json!([hash]))
            .await?;
        if receipt.is_null() {
            return Ok(None);
        }
        let block_hex = receipt["blockNumber"]
            .as_str()
            .ok_or_else(|| malformed("receipt has no block number"))?;
        let block_number = parse_u64(block_hex)?;
        let success = receipt["status"].as_str() == Some("0x1");
        let logs = receipt["logs"]
            .as_array()
            .map(|logs| logs.iter().filter_map(parse_log).collect())
            .unwrap_or_default();

        let block = self
            .call("eth_getBlockByNumber", json!([block_hex, false]))
            .await?;
        let timestamp = block["timestamp"]
            .as_str()
            .ok_or_else(|| malformed("block has no timestamp"))
            .and_then(parse_u64)?;

        Ok(Some((
            PaymentReceipt {
                tx_hash: *tx_hash,
                success,
                block_number,
                logs,
            },
            timestamp,
        )))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let response: Value = self
            .http
            .post(&self.rpc_url)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| Error::Network(format!("payment chain {method}: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Network(format!("payment chain {method}: {e}")))?;
        if let Some(error) = response.get("error") {
            return Err(Error::Network(format!("payment chain {method}: {error}")));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// One receipt log, or `None` if it is malformed.
fn parse_log(log: &Value) -> Option<PaymentLog> {
    Some(PaymentLog {
        address: parse_bytes(log["address"].as_str()?)?,
        topics: log["topics"]
            .as_array()?
            .iter()
            .map(|topic| topic.as_str().and_then(parse_bytes))
            .collect::<Option<Vec<[u8; 32]>>>()?,
        log_index: parse_u64(log["logIndex"].as_str()?).ok()?,
    })
}

fn parse_u64(hex: &str) -> Result<u64> {
    u64::from_str_radix(hex.trim_start_matches("0x"), 16)
        .map_err(|e| malformed(&format!("bad quantity {hex}: {e}")))
}

fn parse_bytes<const N: usize>(hex: &str) -> Option<[u8; N]> {
    hex::decode(hex.trim_start_matches("0x"))
        .ok()?
        .try_into()
        .ok()
}

fn malformed(what: &str) -> Error {
    Error::Network(format!("payment chain answer malformed: {what}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_vault_log() {
        let log = json!({
            "address": "0x6969696969696969696969696969696969696969",
            "topics": [
                "0xf998960b1c6f0e0e89b7bbe6b6fbf3e03e6f08eee5b8430877d8adb8e149d580",
                "0x000000000000000000000000abababababababababababababababababababab",
                "0x0000000000000000000000000000000000000000000000000000000000000057",
                "0x1111111111111111111111111111111111111111111111111111111111111111"
            ],
            "logIndex": "0x1f"
        });
        let parsed = parse_log(&log).unwrap();
        assert_eq!(parsed.log_index, 31);
        assert_eq!(parsed.topics.len(), 4);
        assert_eq!(parsed.topics[0], ant_listings::DATA_PAYMENT_MADE_TOPIC);
        assert_eq!(parsed.address[0], 0x69);
    }

    #[test]
    fn skips_malformed_logs() {
        assert!(
            parse_log(&json!({ "address": "0x12", "topics": [], "logIndex": "0x0" })).is_none()
        );
        assert!(parse_log(&json!({ "topics": [] })).is_none());
    }
}
