//! Configuration drift detection.
//!
//! Two checks:
//!
//! * [`check_environment`] compares one environment's **declared** state
//!   (from a bundle) with its **observed** state, read back from the network
//!   and the running service. It covers contract IDs, WASM hashes,
//!   permission flags, feature flags, limits, config, image digest and
//!   applied migration version.
//! * [`cross_environment`] compares two environments' declared state (e.g.
//!   testnet vs mainnet in one bundle). Everything must match except the
//!   paths either side lists in `network_specific`. This catches "testnet
//!   runs X while mainnet quietly runs Y".
//!
//! Reports never contain secrets: values under secret-looking paths are
//! redacted before they are stored.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::bundle::{find_secret, is_secret_name, is_secret_value, EnvironmentSpec, ReleaseBundle};

pub const REDACTED: &str = "<redacted>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    /// Declared but not observed.
    Missing,
    /// Observed but not declared.
    Unexpected,
    /// Present on both sides with different values.
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftFinding {
    pub path: String,
    pub kind: DriftKind,
    pub expected: Option<Value>,
    pub observed: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftReport {
    pub environment: String,
    /// Digest of the bundle the environment was checked against.
    pub bundle_digest: String,
    /// RFC 3339.
    pub checked_at: String,
    pub findings: Vec<DriftFinding>,
}

impl DriftReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Flatten JSON to `dotted.path -> leaf`. Arrays are leaves, so ordering
/// changes in list-valued settings count as drift.
pub fn flatten(v: &Value) -> BTreeMap<String, Value> {
    fn go(v: &Value, prefix: &str, out: &mut BTreeMap<String, Value>) {
        match v {
            Value::Object(map) if !map.is_empty() => {
                for (k, child) in map {
                    let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    go(child, &p, out);
                }
            }
            leaf => {
                out.insert(prefix.to_string(), leaf.clone());
            }
        }
    }
    let mut out = BTreeMap::new();
    go(v, "", &mut out);
    out
}

fn redact(path: &str, v: &Value) -> Value {
    let secret_path = path.split('.').any(is_secret_name);
    let secret_value = match v {
        Value::String(s) => is_secret_value(s),
        other => find_secret(other, "").is_some(),
    };
    if (secret_path && !matches!(v, Value::Bool(_) | Value::Null)) || secret_value {
        Value::String(REDACTED.into())
    } else {
        v.clone()
    }
}

/// Diff `expected` against `observed`, skipping paths in `ignore` (and
/// anything below them).
pub fn diff(expected: &Value, observed: &Value, ignore: &BTreeSet<String>) -> Vec<DriftFinding> {
    let (e, o) = (flatten(expected), flatten(observed));
    let ignored = |p: &str| ignore.iter().any(|i| p == i || p.starts_with(&format!("{i}.")));
    let paths: BTreeSet<&String> = e.keys().chain(o.keys()).collect();
    paths
        .into_iter()
        .filter(|p| !ignored(p))
        .filter_map(|p| {
            let (ev, ov) = (e.get(p), o.get(p));
            let kind = match (ev, ov) {
                (Some(a), Some(b)) if a == b => return None,
                (Some(_), Some(_)) => DriftKind::Mismatch,
                (Some(_), None) => DriftKind::Missing,
                (None, Some(_)) => DriftKind::Unexpected,
                (None, None) => return None,
            };
            Some(DriftFinding {
                path: p.clone(),
                kind,
                expected: ev.map(|v| redact(p, v)),
                observed: ov.map(|v| redact(p, v)),
            })
        })
        .collect()
}

/// The state an environment should be in once `bundle` is deployed to it.
/// This is the same shape the service's observed-state endpoint returns.
pub fn expected_state(bundle: &ReleaseBundle, env: &EnvironmentSpec) -> Value {
    json!({
        "image_digest": bundle.image_digest,
        "migrations_latest": bundle.migrations.latest,
        "network_passphrase": env.network_passphrase,
        "contracts": env.contracts,
        "permissions": env.permissions,
        "feature_flags": env.feature_flags,
        "limits": env.limits,
        "config": env.config,
    })
}

/// Compare declared vs observed for `environment`. An environment that
/// is not in the bundle is reported as a single `missing` finding at the
/// root, so it can never pass silently.
pub fn check_environment(
    bundle: &ReleaseBundle,
    bundle_digest: &str,
    environment: &str,
    observed: &Value,
    checked_at: &str,
) -> DriftReport {
    let findings = match bundle.environments.get(environment) {
        Some(env) => diff(&expected_state(bundle, env), observed, &BTreeSet::new()),
        None => vec![DriftFinding {
            path: format!("environments.{environment}"),
            kind: DriftKind::Missing,
            expected: None,
            observed: None,
        }],
    };
    DriftReport {
        environment: environment.to_string(),
        bundle_digest: bundle_digest.to_string(),
        checked_at: checked_at.to_string(),
        findings,
    }
}

/// Compare two environments' declared state, ignoring each side's
/// `network_specific` paths and the `environment` name itself.
pub fn cross_environment(a: &EnvironmentSpec, b: &EnvironmentSpec) -> Vec<DriftFinding> {
    let strip = |e: &EnvironmentSpec| {
        let mut v = serde_json::to_value(e).expect("spec serialises");
        let obj = v.as_object_mut().expect("spec is an object");
        obj.remove("environment");
        obj.remove("network_specific");
        v
    };
    let ignore: BTreeSet<String> = a.network_specific.iter().chain(&b.network_specific).cloned().collect();
    diff(&strip(a), &strip(b), &ignore)
}

#[cfg(test)]
mod tests {
    use super::super::bundle::tests::{bundle, spec};
    use super::*;

    fn observed_matching(env: &str) -> Value {
        let b = bundle("r1");
        expected_state(&b, &b.environments[env])
    }

    #[test]
    fn clean_environment_has_no_findings() {
        let b = bundle("r1");
        let r = check_environment(&b, &b.digest(), "mainnet", &observed_matching("mainnet"), "now");
        assert!(r.is_clean(), "{:?}", r.findings);
    }

    /// The CI drift gate: an injected change in deployed reality must fail.
    #[test]
    fn injected_drift_is_detected() {
        let b = bundle("r1");
        let mut observed = observed_matching("mainnet");
        observed["contracts"]["options_pool"]["wasm_hash"] = json!("c".repeat(64));
        observed["feature_flags"]["physical_settlement"] = json!(true);
        observed["permissions"].as_object_mut().unwrap().remove("upgrade_requires_multisig");
        observed["limits"]["new_limit"] = json!(5);
        observed["image_digest"] = json!("sha256:ffff");

        let r = check_environment(&b, &b.digest(), "mainnet", &observed, "now");
        let got: Vec<(&str, DriftKind)> = r.findings.iter().map(|f| (f.path.as_str(), f.kind)).collect();
        assert_eq!(
            got,
            vec![
                ("contracts.options_pool.wasm_hash", DriftKind::Mismatch),
                ("feature_flags.physical_settlement", DriftKind::Mismatch),
                ("image_digest", DriftKind::Mismatch),
                ("limits.new_limit", DriftKind::Unexpected),
                ("permissions.upgrade_requires_multisig", DriftKind::Missing),
            ]
        );
        assert!(!r.is_clean());
    }

    #[test]
    fn unknown_environment_is_drift() {
        let b = bundle("r1");
        assert!(!check_environment(&b, &b.digest(), "futurenet", &json!({}), "now").is_clean());
    }

    #[test]
    fn reports_redact_secrets() {
        let expected = json!({"config": {"vault_token": "", "rpc_url": "a"}});
        let observed = json!({"config": {"vault_token": "hvs.live-secret", "rpc_url": "b", "x": format!("S{}", "B".repeat(55))}});
        let f = diff(&expected, &observed, &BTreeSet::new());
        let text = serde_json::to_string(&f).unwrap();
        assert!(!text.contains("hvs.live-secret"));
        assert!(!text.contains("SBBB"));
        assert!(text.contains("\"b\""));
    }

    #[test]
    fn cross_environment_allows_only_network_specific_differences() {
        let (t, m) = (spec("testnet", "CTEST"), spec("mainnet", "CMAIN"));
        assert!(cross_environment(&t, &m).is_empty());

        let mut m2 = m.clone();
        m2.feature_flags.insert("physical_settlement".into(), true);
        m2.contracts.get_mut("options_pool").unwrap().wasm_hash = "d".repeat(64);
        let paths: Vec<String> = cross_environment(&t, &m2).into_iter().map(|f| f.path).collect();
        assert_eq!(paths, ["contracts.options_pool.wasm_hash", "feature_flags.physical_settlement"]);
    }

    /// Guards the committed environment files: testnet and mainnet may
    /// differ only in their declared network-specific paths.
    #[test]
    fn committed_environment_files_are_consistent() {
        let t: EnvironmentSpec =
            serde_json::from_str(include_str!("../../deploy/environments/testnet.json")).unwrap();
        let m: EnvironmentSpec =
            serde_json::from_str(include_str!("../../deploy/environments/mainnet.json")).unwrap();
        assert_eq!((t.environment.as_str(), m.environment.as_str()), ("testnet", "mainnet"));
        let drift = cross_environment(&t, &m);
        assert!(drift.is_empty(), "testnet/mainnet drift: {drift:#?}");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(super::super::bundle::find_secret(&v, ""), None);
    }
}
