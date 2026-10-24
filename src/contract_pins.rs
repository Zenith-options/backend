//! Pinned contract hashes: proof that the deployed contract is the reviewed one.
//!
//! Once settlement contracts hold collateral, "the audited contract" and
//! "the deployed contract" must be the same verifiable claim. This module
//! makes it one:
//!
//! * `deploy/contract-pins.json` records, for each contract and network,
//!   the SHA-256 of the reviewed WASM, the source commit it was built from
//!   and the digest-pinned container image used to build it. The manifest
//!   is reviewed like code. Changing a pin is a reviewed change.
//! * [`PinManifest::verify_artifact`] is the pre-deploy check. A locally
//!   built WASM whose hash is not pinned is refused, so a stale or tampered
//!   build artifact cannot be deployed unnoticed. `scripts/contracts.sh
//!   deploy` runs the same check before calling `stellar contract deploy`.
//! * [`PinGuard`] is the runtime check. At startup it confirms that every
//!   pinned contract ID on this network runs the pinned WASM, and
//!   [`PinGuard::ensure_live`] re-checks a contract immediately before
//!   funds are routed to it. Soroban contracts can be upgraded in place, so
//!   a hash that matched at startup is not enough.
//!
//! A Soroban contract's WASM hash *is* the SHA-256 of the installed WASM
//! bytes, so the hash computed here from a local file and the hash the
//! network reports are directly comparable.
//!
//! See `docs/contract-builds.md` for the reproducible build procedure.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_MANIFEST_PATH: &str = "deploy/contract-pins.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractPin {
    /// Logical contract name (`vault`, `options`, `oracle`, ...).
    pub name: String,
    /// `testnet`, `mainnet`, `futurenet` or `standalone`.
    pub network: String,
    /// Deployed contract ID (`C...`). Absent until the contract is deployed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_id: Option<String>,
    /// Lower-case hex SHA-256 of the reviewed WASM.
    pub wasm_sha256: String,
    pub source_repo: String,
    /// Full 40-character git commit the WASM was built from.
    pub source_commit: String,
    /// Build container, pinned by digest (`image@sha256:...`), never a tag.
    pub build_image: String,
    /// Audit report or review reference covering this exact hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_ref: Option<String>,
    /// File name of the built WASM under `target/<target>/release/`.
    /// Defaults to `<name>.wasm` with `-` replaced by `_`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

impl ContractPin {
    pub fn artifact_name(&self) -> String {
        self.artifact
            .clone()
            .unwrap_or_else(|| format!("{}.wasm", self.name.replace('-', "_")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinManifest {
    pub schema_version: u32,
    pub contracts: Vec<ContractPin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    InvalidManifest(String),
    /// No pin exists for this contract name/ID on this network.
    NotPinned(String),
    HashMismatch {
        name: String,
        network: String,
        contract_id: Option<String>,
        expected: String,
        actual: String,
    },
    /// The deployed hash could not be fetched.
    Source(String),
    Multiple(Vec<PinError>),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidManifest(m) => write!(f, "invalid contract pin manifest: {m}"),
            Self::NotPinned(m) => write!(f, "contract not pinned: {m}"),
            Self::HashMismatch {
                name,
                network,
                contract_id,
                expected,
                actual,
            } => write!(
                f,
                "WASM hash mismatch for {name} on {network}{}: pinned {expected}, found {actual}",
                contract_id
                    .as_deref()
                    .map(|id| format!(" ({id})"))
                    .unwrap_or_default()
            ),
            Self::Source(m) => write!(f, "cannot fetch deployed WASM hash: {m}"),
            Self::Multiple(errors) => {
                let parts: Vec<String> = errors.iter().map(ToString::to_string).collect();
                write!(f, "{}", parts.join("; "))
            }
        }
    }
}

impl std::error::Error for PinError {}

/// Lower-case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

const NETWORKS: [&str; 4] = ["testnet", "mainnet", "futurenet", "standalone"];

impl PinManifest {
    pub fn from_json(json: &str) -> Result<Self, PinError> {
        let manifest: Self =
            serde_json::from_str(json).map_err(|e| PinError::InvalidManifest(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(path: &Path) -> Result<Self, PinError> {
        let json = std::fs::read_to_string(path).map_err(|e| {
            PinError::InvalidManifest(format!("cannot read {}: {e}", path.display()))
        })?;
        Self::from_json(&json)
    }

    /// Structural checks: a manifest that passes these can be trusted to
    /// mean what it says. All problems are reported at once.
    pub fn validate(&self) -> Result<(), PinError> {
        let mut problems = Vec::new();
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            problems.push(format!(
                "schema_version {} unsupported (expected {MANIFEST_SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        let mut seen = HashSet::new();
        let mut ids = HashSet::new();
        for pin in &self.contracts {
            let at = format!("{}@{}", pin.name, pin.network);
            if pin.name.trim().is_empty() {
                problems.push("contract with empty name".into());
            }
            if !NETWORKS.contains(&pin.network.as_str()) {
                problems.push(format!("{at}: unknown network"));
            }
            if !seen.insert((pin.name.as_str(), pin.network.as_str())) {
                problems.push(format!("{at}: duplicate pin"));
            }
            if !is_lower_hex(&pin.wasm_sha256, 64) {
                problems.push(format!(
                    "{at}: wasm_sha256 must be 64 lower-case hex characters"
                ));
            }
            if !is_lower_hex(&pin.source_commit, 40) {
                problems.push(format!(
                    "{at}: source_commit must be a full 40-character commit hash"
                ));
            }
            if pin.source_repo.trim().is_empty() {
                problems.push(format!("{at}: source_repo is required"));
            }
            match pin.build_image.split_once("@sha256:") {
                Some((image, digest)) if !image.is_empty() && is_lower_hex(digest, 64) => {}
                _ => problems.push(format!(
                    "{at}: build_image must be pinned by digest (image@sha256:<64 hex>), not a tag"
                )),
            }
            if let Some(artifact) = &pin.artifact {
                if !artifact.ends_with(".wasm") || artifact.contains('/') || artifact.contains("..")
                {
                    problems.push(format!("{at}: artifact must be a bare *.wasm file name"));
                }
            }
            if let Some(id) = &pin.contract_id {
                if !(id.len() == 56
                    && id.starts_with('C')
                    && id.bytes().all(|b| b.is_ascii_alphanumeric()))
                {
                    problems.push(format!(
                        "{at}: contract_id must be a 56-character C... strkey"
                    ));
                }
                if !ids.insert((id.as_str(), pin.network.as_str())) {
                    problems.push(format!("{at}: contract_id pinned twice on {}", pin.network));
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(PinError::InvalidManifest(problems.join("; ")))
        }
    }

    pub fn pin(&self, name: &str, network: &str) -> Option<&ContractPin> {
        self.contracts
            .iter()
            .find(|p| p.name == name && p.network == network)
    }

    /// Pre-deploy check: refuses a WASM whose hash is not the pinned one.
    pub fn verify_artifact(
        &self,
        name: &str,
        network: &str,
        wasm: &[u8],
    ) -> Result<&ContractPin, PinError> {
        let pin = self
            .pin(name, network)
            .ok_or_else(|| PinError::NotPinned(format!("{name} on {network}")))?;
        let actual = sha256_hex(wasm);
        if actual != pin.wasm_sha256 {
            return Err(PinError::HashMismatch {
                name: name.into(),
                network: network.into(),
                contract_id: pin.contract_id.clone(),
                expected: pin.wasm_sha256.clone(),
                actual,
            });
        }
        Ok(pin)
    }
}

/// Where the hash of a deployed contract's WASM comes from.
#[async_trait]
pub trait WasmHashSource: Send + Sync {
    async fn deployed_wasm_hash(&self, contract_id: &str) -> Result<String, String>;
}

/// Fetches deployed WASM with the Stellar CLI
/// (`stellar contract fetch --id <ID> --network <NET> --out-file <tmp>`)
/// and hashes it locally. Requires the `stellar` binary on `PATH` (or at
/// `cli`) with the target network configured.
pub struct StellarCliHashSource {
    pub cli: PathBuf,
    pub network: String,
}

impl StellarCliHashSource {
    pub fn new(network: impl Into<String>) -> Self {
        Self {
            cli: PathBuf::from("stellar"),
            network: network.into(),
        }
    }
}

#[async_trait]
impl WasmHashSource for StellarCliHashSource {
    async fn deployed_wasm_hash(&self, contract_id: &str) -> Result<String, String> {
        let out = std::env::temp_dir().join(format!(
            "zenith-fetch-{}-{}.wasm",
            contract_id,
            std::process::id()
        ));
        let status = tokio::process::Command::new(&self.cli)
            .args([
                "contract",
                "fetch",
                "--id",
                contract_id,
                "--network",
                &self.network,
                "--out-file",
            ])
            .arg(&out)
            .status()
            .await
            .map_err(|e| format!("cannot run {}: {e}", self.cli.display()))?;
        if !status.success() {
            let _ = std::fs::remove_file(&out);
            return Err(format!(
                "stellar contract fetch {contract_id} exited with {status}"
            ));
        }
        let bytes = std::fs::read(&out).map_err(|e| format!("cannot read fetched WASM: {e}"));
        let _ = std::fs::remove_file(&out);
        Ok(sha256_hex(&bytes?))
    }
}

/// Runtime guard over the pins for one network.
///
/// Build it with [`PinGuard::verify`] at startup; refuse to start (or to
/// move funds) if that fails. Before each value-moving call into a
/// contract, call [`PinGuard::ensure_live`].
#[derive(Debug, Clone)]
pub struct PinGuard {
    network: String,
    by_contract_id: HashMap<String, ContractPin>,
}

impl PinGuard {
    /// Checks every deployed (contract-ID-bearing) pin for `network` against
    /// the network. All mismatches are reported together.
    pub async fn verify(
        manifest: &PinManifest,
        network: &str,
        source: &dyn WasmHashSource,
    ) -> Result<Self, PinError> {
        let mut by_contract_id = HashMap::new();
        let mut errors = Vec::new();
        for pin in manifest.contracts.iter().filter(|p| p.network == network) {
            let Some(id) = &pin.contract_id else { continue };
            match check(pin, id, source).await {
                Ok(()) => {
                    by_contract_id.insert(id.clone(), pin.clone());
                }
                Err(e) => errors.push(e),
            }
        }
        match errors.len() {
            0 => Ok(Self {
                network: network.into(),
                by_contract_id,
            }),
            1 => Err(errors.pop().expect("one error")),
            _ => Err(PinError::Multiple(errors)),
        }
    }

    pub fn network(&self) -> &str {
        &self.network
    }

    /// The pin for a contract verified at startup. Unknown IDs are refused:
    /// funds are never routed to an unpinned contract.
    pub fn pinned(&self, contract_id: &str) -> Result<&ContractPin, PinError> {
        self.by_contract_id
            .get(contract_id)
            .ok_or_else(|| PinError::NotPinned(format!("{contract_id} on {}", self.network)))
    }

    /// Re-fetches the deployed hash and compares it to the pin. Call
    /// immediately before moving funds through the contract.
    pub async fn ensure_live(
        &self,
        contract_id: &str,
        source: &dyn WasmHashSource,
    ) -> Result<&ContractPin, PinError> {
        let pin = self.pinned(contract_id)?;
        check(pin, contract_id, source).await?;
        Ok(pin)
    }
}

async fn check(
    pin: &ContractPin,
    contract_id: &str,
    source: &dyn WasmHashSource,
) -> Result<(), PinError> {
    let actual = source
        .deployed_wasm_hash(contract_id)
        .await
        .map_err(|e| PinError::Source(format!("{} ({contract_id}): {e}", pin.name)))?
        .to_ascii_lowercase();
    if actual != pin.wasm_sha256 {
        return Err(PinError::HashMismatch {
            name: pin.name.clone(),
            network: pin.network.clone(),
            contract_id: Some(contract_id.into()),
            expected: pin.wasm_sha256.clone(),
            actual,
        });
    }
    Ok(())
}

/// In-memory hash source for tests and rehearsals.
#[derive(Debug, Default, Clone)]
pub struct StaticHashSource(pub HashMap<String, String>);

#[async_trait]
impl WasmHashSource for StaticHashSource {
    async fn deployed_wasm_hash(&self, contract_id: &str) -> Result<String, String> {
        self.0
            .get(contract_id)
            .cloned()
            .ok_or_else(|| format!("no contract {contract_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT_ID: &str = "CVAULTAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const IMAGE: &str =
        "docker.io/library/rust@sha256:0000000000000000000000000000000000000000000000000000000000000000";

    fn pin(name: &str, wasm: &[u8], contract_id: Option<&str>) -> ContractPin {
        ContractPin {
            name: name.into(),
            network: "testnet".into(),
            contract_id: contract_id.map(Into::into),
            wasm_sha256: sha256_hex(wasm),
            source_repo: "https://github.com/Zenith-options/contracts".into(),
            source_commit: "a".repeat(40),
            build_image: IMAGE.into(),
            audit_ref: None,
            artifact: None,
        }
    }

    fn manifest(pins: Vec<ContractPin>) -> PinManifest {
        PinManifest {
            schema_version: 1,
            contracts: pins,
        }
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn shipped_manifest_is_valid() {
        PinManifest::from_json(include_str!("../deploy/contract-pins.json")).unwrap();
    }

    #[test]
    fn validation_rejects_weak_pins() {
        let mut p = pin("vault", b"wasm", None);
        p.build_image = "rust:1.84".into();
        p.source_commit = "main".into();
        p.wasm_sha256 = p.wasm_sha256.to_uppercase();
        p.artifact = Some("../escape.wasm".into());
        let err = manifest(vec![p.clone(), p])
            .validate()
            .unwrap_err()
            .to_string();
        for expected in [
            "digest",
            "40-character",
            "64 lower-case",
            "duplicate",
            "bare *.wasm",
        ] {
            assert!(err.contains(expected), "{err} should mention {expected}");
        }
        assert!(PinManifest::from_json(r#"{"schema_version":2,"contracts":[]}"#).is_err());
    }

    #[test]
    fn artifact_name_defaults_to_crate_file() {
        let mut p = pin("zenith-vault", b"w", None);
        assert_eq!(p.artifact_name(), "zenith_vault.wasm");
        p.artifact = Some("vault_optimized.wasm".into());
        assert_eq!(p.artifact_name(), "vault_optimized.wasm");
    }

    #[test]
    fn artifact_must_match_pin() {
        let m = manifest(vec![pin("vault", b"reviewed", None)]);
        assert!(m.verify_artifact("vault", "testnet", b"reviewed").is_ok());
        assert!(matches!(
            m.verify_artifact("vault", "testnet", b"stale build"),
            Err(PinError::HashMismatch { .. })
        ));
        assert!(matches!(
            m.verify_artifact("vault", "mainnet", b"reviewed"),
            Err(PinError::NotPinned(_))
        ));
    }

    #[tokio::test]
    async fn guard_checks_deployed_hashes_and_refuses_unpinned() {
        let m = manifest(vec![
            pin("vault", b"reviewed", Some(VAULT_ID)),
            pin("oracle", b"x", None),
        ]);
        let mut chain = StaticHashSource::default();
        chain.0.insert(VAULT_ID.into(), sha256_hex(b"reviewed"));

        let guard = PinGuard::verify(&m, "testnet", &chain).await.unwrap();
        assert_eq!(
            guard.ensure_live(VAULT_ID, &chain).await.unwrap().name,
            "vault"
        );
        let other = "C".to_string() + &"B".repeat(55);
        assert!(matches!(guard.pinned(&other), Err(PinError::NotPinned(_))));

        // Contract upgraded in place after startup: the live check catches it.
        chain.0.insert(VAULT_ID.into(), sha256_hex(b"upgraded"));
        assert!(matches!(
            guard.ensure_live(VAULT_ID, &chain).await,
            Err(PinError::HashMismatch { .. })
        ));
        // And startup verification refuses outright.
        assert!(PinGuard::verify(&m, "testnet", &chain).await.is_err());
        // Unreachable source is an error, never a pass.
        assert!(matches!(
            PinGuard::verify(&m, "testnet", &StaticHashSource::default()).await,
            Err(PinError::Source(_))
        ));
    }
}
