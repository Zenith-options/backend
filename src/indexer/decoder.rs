use crate::chain::bindings::events::DecodedEvent;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DecoderKey {
    pub contract_id: String,
    pub wasm_hash: String,
    pub event_topic: String,
    pub schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawContractEvent {
    pub contract_id: String,
    pub wasm_hash: String,
    pub ledger: u32,
    pub tx_index: u32,
    pub op_index: u32,
    pub topics: Vec<String>,
    pub data: serde_json::Value,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    UnsupportedSchema(DecoderKey),
    MissingField(String),
    TypeMismatch(String),
    MalformedData(String),
    DatabaseError(String),
}

#[derive(Debug, Clone)]
pub enum DecodeOutcome {
    Ready(DecodedEvent),
    Quarantined { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantinedEvent {
    pub id: String,
    pub contract_id: String,
    pub wasm_hash: String,
    pub ledger: u32,
    pub tx_index: u32,
    pub op_index: u32,
    pub schema_version: u32,
    pub topics: Vec<String>,
    pub data: serde_json::Value,
    pub error_reason: String,
    pub status: String,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema(key) => write!(
                f,
                "No registered decoder for contract {} (wasm: {}, topic: {}, v{})",
                key.contract_id, key.wasm_hash, key.event_topic, key.schema_version
            ),
            Self::MissingField(field) => write!(f, "Missing event field: {field}"),
            Self::TypeMismatch(err) => write!(f, "Event field type mismatch: {err}"),
            Self::MalformedData(err) => write!(f, "Malformed event data: {err}"),
            Self::DatabaseError(err) => write!(f, "Indexer database error: {err}"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub trait EventDecoder: Send + Sync {
    fn supports(&self, key: &DecoderKey) -> bool;
    fn decode(&self, raw: &RawContractEvent) -> Result<DecodedEvent, DecodeError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackStrategy {
    ExactOnly,
    FallbackLatestSchema,
}

pub struct DecoderRegistry {
    decoders: Vec<Box<dyn EventDecoder>>,
    fallback_strategy: FallbackStrategy,
    paused_contracts: Arc<std::sync::Mutex<HashMap<String, bool>>>,
    db: SqlitePool,
}

impl DecoderRegistry {
    pub fn new(fallback_strategy: FallbackStrategy, db: SqlitePool) -> Self {
        Self {
            decoders: Vec::new(),
            fallback_strategy,
            paused_contracts: Arc::new(std::sync::Mutex::new(HashMap::new())),
            db,
        }
    }

    pub fn register<D: EventDecoder + 'static>(&mut self, decoder: D) {
        self.decoders.push(Box::new(decoder));
    }

    pub fn is_contract_paused(&self, contract_id: &str) -> bool {
        let paused = self.paused_contracts.lock().unwrap();
        *paused.get(contract_id).unwrap_or(&false)
    }

    pub async fn record_wasm_upgrade(
        &self,
        contract_id: &str,
        wasm_hash: &str,
        from_ledger: u32,
    ) -> Result<(), DecodeError> {
        sqlx::query(
            "INSERT INTO contract_wasm_history (contract_id, wasm_hash, from_ledger) VALUES (?1, ?2, ?3)",
        )
        .bind(contract_id)
        .bind(wasm_hash)
        .bind(from_ledger as i64)
        .execute(&self.db)
        .await
        .map_err(|e| DecodeError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    pub async fn decode_and_dispatch(
        &self,
        raw: &RawContractEvent,
        schema_version: u32,
    ) -> Result<DecodedEvent, DecodeError> {
        let topic = raw.topics.first().cloned().unwrap_or_default();
        let key = DecoderKey {
            contract_id: raw.contract_id.clone(),
            wasm_hash: raw.wasm_hash.clone(),
            event_topic: topic,
            schema_version,
        };

        let decoder = self
            .decoders
            .iter()
            .find(|d| d.supports(&key))
            .or_else(|| {
                if self.fallback_strategy != FallbackStrategy::FallbackLatestSchema {
                    return None;
                }
                (1..schema_version)
                    .rev()
                    .find_map(|version| {
                        let fallback_key = DecoderKey {
                            schema_version: version,
                            ..key.clone()
                        };
                        self.decoders
                            .iter()
                            .find(|decoder| decoder.supports(&fallback_key))
                    })
            });

        match decoder {
            Some(d) => match d.decode(raw) {
                Ok(event) => match validate_event(&self.db, &event).await {
                    Ok(()) => Ok(event),
                    Err(err) => {
                        self.handle_failure(raw, schema_version, &err).await?;
                        Err(err)
                    }
                },
                Err(err) => {
                    self.handle_failure(raw, schema_version, &err).await?;
                    Err(err)
                }
            },
            None => {
                let err = DecodeError::UnsupportedSchema(key);
                self.handle_failure(raw, schema_version, &err).await?;
                Err(err)
            }
        }
    }

    /// Decode one event without stopping ingestion when it must be quarantined.
    pub async fn decode_or_quarantine(
        &self,
        raw: &RawContractEvent,
        schema_version: u32,
    ) -> Result<DecodeOutcome, DecodeError> {
        match self.decode_and_dispatch(raw, schema_version).await {
            Ok(event) => Ok(DecodeOutcome::Ready(event)),
            Err(DecodeError::DatabaseError(err)) => Err(DecodeError::DatabaseError(err)),
            Err(error) => Ok(DecodeOutcome::Quarantined {
                reason: error.to_string(),
            }),
        }
    }

    pub async fn quarantined_events(&self, limit: i64) -> Result<Vec<QuarantinedEvent>, DecodeError> {
        let limit = limit.clamp(1, 500);
        let rows = sqlx::query_as::<_, (String, String, String, i64, i64, i64, i64, String, String, String, String)>(
            "SELECT id, contract_id, wasm_hash, ledger, tx_index, op_index, schema_version,
                    topics, data, error_reason, status
             FROM event_quarantine
             WHERE status = 'quarantined'
             ORDER BY ledger, tx_index, op_index
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.db)
        .await
        .map_err(|e| DecodeError::DatabaseError(e.to_string()))?;

        rows.into_iter()
            .map(|row| {
                Ok(QuarantinedEvent {
                    id: row.0,
                    contract_id: row.1,
                    wasm_hash: row.2,
                    ledger: u32::try_from(row.3).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
                    tx_index: u32::try_from(row.4).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
                    op_index: u32::try_from(row.5).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
                    schema_version: u32::try_from(row.6).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
                    topics: serde_json::from_str(&row.7).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
                    data: serde_json::from_str(&row.8).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
                    error_reason: row.9,
                    status: row.10,
                })
            })
            .collect()
    }

    /// Replay a quarantined event using the currently registered decoder.
    /// The row is marked replayed only after schema and semantic validation pass.
    pub async fn replay_quarantined(
        &self,
        id: &str,
        schema_version: u32,
    ) -> Result<DecodedEvent, DecodeError> {
        let row = sqlx::query_as::<_, (String, String, i64, i64, i64, String, String)>(
            "SELECT contract_id, wasm_hash, ledger, tx_index, op_index, topics, data
             FROM event_quarantine WHERE id = ?1 AND status = 'quarantined'",
        )
        .bind(id)
        .fetch_optional(&self.db)
        .await
        .map_err(|e| DecodeError::DatabaseError(e.to_string()))?
        .ok_or_else(|| DecodeError::MalformedData("quarantined event not found".into()))?;

        let raw = RawContractEvent {
            contract_id: row.0,
            wasm_hash: row.1,
            ledger: u32::try_from(row.2).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
            tx_index: u32::try_from(row.3).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
            op_index: u32::try_from(row.4).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
            topics: serde_json::from_str(&row.5).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
            data: serde_json::from_str(&row.6).map_err(|e| DecodeError::DatabaseError(e.to_string()))?,
        };
        let event = self.decode_and_dispatch(&raw, schema_version).await?;
        sqlx::query("UPDATE event_quarantine SET replay_attempts = replay_attempts + 1 WHERE id = ?1 AND status = 'quarantined'")
        .bind(id)
        .execute(&self.db)
        .await
        .map_err(|e| DecodeError::DatabaseError(e.to_string()))?;
        Ok(event)
    }

    /// Mark replay complete only after the caller has applied the returned
    /// event to its projection transaction.
    pub async fn mark_replayed(&self, id: &str) -> Result<bool, DecodeError> {
        let result = sqlx::query(
            "UPDATE event_quarantine SET status = 'replayed',
                    resolved_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1 AND status = 'quarantined'",
        )
        .bind(id)
        .execute(&self.db)
        .await
        .map_err(|e| DecodeError::DatabaseError(e.to_string()))?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn dismiss_quarantined(&self, id: &str) -> Result<bool, DecodeError> {
        let result = sqlx::query(
            "UPDATE event_quarantine SET status = 'dismissed', resolved_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1 AND status = 'quarantined'",
        )
        .bind(id)
        .execute(&self.db)
        .await
        .map_err(|e| DecodeError::DatabaseError(e.to_string()))?;
        Ok(result.rows_affected() == 1)
    }

    async fn handle_failure(
        &self,
        raw: &RawContractEvent,
        schema_version: u32,
        error: &DecodeError,
    ) -> Result<(), DecodeError> {
        let topics_json = serde_json::to_string(&raw.topics)
            .map_err(|e| DecodeError::MalformedData(e.to_string()))?;
        let data_json = raw.data.to_string();
        let err_reason = error.to_string();

        sqlx::query(
            "INSERT INTO event_quarantine
             (id, contract_id, wasm_hash, ledger, tx_index, op_index, schema_version, topics, data, error_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (contract_id, wasm_hash, ledger, tx_index, op_index) DO NOTHING",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&raw.contract_id)
        .bind(&raw.wasm_hash)
        .bind(raw.ledger as i64)
        .bind(raw.tx_index as i64)
        .bind(raw.op_index as i64)
        .bind(schema_version as i64)
        .bind(&topics_json)
        .bind(&data_json)
        .bind(&err_reason)
        .execute(&self.db)
        .await
        .map_err(|e| DecodeError::DatabaseError(e.to_string()))?;

        Ok(())
    }
}

async fn validate_known_series(
    db: &SqlitePool,
    event: &DecodedEvent,
) -> Result<(), DecodeError> {
    let series_id = match event {
        DecodedEvent::OptionMinted { series_id, .. }
        | DecodedEvent::OptionExercised { series_id, .. } => series_id,
        _ => return Ok(()),
    };
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM option_series
         WHERE instrument_id = ?1 OR CAST(id AS TEXT) = ?1",
    )
    .bind(series_id)
    .fetch_one(db)
    .await
    .map_err(|e| DecodeError::DatabaseError(e.to_string()))?;
    if exists == 0 {
        return Err(DecodeError::MalformedData(format!(
            "unknown option series: {series_id}"
        )));
    }
    Ok(())
}

async fn validate_event(db: &SqlitePool, event: &DecodedEvent) -> Result<(), DecodeError> {
    validate_decoded_event(event)?;
    validate_known_series(db, event).await
}

pub fn validate_decoded_event(event: &DecodedEvent) -> Result<(), DecodeError> {
    let nonempty = |name: &str, value: &str| {
        if value.trim().is_empty() || value.len() > 256 {
            Err(DecodeError::MalformedData(format!(
                "{name} must contain 1 to 256 characters"
            )))
        } else {
            Ok(())
        }
    };
    let finite_positive = |name: &str, value: f64| {
        if !value.is_finite() || value <= 0.0 {
            Err(DecodeError::MalformedData(format!(
                "{name} must be finite and positive"
            )))
        } else {
            Ok(())
        }
    };

    match event {
        DecodedEvent::OptionMinted {
            series_id,
            strike,
            expiry,
            contracts,
            writer,
            ..
        } => {
            nonempty("series_id", series_id)?;
            nonempty("writer", writer)?;
            finite_positive("strike", *strike)?;
            finite_positive("contracts", *contracts)?;
            if *expiry == 0 {
                return Err(DecodeError::MalformedData(
                    "expiry must be a positive timestamp".into(),
                ));
            }
        }
        DecodedEvent::OptionExercised {
            series_id,
            buyer,
            payout,
        } => {
            nonempty("series_id", series_id)?;
            nonempty("buyer", buyer)?;
            if !payout.is_finite() || *payout < 0.0 {
                return Err(DecodeError::MalformedData(
                    "payout must be finite and non-negative".into(),
                ));
            }
        }
        DecodedEvent::CollateralDeposited { wallet, amount } => {
            nonempty("wallet", wallet)?;
            finite_positive("amount", *amount)?;
        }
        DecodedEvent::ContractUpgraded {
            new_wasm_hash,
            new_version,
        } => {
            nonempty("new_wasm_hash", new_wasm_hash)?;
            if *new_version == 0 {
                return Err(DecodeError::MalformedData(
                    "contract version must be positive".into(),
                ));
            }
        }
    }
    Ok(())
}

// Sample V1 & V2 Decoders for testing upgrades
pub struct OptionMintedDecoderV1;
impl EventDecoder for OptionMintedDecoderV1 {
    fn supports(&self, key: &DecoderKey) -> bool {
        key.event_topic == "mint" && key.schema_version == 1 && key.wasm_hash == "WASM_V1_HASH"
    }

    fn decode(&self, raw: &RawContractEvent) -> Result<DecodedEvent, DecodeError> {
        let series_id = raw.data["series_id"].as_str().ok_or(DecodeError::MissingField("series_id".into()))?;
        let strike = raw.data["strike"].as_f64().ok_or(DecodeError::MissingField("strike".into()))?;
        let contracts = raw.data["contracts"].as_f64().ok_or(DecodeError::MissingField("contracts".into()))?;
        let writer = raw.data["writer"].as_str().ok_or(DecodeError::MissingField("writer".into()))?;

        Ok(DecodedEvent::OptionMinted {
            series_id: series_id.to_string(),
            strike,
            expiry: 1735689600,
            is_call: true,
            contracts,
            writer: writer.to_string(),
        })
    }
}

pub struct OptionMintedDecoderV2;
impl EventDecoder for OptionMintedDecoderV2 {
    fn supports(&self, key: &DecoderKey) -> bool {
        key.event_topic == "mint" && key.schema_version == 2 && key.wasm_hash == "WASM_V2_HASH"
    }

    fn decode(&self, raw: &RawContractEvent) -> Result<DecodedEvent, DecodeError> {
        let series_id = raw.data["series_id"].as_str().ok_or(DecodeError::MissingField("series_id".into()))?;
        let strike = raw.data["strike_price"].as_f64().ok_or(DecodeError::MissingField("strike_price".into()))?;
        let contracts = raw.data["volume"].as_f64().ok_or(DecodeError::MissingField("volume".into()))?;
        let is_call = raw.data["is_call"].as_bool().ok_or(DecodeError::MissingField("is_call".into()))?;
        let writer = raw.data["writer"].as_str().ok_or(DecodeError::MissingField("writer".into()))?;

        Ok(DecodedEvent::OptionMinted {
            series_id: series_id.to_string(),
            strike,
            expiry: 1735689600,
            is_call,
            contracts,
            writer: writer.to_string(),
        })
    }
}
