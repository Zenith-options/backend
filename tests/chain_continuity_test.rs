use async_trait::async_trait;
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use zenith_backend::soroban::continuity::*;
use zenith_backend::soroban::jsonrpc::{JsonRpc, RpcError, ScriptedJsonRpc};

const PASS: &str = "Test SDF Network ; September 2015";

/// A fake RPC node. `fork(at, tag)` replaces history from `at` upwards with
/// a different line, as a failover to a node with another history would.
#[derive(Default)]
struct Node {
    ledgers: Mutex<BTreeMap<u32, LedgerHeader>>,
    txs: Mutex<HashMap<String, u32>>,
}

fn header(seq: u32, tag: &str, parent_tag: &str) -> LedgerHeader {
    LedgerHeader {
        sequence: seq,
        hash: format!("{tag}{seq}"),
        prev_hash: Some(format!("{parent_tag}{}", seq - 1)),
        close_time: 1_700_000_000 + seq as u64 * 5,
    }
}

impl Node {
    fn linear(tag: &str, range: std::ops::RangeInclusive<u32>) -> Self {
        let n = Node::default();
        n.extend(tag, tag, range);
        n
    }
    /// Adds ledgers tagged `tag`; the first one's parent is tagged `parent_tag`.
    fn extend(&self, tag: &str, parent_tag: &str, range: std::ops::RangeInclusive<u32>) {
        let mut l = self.ledgers.lock().unwrap();
        let start = *range.start();
        for s in range {
            l.insert(s, header(s, tag, if s == start { parent_tag } else { tag }));
        }
    }
    fn fork(&self, at: u32, tag: &str, upto: u32, parent_tag: &str) {
        self.ledgers.lock().unwrap().retain(|s, _| *s < at);
        self.extend(tag, parent_tag, at..=upto);
    }
    fn get(&self, s: u32) -> LedgerHeader {
        self.ledgers.lock().unwrap()[&s].clone()
    }
    fn put_tx(&self, hash: &str, ledger: u32) {
        self.txs.lock().unwrap().insert(hash.into(), ledger);
    }
    fn drop_tx(&self, hash: &str) {
        self.txs.lock().unwrap().remove(hash);
    }
}

#[async_trait]
impl LedgerSource for Node {
    async fn ledger(&self, s: u32) -> Result<Option<LedgerHeader>, RpcError> {
        Ok(self.ledgers.lock().unwrap().get(&s).cloned())
    }
}

#[async_trait]
impl TxLocator for Node {
    async fn locate(&self, tx: &str) -> Result<Option<TxLocation>, RpcError> {
        let seq = self.txs.lock().unwrap().get(tx).copied();
        Ok(seq.map(|s| TxLocation {
            ledger_sequence: s,
            ledger_hash: self.get(s).hash,
        }))
    }
}

#[derive(Default)]
struct Rederive {
    rederived: Mutex<Vec<(String, u32)>>,
    orphaned: Mutex<Vec<String>>,
}

#[async_trait]
impl Rederiver for Rederive {
    async fn rederive(
        &self,
        old: &DerivedRecord,
        at: &TxLocation,
    ) -> Result<NewDerivation, String> {
        self.rederived
            .lock()
            .unwrap()
            .push((old.tx_hash.clone(), at.ledger_sequence));
        Ok(NewDerivation {
            kind: old.kind.clone(),
            ref_id: old.ref_id.clone(),
            tx_hash: old.tx_hash.clone(),
            ledger_sequence: at.ledger_sequence,
            ledger_hash: at.ledger_hash.clone(),
        })
    }
    async fn orphaned(&self, old: &DerivedRecord) -> Result<(), String> {
        self.orphaned.lock().unwrap().push(old.tx_hash.clone());
        Ok(())
    }
}

async fn tracker(
    passphrase: &str,
    db: Option<sqlx::SqlitePool>,
) -> (ContinuityTracker, sqlx::SqlitePool) {
    let db = match db {
        Some(db) => db,
        None => {
            let db = SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .unwrap();
            ensure_schema(&db).await.unwrap();
            db
        }
    };
    let cfg = ContinuityConfig {
        passphrase: passphrase.into(),
        max_reorg_depth: 16,
    };
    (ContinuityTracker::new(db.clone(), cfg), db)
}

async fn sync(t: &ContinuityTracker, node: &Node, range: std::ops::RangeInclusive<u32>) {
    for s in range {
        let v = t.observe(&node.get(s), node).await.unwrap();
        assert!(
            matches!(v, Verdict::Anchored | Verdict::Extends),
            "ledger {s}: {v:?}"
        );
    }
}

async fn derive(t: &ContinuityTracker, node: &Node, tx: &str, ledger: u32) -> String {
    node.put_tx(tx, ledger);
    t.record_derivation(&NewDerivation {
        kind: "settlement".into(),
        ref_id: format!("pos-{tx}"),
        tx_hash: tx.into(),
        ledger_sequence: ledger,
        ledger_hash: node.get(ledger).hash,
    })
    .await
    .unwrap()
}

fn discontinuity(v: Verdict) -> Discontinuity {
    match v {
        Verdict::Discontinuity(d) => d,
        other => panic!("expected discontinuity, got {other:?}"),
    }
}

#[tokio::test]
async fn normal_progress_and_benign_observations() {
    let node = Node::linear("a", 1..=6);
    let (t, _) = tracker(PASS, None).await;
    sync(&t, &node, 1..=4).await;

    assert_eq!(
        t.observe(&node.get(4), &node).await.unwrap(),
        Verdict::Duplicate
    );
    assert_eq!(
        t.observe(&node.get(2), &node).await.unwrap(),
        Verdict::StaleView
    );
    assert_eq!(
        t.observe(&node.get(6), &node).await.unwrap(),
        Verdict::Gap { from: 5, to: 5 }
    );
    assert_eq!(t.tip().await.unwrap(), Some((4, "a4".into())));

    // Can't record state against a hash that isn't canonical.
    let err = t
        .record_derivation(&NewDerivation {
            kind: "settlement".into(),
            ref_id: "x".into(),
            tx_hash: "tx".into(),
            ledger_sequence: 3,
            ledger_hash: "zz3".into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ContinuityError::NonCanonicalLedger { sequence: 3, .. }
    ));
    assert!(t.discontinuities().await.unwrap().is_empty());
}

#[tokio::test]
async fn aborted_close_reverts_and_rederives_from_replacement_ledger() {
    let node = Node::linear("a", 1..=5);
    let (t, _) = tracker(PASS, None).await;
    sync(&t, &node, 1..=5).await;
    let kept = derive(&t, &node, "tx4", 4).await;
    let moved = derive(&t, &node, "tx5", 5).await;
    let gone = derive(&t, &node, "tx5b", 5).await;

    // Ledger 5 was rolled back: the node now has b5 (parent a4) and b6.
    node.fork(5, "b", 6, "a");
    node.put_tx("tx5", 6); // re-included one ledger later
    node.drop_tx("tx5b"); // never made it into the canonical chain

    let d = discontinuity(t.observe(&node.get(6), &node).await.unwrap());
    assert_eq!(d.kind, DiscontinuityKind::AbortedClose);
    assert_eq!(d.fork_sequence, Some(5));
    assert_eq!(d.expected_hash.as_deref(), Some("a5"));
    assert_eq!(d.discarded, vec![(5, "a5".into())]);
    assert_eq!(d.reverted_records, 2);
    assert_eq!(d.epoch_after, d.epoch_before);

    assert_eq!(
        t.record(&kept).await.unwrap().unwrap().status,
        RecordStatus::Active
    );
    let rev = t.record(&moved).await.unwrap().unwrap();
    assert_eq!(rev.status, RecordStatus::Reverted);
    assert_eq!(rev.reverted_by.as_deref(), Some(d.id.as_str()));

    // Re-derivation waits until the canonical lineage covers the new ledger.
    let rd = Rederive::default();
    let s = t.reconcile_reverted(&node, &rd, 100).await.unwrap();
    assert_eq!(
        s,
        ReconcileSummary {
            rederived: 0,
            orphaned: 1,
            pending: 1
        }
    );
    assert_eq!(*rd.orphaned.lock().unwrap(), vec!["tx5b".to_string()]);
    assert_eq!(
        t.record(&gone).await.unwrap().unwrap().status,
        RecordStatus::Orphaned
    );

    sync(&t, &node, 5..=6).await;
    assert_eq!(t.tip().await.unwrap(), Some((6, "b6".into())));
    let s = t.reconcile_reverted(&node, &rd, 100).await.unwrap();
    assert_eq!(
        s,
        ReconcileSummary {
            rederived: 1,
            orphaned: 0,
            pending: 0
        }
    );
    assert_eq!(*rd.rederived.lock().unwrap(), vec![("tx5".to_string(), 6)]);

    let old = t.record(&moved).await.unwrap().unwrap();
    assert_eq!(old.status, RecordStatus::Rederived);
    let new = t
        .record(old.rederived_as.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (new.status, new.ledger_sequence, new.ledger_hash.as_str()),
        (RecordStatus::Active, 6, "b6")
    );
    assert!(t
        .records_with_status(RecordStatus::Reverted)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn hash_mismatch_at_tip_and_deeper_regression_find_common_ancestor() {
    let node = Node::linear("a", 1..=10);
    let (t, _) = tracker(PASS, None).await;
    sync(&t, &node, 1..=10).await;
    let r6 = derive(&t, &node, "t6", 6).await;
    let r7 = derive(&t, &node, "t7", 7).await;
    let r10 = derive(&t, &node, "t10", 10).await;

    // Same height, different hash.
    node.fork(10, "b", 10, "a");
    let d = discontinuity(t.observe(&node.get(10), &node).await.unwrap());
    assert_eq!(
        (d.kind, d.fork_sequence),
        (DiscontinuityKind::HashMismatch, Some(10))
    );
    assert_eq!(t.tip().await.unwrap(), Some((10, "b10".into())));
    assert_eq!(
        t.record(&r10).await.unwrap().unwrap().status,
        RecordStatus::Reverted
    );

    // RPC regresses to ledger 8 of a line that diverged at 7.
    node.fork(7, "c", 8, "a");
    let d = discontinuity(t.observe(&node.get(8), &node).await.unwrap());
    assert_eq!(d.kind, DiscontinuityKind::SequenceRegression);
    assert_eq!(
        d.fork_sequence,
        Some(7),
        "walked back to the common ancestor a6"
    );
    assert_eq!(
        d.discarded.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![7, 8, 9, 10]
    );
    assert_eq!(
        t.record(&r6).await.unwrap().unwrap().status,
        RecordStatus::Active
    );
    assert_eq!(
        t.record(&r7).await.unwrap().unwrap().status,
        RecordStatus::Reverted
    );
    assert_eq!(t.tip().await.unwrap(), Some((6, "a6".into())));

    let kinds: Vec<String> = t
        .discontinuities()
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.kind)
        .collect();
    assert_eq!(kinds, vec!["hash_mismatch", "sequence_regression"]);
}

#[tokio::test]
async fn network_resets_revert_everything_and_start_a_new_epoch() {
    // Testnet reset: the chain restarts from genesis.
    let node = Node::linear("a", 100..=120);
    let (t, db) = tracker(PASS, None).await;
    sync(&t, &node, 100..=120).await;
    let r = derive(&t, &node, "old", 110).await;

    let fresh = Node::linear("n", 2..=5);
    let d = discontinuity(t.observe(&fresh.get(5), &fresh).await.unwrap());
    assert_eq!(
        (d.kind, d.fork_sequence),
        (DiscontinuityKind::NetworkReset, None)
    );
    assert_eq!((d.epoch_before, d.epoch_after), (0, 1));
    assert_eq!(d.discarded.len(), 21);
    assert_eq!(
        t.record(&r).await.unwrap().unwrap().status,
        RecordStatus::Reverted
    );
    assert_eq!(t.tip().await.unwrap(), Some((5, "n5".into())));
    assert_eq!(t.epoch().await.unwrap(), 1);

    // A fork deeper than max_reorg_depth (16) is also treated as a reset.
    let deep = Node::linear("x", 5..=60);
    let (t2, _) = tracker(PASS, None).await;
    sync(&t2, &deep, 5..=60).await;
    deep.fork(20, "y", 60, "x");
    let d = discontinuity(t2.observe(&deep.get(60), &deep).await.unwrap());
    assert_eq!(d.kind, DiscontinuityKind::NetworkReset);

    // Same database, but the backend now points at a different network.
    let (t3, _) = tracker("Public Global Stellar Network ; September 2015", Some(db)).await;
    let d = discontinuity(t3.observe(&fresh.get(5), &fresh).await.unwrap());
    assert_eq!(
        (d.kind, d.epoch_after),
        (DiscontinuityKind::NetworkReset, 2)
    );
}

#[tokio::test]
async fn rpc_source_reads_ledgers_and_transactions() {
    let rpc = Arc::new(ScriptedJsonRpc::new());
    rpc.push(
        "getLatestLedger",
        Ok(json!({"id": "h9", "sequence": 9, "protocolVersion": 22})),
    );
    rpc.push(
        "getLedgers",
        Ok(json!({"ledgers": [{"hash": "h9", "sequence": 9, "ledgerCloseTime": "1700000045",
                               "headerJson": {"hash": "h9", "header": {"previous_ledger_hash": "h8"}}}]})),
    );
    rpc.push(
        "getTransaction",
        Ok(json!({"status": "SUCCESS", "ledger": 9})),
    );
    let src = RpcChainSource::new(rpc.clone() as Arc<dyn JsonRpc>);

    let latest = src.latest().await.unwrap();
    assert_eq!(latest.prev_hash.as_deref(), Some("h8"));
    assert_eq!(
        src.locate("abc").await.unwrap(),
        Some(TxLocation {
            ledger_sequence: 9,
            ledger_hash: "h9".into()
        })
    );
    // Asking for a ledger the response doesn't contain yields None.
    assert_eq!(src.ledger(3).await.unwrap(), None);

    let missing = ScriptedJsonRpc::new();
    missing.push("getTransaction", Ok(json!({"status": "NOT_FOUND"})));
    assert_eq!(
        RpcChainSource::new(Arc::new(missing))
            .locate("abc")
            .await
            .unwrap(),
        None
    );
}
