//! Versioned schema for settlement-contract events, and the decoder registry
//! the indexer runs them through.
//!
//! ## Wire format
//!
//! Every event the settlement contracts emit has the topic layout
//!
//! ```text
//! [ "zenith", <event name: Symbol>, <schema version: u32>, ...index topics ]
//! ```
//!
//! and a `Map<Symbol, Val>` body. A contract change that alters the body of an
//! event **must** bump its schema version. The backend declares exactly which
//! `(event, version)` pairs it can decode in [`SETTLEMENT_SCHEMAS`]; the
//! registry validates each body against its schema (required fields, types,
//! no unexpected fields) before a version-specific decoder maps it to the
//! version-independent [`SettlementEvent`].
//!
//! ## Never drop, quarantine
//!
//! Anything the registry cannot decode — unknown event name, unknown version,
//! or a body that violates its declared schema — is written to
//! `event_quarantine` with the full raw payload and a reason, instead of being
//! skipped. After shipping a decoder for the new version, call
//! [`EventIngestor::replay_quarantine`] to decode and release the backlog.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::SqlitePool;
use std::collections::BTreeMap;

pub const EVENT_NAMESPACE: &str = "zenith";
const MIGRATION_SQL: &str = include_str!("../../migrations/0013_event_quarantine.sql");

pub async fn ensure_schema(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(MIGRATION_SQL).execute(db).await.map(|_| ())
}

// ─── Raw input ───────────────────────────────────────────────────────────────

/// One contract event as returned by `getEvents` (with `xdrFormat: "json"`).
/// Topic and body values may be bare JSON (`"settle"`, `2`) or tagged ScVal
/// JSON (`{"symbol":"settle"}`, `{"u32":2}`); both are accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawContractEvent {
    /// RPC event id (`<toid>-<index>`), unique per event and ordered.
    pub id: String,
    pub contract_id: String,
    pub ledger: u32,
    pub tx_hash: String,
    pub topics: Vec<Value>,
    pub data: Value,
}

/// Strips one level of ScVal tagging: `{"symbol":"x"}` → `"x"`.
fn untag(v: &Value) -> &Value {
    match v {
        Value::Object(m) if m.len() == 1 => {
            let (k, inner) = m.iter().next().unwrap();
            match k.as_str() {
                "symbol" | "string" | "address" | "u32" | "i32" | "u64" | "i64" | "u128"
                | "i128" | "bool" => inner,
                _ => v,
            }
        }
        _ => v,
    }
}

fn as_str(v: &Value) -> Option<&str> {
    untag(v).as_str()
}

fn as_u64(v: &Value) -> Option<u64> {
    let v = untag(v);
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn as_i128(v: &Value) -> Option<i128> {
    let v = untag(v);
    match v {
        Value::Number(n) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Accepts a JSON object, or ScVal map JSON `[{"key":..,"val":..}]` /
/// `{"map":[...]}`, and returns plain `name -> value`.
fn body_map(data: &Value) -> Option<Map<String, Value>> {
    match untag(data) {
        Value::Object(m) if m.len() == 1 && m.contains_key("map") => body_map(&m["map"]),
        Value::Object(m) => Some(m.clone()),
        Value::Array(entries) => entries
            .iter()
            .map(|e| Some((as_str(e.get("key")?)?.to_string(), e.get("val")?.clone())))
            .collect(),
        _ => None,
    }
}

// ─── Schemas ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    /// Stellar strkey (`G...` account or `C...` contract).
    Address,
    Symbol,
    U32,
    U64,
    /// Fixed-point amount; JSON number or decimal string.
    I128,
}

impl FieldType {
    fn accepts(self, v: &Value) -> bool {
        match self {
            Self::Address => as_str(v)
                .is_some_and(|s| s.len() == 56 && (s.starts_with('G') || s.starts_with('C'))),
            Self::Symbol => as_str(v).is_some_and(|s| {
                !s.is_empty()
                    && s.len() <= 32
                    && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            }),
            Self::U32 => as_u64(v).is_some_and(|n| n <= u32::MAX as u64),
            Self::U64 => as_u64(v).is_some(),
            Self::I128 => as_i128(v).is_some(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FieldSpec {
    pub name: &'static str,
    pub ty: FieldType,
}

const fn f(name: &'static str, ty: FieldType) -> FieldSpec {
    FieldSpec { name, ty }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EventSchema {
    pub event: &'static str,
    pub version: u32,
    pub fields: &'static [FieldSpec],
}

use FieldType::*;

/// Every settlement event version the backend can decode. Adding a contract
/// event version means adding a row here **and** a decoder in
/// [`SchemaDecoderRegistry::settlement`]; the registry refuses to build if the
/// two disagree.
pub const SETTLEMENT_SCHEMAS: &[EventSchema] = &[
    EventSchema {
        event: "series_settled",
        version: 1,
        fields: &[
            f("series_id", Symbol),
            f("settlement_price", I128),
            f("settled_at", U64),
        ],
    },
    EventSchema {
        event: "series_settled",
        version: 2,
        fields: &[
            f("series_id", Symbol),
            f("settlement_price", I128),
            f("price_decimals", U32),
            f("settled_at", U64),
            f("oracle", Address),
        ],
    },
    EventSchema {
        event: "position_settled",
        version: 1,
        fields: &[
            f("position_id", U64),
            f("owner", Address),
            f("payout", I128),
        ],
    },
    EventSchema {
        event: "position_settled",
        version: 2,
        fields: &[
            f("position_id", U64),
            f("series_id", Symbol),
            f("owner", Address),
            f("payout", I128),
            f("fee", I128),
        ],
    },
    EventSchema {
        event: "collateral_released",
        version: 1,
        fields: &[f("owner", Address), f("asset", Address), f("amount", I128)],
    },
];

/// Price decimals implied by v1 `series_settled`, which predates the field.
pub const V1_PRICE_DECIMALS: u32 = 7;

// ─── Decoded output ──────────────────────────────────────────────────────────

/// Version-independent settlement event. Fields a version doesn't carry are
/// filled with that version's documented default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettlementEvent {
    SeriesSettled {
        series_id: String,
        settlement_price: i128,
        price_decimals: u32,
        settled_at: u64,
        oracle: Option<String>,
    },
    PositionSettled {
        position_id: u64,
        series_id: Option<String>,
        owner: String,
        payout: i128,
        fee: i128,
    },
    CollateralReleased {
        owner: String,
        asset: String,
        amount: i128,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodedSettlementEvent {
    pub event_id: String,
    pub contract_id: String,
    pub ledger: u32,
    pub tx_hash: String,
    pub schema_version: u32,
    pub event: SettlementEvent,
}

// ─── Failures ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum DecodeFailure {
    /// Topics don't carry a usable event name / version.
    MalformedEnvelope {
        detail: String,
    },
    UnknownEvent {
        event: String,
        version: u32,
    },
    UnknownVersion {
        event: String,
        version: u32,
        supported: Vec<u32>,
    },
    SchemaViolation {
        event: String,
        version: u32,
        detail: String,
    },
}

impl DecodeFailure {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MalformedEnvelope { .. } => "malformed_envelope",
            Self::UnknownEvent { .. } => "unknown_event",
            Self::UnknownVersion { .. } => "unknown_version",
            Self::SchemaViolation { .. } => "schema_violation",
        }
    }
}

impl std::fmt::Display for DecodeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedEnvelope { detail } => write!(f, "malformed event envelope: {detail}"),
            Self::UnknownEvent { event, version } => write!(f, "unknown event {event} v{version}"),
            Self::UnknownVersion {
                event,
                version,
                supported,
            } => {
                write!(
                    f,
                    "unknown schema version {event} v{version} (supported: {supported:?})"
                )
            }
            Self::SchemaViolation {
                event,
                version,
                detail,
            } => {
                write!(f, "{event} v{version} violates its schema: {detail}")
            }
        }
    }
}

// ─── Registry ────────────────────────────────────────────────────────────────

/// Typed, schema-validated view of an event body handed to decoders.
pub struct Fields<'a>(&'a Map<String, Value>);

impl Fields<'_> {
    // Validation has already run, so these only fail on a registry bug.
    pub fn str(&self, k: &str) -> String {
        self.0
            .get(k)
            .and_then(as_str)
            .unwrap_or_default()
            .to_string()
    }
    pub fn u64(&self, k: &str) -> u64 {
        self.0.get(k).and_then(as_u64).unwrap_or_default()
    }
    pub fn u32(&self, k: &str) -> u32 {
        self.u64(k) as u32
    }
    pub fn i128(&self, k: &str) -> i128 {
        self.0.get(k).and_then(as_i128).unwrap_or_default()
    }
}

pub type DecodeFn = fn(&Fields<'_>) -> SettlementEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    DecoderWithoutSchema { event: String, version: u32 },
    SchemaWithoutDecoder { event: String, version: u32 },
    Duplicate { event: String, version: u32 },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DecoderWithoutSchema { event, version } => {
                write!(f, "decoder for {event} v{version} has no declared schema")
            }
            Self::SchemaWithoutDecoder { event, version } => {
                write!(
                    f,
                    "backend claims {event} v{version} but has no decoder for it"
                )
            }
            Self::Duplicate { event, version } => {
                write!(f, "{event} v{version} registered twice")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

pub struct SchemaDecoderRegistry {
    decoders: BTreeMap<(String, u32), (EventSchema, DecodeFn)>,
}

impl SchemaDecoderRegistry {
    /// Builds a registry from `(schema, decoder)` pairs and checks it decodes
    /// exactly the versions `claimed` lists — no more, no less.
    pub fn build(
        claimed: &[EventSchema],
        decoders: &[(&str, u32, DecodeFn)],
    ) -> Result<Self, RegistryError> {
        let mut map = BTreeMap::new();
        for &(event, version, decode) in decoders {
            let schema = claimed
                .iter()
                .find(|s| s.event == event && s.version == version)
                .copied()
                .ok_or_else(|| RegistryError::DecoderWithoutSchema {
                    event: event.into(),
                    version,
                })?;
            if map
                .insert((event.to_string(), version), (schema, decode))
                .is_some()
            {
                return Err(RegistryError::Duplicate {
                    event: event.into(),
                    version,
                });
            }
        }
        if let Some(s) = claimed
            .iter()
            .find(|s| !map.contains_key(&(s.event.to_string(), s.version)))
        {
            return Err(RegistryError::SchemaWithoutDecoder {
                event: s.event.into(),
                version: s.version,
            });
        }
        Ok(Self { decoders: map })
    }

    /// The registry for [`SETTLEMENT_SCHEMAS`].
    pub fn settlement() -> Self {
        Self::build(
            SETTLEMENT_SCHEMAS,
            &[
                ("series_settled", 1, |f| SettlementEvent::SeriesSettled {
                    series_id: f.str("series_id"),
                    settlement_price: f.i128("settlement_price"),
                    price_decimals: V1_PRICE_DECIMALS,
                    settled_at: f.u64("settled_at"),
                    oracle: None,
                }),
                ("series_settled", 2, |f| SettlementEvent::SeriesSettled {
                    series_id: f.str("series_id"),
                    settlement_price: f.i128("settlement_price"),
                    price_decimals: f.u32("price_decimals"),
                    settled_at: f.u64("settled_at"),
                    oracle: Some(f.str("oracle")),
                }),
                ("position_settled", 1, |f| {
                    SettlementEvent::PositionSettled {
                        position_id: f.u64("position_id"),
                        series_id: None,
                        owner: f.str("owner"),
                        payout: f.i128("payout"),
                        fee: 0,
                    }
                }),
                ("position_settled", 2, |f| {
                    SettlementEvent::PositionSettled {
                        position_id: f.u64("position_id"),
                        series_id: Some(f.str("series_id")),
                        owner: f.str("owner"),
                        payout: f.i128("payout"),
                        fee: f.i128("fee"),
                    }
                }),
                ("collateral_released", 1, |f| {
                    SettlementEvent::CollateralReleased {
                        owner: f.str("owner"),
                        asset: f.str("asset"),
                        amount: f.i128("amount"),
                    }
                }),
            ],
        )
        .expect("SETTLEMENT_SCHEMAS and settlement decoders must agree")
    }

    /// `(event, version)` pairs this registry decodes.
    pub fn supported(&self) -> Vec<(String, u32)> {
        self.decoders.keys().cloned().collect()
    }

    pub fn supports(&self, event: &str, version: u32) -> bool {
        self.decoders.contains_key(&(event.to_string(), version))
    }

    /// `Ok(None)` for events from another namespace (not ours to decode).
    pub fn decode(
        &self,
        raw: &RawContractEvent,
    ) -> Result<Option<DecodedSettlementEvent>, DecodeFailure> {
        match raw.topics.first().and_then(as_str) {
            Some(EVENT_NAMESPACE) => {}
            _ => return Ok(None),
        }
        let event =
            raw.topics
                .get(1)
                .and_then(as_str)
                .ok_or_else(|| DecodeFailure::MalformedEnvelope {
                    detail: "topic[1] (event name) missing".into(),
                })?;
        let version = raw
            .topics
            .get(2)
            .and_then(as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| DecodeFailure::MalformedEnvelope {
                detail: format!("{event}: topic[2] (schema version) missing or not a u32"),
            })?;

        let Some((schema, decode)) = self.decoders.get(&(event.to_string(), version)) else {
            let supported: Vec<u32> = self
                .decoders
                .keys()
                .filter(|(e, _)| e == event)
                .map(|(_, v)| *v)
                .collect();
            return Err(if supported.is_empty() {
                DecodeFailure::UnknownEvent {
                    event: event.into(),
                    version,
                }
            } else {
                DecodeFailure::UnknownVersion {
                    event: event.into(),
                    version,
                    supported,
                }
            });
        };

        let violation = |detail: String| DecodeFailure::SchemaViolation {
            event: event.into(),
            version,
            detail,
        };
        let body = body_map(&raw.data).ok_or_else(|| violation("body is not a map".into()))?;
        for spec in schema.fields {
            match body.get(spec.name) {
                None => return Err(violation(format!("missing field `{}`", spec.name))),
                Some(v) if !spec.ty.accepts(v) => {
                    return Err(violation(format!(
                        "field `{}` is not a valid {:?}: {v}",
                        spec.name, spec.ty
                    )))
                }
                _ => {}
            }
        }
        // An extra field means the contract changed the body without bumping
        // the version — refuse rather than silently ignore new data.
        if let Some(extra) = body
            .keys()
            .find(|k| !schema.fields.iter().any(|s| s.name == *k))
        {
            return Err(violation(format!("unexpected field `{extra}`")));
        }

        Ok(Some(DecodedSettlementEvent {
            event_id: raw.id.clone(),
            contract_id: raw.contract_id.clone(),
            ledger: raw.ledger,
            tx_hash: raw.tx_hash.clone(),
            schema_version: version,
            event: decode(&Fields(&body)),
        }))
    }
}

// ─── Ingestor & quarantine ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Decoded(DecodedSettlementEvent),
    /// Stored in `event_quarantine`; nothing was dropped.
    Quarantined {
        event_id: String,
        failure: DecodeFailure,
    },
    /// Not a settlement event.
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuarantineSummary {
    pub event_name: Option<String>,
    pub schema_version: Option<u32>,
    pub reason: String,
    pub count: i64,
    pub first_ledger: u32,
    pub last_ledger: u32,
}

/// `(event_name, schema_version, reason, count, min_ledger, max_ledger)`
type SummaryRow = (Option<String>, Option<i64>, String, i64, i64, i64);

pub struct EventIngestor {
    registry: SchemaDecoderRegistry,
    db: SqlitePool,
}

impl EventIngestor {
    pub fn new(registry: SchemaDecoderRegistry, db: SqlitePool) -> Self {
        Self { registry, db }
    }

    pub fn registry(&self) -> &SchemaDecoderRegistry {
        &self.registry
    }

    pub async fn ingest(&self, raw: &RawContractEvent) -> Result<IngestOutcome, sqlx::Error> {
        match self.registry.decode(raw) {
            Ok(Some(ev)) => Ok(IngestOutcome::Decoded(ev)),
            Ok(None) => Ok(IngestOutcome::Ignored),
            Err(failure) => {
                self.quarantine(raw, &failure).await?;
                tracing::error!(
                    target: "zenith::indexer",
                    event_id = %raw.id,
                    contract_id = %raw.contract_id,
                    ledger = raw.ledger,
                    reason = failure.code(),
                    "settlement event quarantined: {failure}"
                );
                Ok(IngestOutcome::Quarantined {
                    event_id: raw.id.clone(),
                    failure,
                })
            }
        }
    }

    async fn quarantine(
        &self,
        raw: &RawContractEvent,
        failure: &DecodeFailure,
    ) -> Result<(), sqlx::Error> {
        let (name, version) = match failure {
            DecodeFailure::MalformedEnvelope { .. } => {
                (raw.topics.get(1).and_then(as_str).map(str::to_string), None)
            }
            DecodeFailure::UnknownEvent { event, version }
            | DecodeFailure::UnknownVersion { event, version, .. }
            | DecodeFailure::SchemaViolation { event, version, .. } => {
                (Some(event.clone()), Some(*version as i64))
            }
        };
        // Re-ingesting the same event (e.g. after a restart) updates the
        // existing row instead of duplicating it.
        sqlx::query(
            "INSERT INTO event_quarantine
                (event_id, contract_id, ledger, tx_hash, event_name, schema_version,
                 reason, detail, topics, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (event_id) DO UPDATE SET
                reason = excluded.reason,
                detail = excluded.detail,
                attempts = event_quarantine.attempts + 1,
                last_attempt_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE event_quarantine.status = 'quarantined'",
        )
        .bind(&raw.id)
        .bind(&raw.contract_id)
        .bind(raw.ledger as i64)
        .bind(&raw.tx_hash)
        .bind(name)
        .bind(version)
        .bind(failure.code())
        .bind(failure.to_string())
        .bind(serde_json::to_string(&raw.topics).unwrap_or_default())
        .bind(raw.data.to_string())
        .execute(&self.db)
        .await
        .map(|_| ())
    }

    /// Re-runs every quarantined event through the current registry, oldest
    /// first. Events that now decode are marked `released` and returned in
    /// chain order for the caller to project; the rest stay quarantined.
    pub async fn replay_quarantine(&self) -> Result<Vec<DecodedSettlementEvent>, sqlx::Error> {
        let rows: Vec<(String, String, i64, String, String, String)> = sqlx::query_as(
            "SELECT event_id, contract_id, ledger, tx_hash, topics, data
             FROM event_quarantine WHERE status = 'quarantined'
             ORDER BY ledger, event_id",
        )
        .fetch_all(&self.db)
        .await?;

        let mut released = Vec::new();
        for (id, contract_id, ledger, tx_hash, topics, data) in rows {
            let raw = RawContractEvent {
                id,
                contract_id,
                ledger: ledger as u32,
                tx_hash,
                topics: serde_json::from_str(&topics).unwrap_or_default(),
                data: serde_json::from_str(&data).unwrap_or(Value::Null),
            };
            match self.ingest(&raw).await? {
                IngestOutcome::Decoded(ev) => {
                    sqlx::query(
                        "UPDATE event_quarantine
                         SET status = 'released',
                             released_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                         WHERE event_id = ?1",
                    )
                    .bind(&ev.event_id)
                    .execute(&self.db)
                    .await?;
                    released.push(ev);
                }
                IngestOutcome::Ignored => {
                    sqlx::query(
                        "UPDATE event_quarantine SET status = 'ignored' WHERE event_id = ?1",
                    )
                    .bind(&raw.id)
                    .execute(&self.db)
                    .await?;
                }
                IngestOutcome::Quarantined { .. } => {}
            }
        }
        Ok(released)
    }

    /// Outstanding quarantined events grouped by event/version/reason — what
    /// an operator dashboard or alert should show. Non-empty means the
    /// indexer is missing data.
    pub async fn quarantine_summary(&self) -> Result<Vec<QuarantineSummary>, sqlx::Error> {
        let rows: Vec<SummaryRow> = sqlx::query_as(
            "SELECT event_name, schema_version, reason, COUNT(*), MIN(ledger), MAX(ledger)
             FROM event_quarantine WHERE status = 'quarantined'
             GROUP BY event_name, schema_version, reason
             ORDER BY MIN(ledger)",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(event_name, v, reason, count, lo, hi)| QuarantineSummary {
                event_name,
                schema_version: v.map(|v| v as u32),
                reason,
                count,
                first_ledger: lo as u32,
                last_ledger: hi as u32,
            })
            .collect())
    }

    /// Lowest ledger with an outstanding quarantined event. Projections that
    /// need a gap-free history must not treat anything at or after this
    /// ledger as final.
    pub async fn earliest_quarantined_ledger(&self) -> Result<Option<u32>, sqlx::Error> {
        let (min,): (Option<i64>,) =
            sqlx::query_as("SELECT MIN(ledger) FROM event_quarantine WHERE status = 'quarantined'")
                .fetch_one(&self.db)
                .await?;
        Ok(min.map(|l| l as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const G: &str = "GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN7";
    const C: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";

    fn raw(topics: Vec<Value>, data: Value) -> RawContractEvent {
        RawContractEvent {
            id: "0000004294967296-0000000001".into(),
            contract_id: C.into(),
            ledger: 1,
            tx_hash: "aa".into(),
            topics,
            data,
        }
    }

    #[test]
    fn every_claimed_schema_has_a_decoder() {
        let r = SchemaDecoderRegistry::settlement();
        assert_eq!(r.supported().len(), SETTLEMENT_SCHEMAS.len());
    }

    #[test]
    fn registry_build_rejects_claim_without_decoder() {
        let err = SchemaDecoderRegistry::build(SETTLEMENT_SCHEMAS, &[])
            .err()
            .unwrap();
        assert!(matches!(err, RegistryError::SchemaWithoutDecoder { .. }));
        let err = SchemaDecoderRegistry::build(&[], &[("x", 1, |_| unreachable!())])
            .err()
            .unwrap();
        assert!(matches!(err, RegistryError::DecoderWithoutSchema { .. }));
    }

    #[test]
    fn decodes_v1_and_v2_to_the_same_shape() {
        let r = SchemaDecoderRegistry::settlement();
        let v1 = r
            .decode(&raw(
                vec![json!("zenith"), json!("position_settled"), json!(1)],
                json!({"position_id": 7, "owner": G, "payout": "1000000000000000000000"}),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            v1.event,
            SettlementEvent::PositionSettled {
                position_id: 7,
                series_id: None,
                owner: G.into(),
                payout: 1_000_000_000_000_000_000_000,
                fee: 0
            }
        );

        // Tagged ScVal JSON, map-entry body form.
        let v2 = r
            .decode(&raw(
                vec![
                    json!({"symbol":"zenith"}),
                    json!({"symbol":"position_settled"}),
                    json!({"u32":2}),
                ],
                json!({"map": [
                    {"key": {"symbol":"position_id"}, "val": {"u64": "7"}},
                    {"key": {"symbol":"series_id"}, "val": {"symbol":"XLM_C_0_12"}},
                    {"key": {"symbol":"owner"}, "val": {"address": G}},
                    {"key": {"symbol":"payout"}, "val": {"i128": "500"}},
                    {"key": {"symbol":"fee"}, "val": {"i128": "5"}}
                ]}),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(v2.schema_version, 2);
        assert!(matches!(
            v2.event,
            SettlementEvent::PositionSettled { fee: 5, .. }
        ));
    }

    #[test]
    fn classifies_failures() {
        let r = SchemaDecoderRegistry::settlement();
        let t = |name: &str, v: u32| vec![json!("zenith"), json!(name), json!(v)];

        assert_eq!(r.decode(&raw(vec![json!("transfer")], json!({}))), Ok(None));
        assert!(matches!(
            r.decode(&raw(
                vec![json!("zenith"), json!("series_settled")],
                json!({})
            )),
            Err(DecodeFailure::MalformedEnvelope { .. })
        ));
        assert_eq!(
            r.decode(&raw(t("series_settled", 3), json!({}))),
            Err(DecodeFailure::UnknownVersion {
                event: "series_settled".into(),
                version: 3,
                supported: vec![1, 2]
            })
        );
        assert!(matches!(
            r.decode(&raw(t("brand_new", 1), json!({}))),
            Err(DecodeFailure::UnknownEvent { .. })
        ));
        let missing = r.decode(&raw(
            t("collateral_released", 1),
            json!({"owner": G, "asset": C}),
        ));
        assert!(
            matches!(missing, Err(DecodeFailure::SchemaViolation { ref detail, .. }) if detail.contains("amount"))
        );
        let bad_type = r.decode(&raw(
            t("collateral_released", 1),
            json!({"owner": "not-an-address", "asset": C, "amount": 1}),
        ));
        assert!(matches!(
            bad_type,
            Err(DecodeFailure::SchemaViolation { .. })
        ));
        let extra = r.decode(&raw(
            t("collateral_released", 1),
            json!({"owner": G, "asset": C, "amount": 1, "memo": "x"}),
        ));
        assert!(
            matches!(extra, Err(DecodeFailure::SchemaViolation { ref detail, .. }) if detail.contains("memo"))
        );
    }
}
