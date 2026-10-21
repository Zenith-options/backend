use async_trait::async_trait;
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zenith_backend::soroban::jsonrpc::ScriptedJsonRpc;
use zenith_backend::soroban::protocol::*;

#[derive(Default)]
struct Recorder(Mutex<Vec<OperatorSignal>>);

#[async_trait]
impl RunbookHook for Recorder {
    async fn notify(&self, signal: &OperatorSignal) {
        self.0.lock().unwrap().push(signal.clone());
    }
}

fn script(rpc: &ScriptedJsonRpc, rpc_protocol: u32, ledger_protocol: u32) {
    rpc.push(
        "getVersionInfo",
        Ok(json!({"version": "22.1.0", "captiveCoreVersion": "stellar-core 22.1.0", "protocolVersion": rpc_protocol})),
    );
    rpc.push(
        "getNetwork",
        Ok(json!({"passphrase": "Test SDF Network ; September 2015", "protocolVersion": rpc_protocol})),
    );
    rpc.push(
        "getLatestLedger",
        Ok(json!({"id": "ab", "sequence": 5000, "protocolVersion": ledger_protocol})),
    );
}

fn sdk() -> ContractSdkInfo {
    ContractSdkInfo {
        sdk_version: "22.0.0".into(),
        min_protocol: 22,
        max_protocol: 22,
    }
}

async fn pool() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    ensure_schema(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn startup_succeeds_and_persists_when_compatible() {
    let rpc = Arc::new(ScriptedJsonRpc::new());
    script(&rpc, 22, 22);
    let hook = Arc::new(Recorder::default());
    let db = pool().await;
    let monitor = ProtocolMonitor::new(
        rpc,
        sdk(),
        ProtocolPolicy::default(),
        hook.clone(),
        Some(db.clone()),
    );

    let report = monitor.verify_chain_mode_startup().await.unwrap();
    assert_eq!(report.status, CompatibilityStatus::Compatible);
    assert_eq!(
        report.network.core_version.as_deref(),
        Some("stellar-core 22.1.0")
    );
    assert!(hook.0.lock().unwrap().is_empty());

    let (status,): (String,) = sqlx::query_as("SELECT status FROM protocol_observations")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(status, "compatible");
}

#[tokio::test]
async fn startup_refused_while_rpc_lags_network_upgrade() {
    let rpc = Arc::new(ScriptedJsonRpc::new());
    script(&rpc, 22, 23);
    let hook = Arc::new(Recorder::default());
    let monitor = ProtocolMonitor::new(rpc, sdk(), ProtocolPolicy::default(), hook.clone(), None);

    let err = monitor.verify_chain_mode_startup().await.unwrap_err();
    assert!(
        err.to_string().contains("rpc_ledger_protocol_mismatch"),
        "{err}"
    );
    assert!(matches!(
        hook.0.lock().unwrap()[0],
        OperatorSignal::CompatibilityDegraded {
            status: CompatibilityStatus::Incompatible,
            ..
        }
    ));
}

#[tokio::test]
async fn unreachable_rpc_invalidates_gate_and_signals() {
    let rpc = Arc::new(ScriptedJsonRpc::new());
    script(&rpc, 22, 22);
    let hook = Arc::new(Recorder::default());
    let monitor = ProtocolMonitor::new(
        rpc.clone(),
        sdk(),
        ProtocolPolicy::default(),
        hook.clone(),
        None,
    );
    monitor.check_once(unix_now()).await.unwrap();
    assert!(monitor.gate().check_chain_mode_startup(unix_now()).is_ok());

    let broken = Arc::new(ScriptedJsonRpc::new()); // no scripted responses
    let monitor =
        ProtocolMonitor::new(broken, sdk(), ProtocolPolicy::default(), hook.clone(), None);
    assert!(monitor.check_once(unix_now()).await.is_err());
    assert_eq!(
        monitor
            .gate()
            .check_chain_mode_startup(unix_now())
            .unwrap_err(),
        GateError::Unverified
    );
    assert!(matches!(
        hook.0.lock().unwrap().last(),
        Some(OperatorSignal::RpcUnavailable { .. })
    ));
}

#[tokio::test]
async fn detects_live_protocol_change_and_scheduled_upgrade() {
    let rpc = Arc::new(ScriptedJsonRpc::new());
    script(&rpc, 22, 22);
    script(&rpc, 23, 23);
    let hook = Arc::new(Recorder::default());
    let policy = ProtocolPolicy {
        scheduled_upgrade: Some(ScheduledUpgrade {
            target_protocol: 23,
            scheduled_at: 2_000,
        }),
        deploy_freeze: Duration::from_secs(3600),
        ..Default::default()
    };
    let monitor = ProtocolMonitor::new(rpc, sdk(), policy, hook.clone(), None);

    // Before: upgrade to an unsupported protocol is 1000s away -> deploys frozen.
    let before = monitor.check_once(1_000).await.unwrap();
    assert_eq!(before.status, CompatibilityStatus::DeployBlocked);
    // After: network moved to 23, which this SDK doesn't support.
    let after = monitor.check_once(2_100).await.unwrap();
    assert_eq!(after.status, CompatibilityStatus::Incompatible);

    let signals = hook.0.lock().unwrap().clone();
    assert!(signals.contains(&OperatorSignal::UpgradeScheduled {
        target_protocol: 23,
        scheduled_at: 2_000,
        sdk_supports_target: false
    }));
    assert!(signals.contains(&OperatorSignal::ProtocolChanged { from: 22, to: 23 }));
    assert!(signals
        .iter()
        .all(|s| s.runbook().starts_with(RUNBOOK_PATH)));
}
