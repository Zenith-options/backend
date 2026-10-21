//! Cost taxonomy and net-cost computation for Soroban operations.
//!
//! All amounts are **stroops** (`i64`, 1 XLM = 10^7 stroops).
//!
//! A Soroban transaction's fee has two parts:
//!
//! * the **inclusion fee**: what the network actually charged for inclusion,
//!   which is at most the bid;
//! * the **resource fee**, which is
//!   * *non-refundable* (CPU, I/O, bandwidth, historical storage), charged
//!     whenever the transaction is applied, success or failure;
//!   * *refundable* (rent for new entries / TTL extensions, events, return
//!     value), declared up front as a maximum. Whatever is not consumed is
//!     refunded after apply. A failed transaction consumes none of it.
//!
//! So `net = inclusion + non_refundable + refundable_max - refund`.
//!
//! A transaction that was never applied (rejected in preflight, or
//! rejected by the RPC/network before inclusion) costs nothing. For a
//! **fee bump**, the outer envelope pays the whole fee and the inner
//! transaction is not charged. Record one event with
//! [`OperationType::FeeBump`] for the outer envelope, and nothing for the
//! inner one, so the cost is never counted twice.

use serde::{Deserialize, Serialize};

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| v.as_str() == s)
            }
        }
    };
}

str_enum!(
    /// What kind of chain operation incurred the cost.
    OperationType {
        Submission => "submission",
        FeeBump => "fee_bump",
        RestoreFootprint => "restore_footprint",
        ExtendTtl => "extend_ttl",
        StorageWrite => "storage_write",
        UploadWasm => "upload_wasm",
    }
);

str_enum!(
    /// The business action that caused the chain traffic. This is also the
    /// budget category.
    BusinessAction {
        SettlementBatch => "settlement_batch",
        VaultDeposit => "vault_deposit",
        OraclePublish => "oracle_publish",
        KeeperRun => "keeper_run",
        EmergencyPause => "emergency_pause",
        Admin => "admin",
        Other => "other",
    }
);

str_enum!(
    /// Critical traffic (settlement completion, emergency pause) is never
    /// blocked by a budget. It is still recorded and alarmed on.
    Criticality {
        Critical => "critical",
        NonCritical => "non_critical",
    }
);

str_enum!(
    SubmissionStatus {
        Success => "success",
        FailedOnChain => "failed_on_chain",
        PreflightRejected => "preflight_rejected",
        NotIncluded => "not_included",
    }
);

impl BusinessAction {
    /// Default criticality when the caller does not say. Settlement and
    /// emergency pause must always reach the chain.
    pub fn default_criticality(self) -> Criticality {
        match self {
            BusinessAction::SettlementBatch | BusinessAction::EmergencyPause => {
                Criticality::Critical
            }
            _ => Criticality::NonCritical,
        }
    }
}

/// Links a cost to what caused it, so reports can group by action, account,
/// series or time window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    pub action: BusinessAction,
    /// The business entity, e.g. the settlement batch id or vault deposit id.
    pub action_id: String,
    pub request_id: Option<String>,
    pub job_id: Option<String>,
    pub account: Option<String>,
    pub series: Option<String>,
}

impl Attribution {
    pub fn new(action: BusinessAction, action_id: impl Into<String>) -> Self {
        Self {
            action,
            action_id: action_id.into(),
            request_id: None,
            job_id: None,
            account: None,
            series: None,
        }
    }
}

/// Fee figures as read from the submission result / transaction meta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeReport {
    /// Total fee bid (`tx.fee`, or the outer fee for a fee bump).
    pub max_fee_bid: i64,
    pub inclusion_fee_charged: i64,
    pub resource_fee_non_refundable: i64,
    /// Declared maximum refundable resource fee.
    pub resource_fee_refundable_max: i64,
    /// Refundable fee consumed by rent (new entries, TTL extensions).
    pub rent_fee_charged: i64,
    /// Refundable fee consumed by events and the return value.
    pub other_refundable_charged: i64,
    /// Authoritative `feeCharged` from the transaction result, when known.
    pub fee_charged: Option<i64>,
}

/// Computed, post-refund cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub inclusion_fee: i64,
    /// Non-refundable plus consumed refundable resource fee (including rent).
    pub resource_fee: i64,
    /// The rent/TTL part of `resource_fee`.
    pub rent_fee: i64,
    pub refund: i64,
    pub net_cost: i64,
    /// `false` when `fee_charged` disagreed with the computed figure.
    /// `net_cost` then equals `fee_charged`, which is authoritative.
    pub reconciled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CostError {
    NegativeFee(&'static str),
    /// More refundable fee was consumed than was declared.
    RefundableOverspent {
        declared: i64,
        consumed: i64,
    },
}

impl std::fmt::Display for CostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CostError::NegativeFee(field) => write!(f, "fee field `{field}` is negative"),
            CostError::RefundableOverspent { declared, consumed } => {
                write!(
                    f,
                    "refundable fee consumed ({consumed}) exceeds declared maximum ({declared})"
                )
            }
        }
    }
}

impl std::error::Error for CostError {}

/// Compute the true net cost of one submission attempt.
pub fn compute_cost(
    status: SubmissionStatus,
    fees: &FeeReport,
) -> Result<CostBreakdown, CostError> {
    for (name, v) in [
        ("max_fee_bid", fees.max_fee_bid),
        ("inclusion_fee_charged", fees.inclusion_fee_charged),
        (
            "resource_fee_non_refundable",
            fees.resource_fee_non_refundable,
        ),
        (
            "resource_fee_refundable_max",
            fees.resource_fee_refundable_max,
        ),
        ("rent_fee_charged", fees.rent_fee_charged),
        ("other_refundable_charged", fees.other_refundable_charged),
        ("fee_charged", fees.fee_charged.unwrap_or(0)),
    ] {
        if v < 0 {
            return Err(CostError::NegativeFee(name));
        }
    }

    let computed = match status {
        SubmissionStatus::PreflightRejected | SubmissionStatus::NotIncluded => CostBreakdown {
            reconciled: true,
            ..Default::default()
        },
        SubmissionStatus::FailedOnChain => {
            let refund = fees.resource_fee_refundable_max;
            CostBreakdown {
                inclusion_fee: fees.inclusion_fee_charged,
                resource_fee: fees.resource_fee_non_refundable,
                rent_fee: 0,
                refund,
                net_cost: fees.inclusion_fee_charged + fees.resource_fee_non_refundable,
                reconciled: true,
            }
        }
        SubmissionStatus::Success => {
            let consumed = fees.rent_fee_charged + fees.other_refundable_charged;
            if consumed > fees.resource_fee_refundable_max {
                return Err(CostError::RefundableOverspent {
                    declared: fees.resource_fee_refundable_max,
                    consumed,
                });
            }
            let refund = fees.resource_fee_refundable_max - consumed;
            CostBreakdown {
                inclusion_fee: fees.inclusion_fee_charged,
                resource_fee: fees.resource_fee_non_refundable + consumed,
                rent_fee: fees.rent_fee_charged,
                refund,
                net_cost: fees.inclusion_fee_charged
                    + fees.resource_fee_non_refundable
                    + fees.resource_fee_refundable_max
                    - refund,
                reconciled: true,
            }
        }
    };

    // Never-applied transactions are free whatever the RPC echoes back.
    let applied = matches!(
        status,
        SubmissionStatus::Success | SubmissionStatus::FailedOnChain
    );
    match fees.fee_charged {
        Some(charged) if applied && charged != computed.net_cost => Ok(CostBreakdown {
            net_cost: charged,
            reconciled: false,
            ..computed
        }),
        _ => Ok(computed),
    }
}

/// A cost event ready to be recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewCostEvent {
    pub operation: OperationType,
    pub status: SubmissionStatus,
    pub tx_hash: Option<String>,
    pub ledger: Option<u32>,
    pub attribution: Attribution,
    pub criticality: Criticality,
    pub max_fee_bid: i64,
    pub cost: CostBreakdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fees() -> FeeReport {
        FeeReport {
            max_fee_bid: 1_000_000,
            inclusion_fee_charged: 100,
            resource_fee_non_refundable: 50_000,
            resource_fee_refundable_max: 200_000,
            rent_fee_charged: 120_000,
            other_refundable_charged: 5_000,
            fee_charged: None,
        }
    }

    #[test]
    fn successful_invocation_nets_out_refund() {
        let c = compute_cost(SubmissionStatus::Success, &fees()).unwrap();
        assert_eq!(c.refund, 75_000);
        assert_eq!(c.rent_fee, 120_000);
        assert_eq!(c.resource_fee, 175_000);
        assert_eq!(c.net_cost, 100 + 50_000 + 125_000);
        assert!(c.reconciled);
    }

    #[test]
    fn failed_on_chain_still_costs_inclusion_and_non_refundable() {
        let c = compute_cost(SubmissionStatus::FailedOnChain, &fees()).unwrap();
        assert_eq!(c.net_cost, 50_100);
        assert_eq!(c.refund, 200_000);
        assert_eq!(c.rent_fee, 0);
    }

    #[test]
    fn never_applied_is_free() {
        for s in [
            SubmissionStatus::PreflightRejected,
            SubmissionStatus::NotIncluded,
        ] {
            let c = compute_cost(
                s,
                &FeeReport {
                    fee_charged: Some(100),
                    ..fees()
                },
            )
            .unwrap();
            assert_eq!(c.net_cost, 0);
            assert!(c.reconciled);
        }
    }

    #[test]
    fn fee_charged_is_authoritative_on_mismatch() {
        let ok = compute_cost(
            SubmissionStatus::Success,
            &FeeReport {
                fee_charged: Some(175_100),
                ..fees()
            },
        )
        .unwrap();
        assert!(ok.reconciled);
        let off = compute_cost(
            SubmissionStatus::Success,
            &FeeReport {
                fee_charged: Some(180_000),
                ..fees()
            },
        )
        .unwrap();
        assert_eq!(off.net_cost, 180_000);
        assert!(!off.reconciled);
    }

    #[test]
    fn classic_fee_bump_with_no_resource_fee() {
        // A fee-bumped classic payment: only the inclusion fee.
        let f = FeeReport {
            max_fee_bid: 400,
            inclusion_fee_charged: 200,
            fee_charged: Some(200),
            ..Default::default()
        };
        let c = compute_cost(SubmissionStatus::Success, &f).unwrap();
        assert_eq!(c.net_cost, 200);
        assert!(c.reconciled);
    }

    #[test]
    fn rejects_inconsistent_inputs() {
        assert_eq!(
            compute_cost(
                SubmissionStatus::Success,
                &FeeReport {
                    rent_fee_charged: 300_000,
                    ..fees()
                }
            ),
            Err(CostError::RefundableOverspent {
                declared: 200_000,
                consumed: 305_000
            })
        );
        assert_eq!(
            compute_cost(
                SubmissionStatus::Success,
                &FeeReport {
                    inclusion_fee_charged: -1,
                    ..fees()
                }
            ),
            Err(CostError::NegativeFee("inclusion_fee_charged"))
        );
    }

    #[test]
    fn enums_round_trip_and_default_criticality() {
        for a in BusinessAction::ALL {
            assert_eq!(BusinessAction::parse(a.as_str()), Some(*a));
        }
        for o in OperationType::ALL {
            assert_eq!(OperationType::parse(o.as_str()), Some(*o));
        }
        assert_eq!(
            BusinessAction::SettlementBatch.default_criticality(),
            Criticality::Critical
        );
        assert_eq!(
            BusinessAction::KeeperRun.default_criticality(),
            Criticality::NonCritical
        );
    }
}
