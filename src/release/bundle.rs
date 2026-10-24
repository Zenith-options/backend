//! Immutable, signed release bundles.
//!
//! A bundle pins everything a release consists of:
//!
//! * the backend image digest (`sha256:...`, never a mutable tag);
//! * the migration set (latest version, a digest over every migration file,
//!   and which migrations are forward-only, i.e. have no `.down.sql`);
//! * for each network: contract IDs and WASM hashes, key permission flags,
//!   feature flags, limit thresholds, and the rest of the effective config.
//!
//! The bundle is serialised as **canonical JSON** (object keys sorted, no
//! whitespace) and identified by `sha256:<hex>` of those bytes. The digest
//! is signed with an ed25519 release key. Changing any byte changes the
//! digest and invalidates the signature, which is what makes a bundle
//! immutable.
//!
//! Bundles must never contain secrets. [`ReleaseBundle::new`] refuses to
//! build one if any value looks like a credential.

use std::collections::BTreeMap;
use std::path::Path;

use data_encoding::{BASE64, HEXLOWER};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// A config value looks like a secret. Carries the path only, never the value.
    SecretInBundle(String),
    InvalidImageDigest(String),
    InvalidWasmHash { contract: String },
    Io(String),
    Parse(String),
    BadKey(String),
    UnknownSigningKey(String),
    DigestMismatch { claimed: String, actual: String },
    BadSignature,
    AlreadyExists(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleError::SecretInBundle(p) => write!(f, "refusing to bundle a secret-looking value at `{p}`"),
            BundleError::InvalidImageDigest(d) => write!(f, "image must be pinned by sha256 digest, got `{d}`"),
            BundleError::InvalidWasmHash { contract } => {
                write!(f, "contract `{contract}` wasm_hash must be 64 lowercase hex chars")
            }
            BundleError::Io(e) => write!(f, "io: {e}"),
            BundleError::Parse(e) => write!(f, "parse: {e}"),
            BundleError::BadKey(e) => write!(f, "bad key: {e}"),
            BundleError::UnknownSigningKey(k) => write!(f, "bundle signed by untrusted key `{k}`"),
            BundleError::DigestMismatch { claimed, actual } => {
                write!(f, "bundle digest mismatch: claimed {claimed}, actual {actual}")
            }
            BundleError::BadSignature => write!(f, "bundle signature does not verify"),
            BundleError::AlreadyExists(p) => write!(f, "{p} already exists; bundles are immutable"),
        }
    }
}

impl std::error::Error for BundleError {}

/// One deployed contract on one network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractRef {
    pub contract_id: String,
    /// Hex SHA-256 of the installed WASM.
    pub wasm_hash: String,
}

/// Declared desired state of one environment (`deploy/environments/<env>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentSpec {
    pub environment: String,
    pub network_passphrase: String,
    pub contracts: BTreeMap<String, ContractRef>,
    /// Key permission flags, e.g. who is contract admin, whether upgrades
    /// require multisig.
    #[serde(default)]
    pub permissions: BTreeMap<String, Value>,
    #[serde(default)]
    pub feature_flags: BTreeMap<String, bool>,
    /// Limit thresholds (position caps, oracle staleness, budgets).
    #[serde(default)]
    pub limits: BTreeMap<String, Value>,
    /// Remaining effective (non-secret) configuration.
    #[serde(default)]
    pub config: BTreeMap<String, Value>,
    /// Dotted paths that are *expected* to differ between environments
    /// (contract IDs, passphrase, RPC URL). Everything else must match.
    #[serde(default)]
    pub network_specific: Vec<String>,
}

impl EnvironmentSpec {
    pub fn load(path: &Path) -> Result<Self, BundleError> {
        let text = std::fs::read_to_string(path).map_err(|e| BundleError::Io(format!("{}: {e}", path.display())))?;
        serde_json::from_str(&text).map_err(|e| BundleError::Parse(format!("{}: {e}", path.display())))
    }
}

/// The pinned migration set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationSet {
    /// Highest migration file stem, e.g. `0014_promotion_registry`.
    pub latest: String,
    /// Every up-migration stem, in apply order.
    pub files: Vec<String>,
    /// `sha256:<hex>` over every migration file name and content (up and down).
    pub digest: String,
    /// Up-migrations without a matching `.down.sql`.
    pub forward_only: Vec<String>,
}

impl MigrationSet {
    /// Read `dir` (e.g. `migrations/`). Subdirectories are ignored.
    pub fn from_dir(dir: &Path) -> Result<Self, BundleError> {
        let io = |e: std::io::Error| BundleError::Io(format!("{}: {e}", dir.display()));
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map_err(io)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        names.sort();
        let mut hasher = Sha256::new();
        for name in &names {
            let content = std::fs::read(dir.join(name)).map_err(io)?;
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update(&content);
            hasher.update([0]);
        }
        let ups: Vec<String> = names
            .iter()
            .filter(|n| !n.ends_with(".down.sql"))
            .map(|n| n.trim_end_matches(".sql").to_string())
            .collect();
        let forward_only = ups
            .iter()
            .filter(|stem| !names.contains(&format!("{stem}.down.sql")))
            .cloned()
            .collect();
        Ok(MigrationSet {
            latest: ups.last().cloned().unwrap_or_default(),
            files: ups,
            digest: format!("sha256:{}", HEXLOWER.encode(&hasher.finalize())),
            forward_only,
        })
    }
}

/// The release bundle (unsigned payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseBundle {
    pub schema_version: u32,
    pub release_id: String,
    pub git_commit: String,
    /// RFC 3339, supplied by CI.
    pub created_at: String,
    pub image_digest: String,
    pub migrations: MigrationSet,
    /// Keyed by environment name (`testnet`, `mainnet`).
    pub environments: BTreeMap<String, EnvironmentSpec>,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl ReleaseBundle {
    pub fn new(
        release_id: impl Into<String>,
        git_commit: impl Into<String>,
        created_at: impl Into<String>,
        image_digest: impl Into<String>,
        migrations: MigrationSet,
        environments: Vec<EnvironmentSpec>,
    ) -> Result<Self, BundleError> {
        let bundle = ReleaseBundle {
            schema_version: BUNDLE_SCHEMA_VERSION,
            release_id: release_id.into(),
            git_commit: git_commit.into(),
            created_at: created_at.into(),
            image_digest: image_digest.into(),
            migrations,
            environments: environments.into_iter().map(|e| (e.environment.clone(), e)).collect(),
        };
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn validate(&self) -> Result<(), BundleError> {
        let d = self.image_digest.strip_prefix("sha256:").unwrap_or("");
        if !is_hex64(d) {
            return Err(BundleError::InvalidImageDigest(self.image_digest.clone()));
        }
        for env in self.environments.values() {
            for (name, c) in &env.contracts {
                if !is_hex64(&c.wasm_hash) {
                    return Err(BundleError::InvalidWasmHash { contract: format!("{}.{name}", env.environment) });
                }
            }
        }
        let value = serde_json::to_value(self).expect("bundle serialises");
        if let Some(path) = find_secret(&value, "") {
            return Err(BundleError::SecretInBundle(path));
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_json(&serde_json::to_value(self).expect("bundle serialises")).into_bytes()
    }

    pub fn digest(&self) -> String {
        format!("sha256:{}", HEXLOWER.encode(&Sha256::digest(self.canonical_bytes())))
    }

    pub fn sign(self, key: &SigningKey) -> SignedBundle {
        let digest = self.digest();
        let signature = key.sign(digest.as_bytes());
        SignedBundle {
            key_id: key_id(&key.verifying_key()),
            signature: BASE64.encode(&signature.to_bytes()),
            digest,
            bundle: self,
        }
    }
}

/// A bundle plus its digest and detached signature. This is the file that
/// is published and promoted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedBundle {
    pub bundle: ReleaseBundle,
    pub digest: String,
    pub key_id: String,
    /// Base64 ed25519 signature over the ASCII digest string.
    pub signature: String,
}

/// Trusted release verification keys: `key_id -> base64 public key`.
pub type TrustedKeys = BTreeMap<String, String>;

impl SignedBundle {
    /// Recompute the digest, check the signer is trusted, verify the signature.
    pub fn verify(&self, trusted: &TrustedKeys) -> Result<(), BundleError> {
        let actual = self.bundle.digest();
        if actual != self.digest {
            return Err(BundleError::DigestMismatch { claimed: self.digest.clone(), actual });
        }
        let pk_b64 = trusted.get(&self.key_id).ok_or_else(|| BundleError::UnknownSigningKey(self.key_id.clone()))?;
        let key = verifying_key_from_b64(pk_b64)?;
        if key_id(&key) != self.key_id {
            return Err(BundleError::BadKey(format!("trusted key `{}` does not match its id", self.key_id)));
        }
        let sig_bytes: [u8; 64] = BASE64
            .decode(self.signature.as_bytes())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(BundleError::BadSignature)?;
        key.verify(self.digest.as_bytes(), &Signature::from_bytes(&sig_bytes))
            .map_err(|_| BundleError::BadSignature)?;
        self.bundle.validate()
    }

    pub fn load(path: &Path) -> Result<Self, BundleError> {
        let text = std::fs::read_to_string(path).map_err(|e| BundleError::Io(format!("{}: {e}", path.display())))?;
        serde_json::from_str(&text).map_err(|e| BundleError::Parse(format!("{}: {e}", path.display())))
    }

    /// Write to `path`, refusing to overwrite an existing file.
    pub fn write_new(&self, path: &Path) -> Result<(), BundleError> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                BundleError::AlreadyExists(path.display().to_string())
            } else {
                BundleError::Io(e.to_string())
            }
        })?;
        let text = serde_json::to_string_pretty(self).expect("bundle serialises");
        f.write_all(text.as_bytes()).and_then(|_| f.write_all(b"\n")).map_err(|e| BundleError::Io(e.to_string()))
    }
}

/// Short, stable key id: first 16 hex chars of SHA-256(public key).
pub fn key_id(key: &VerifyingKey) -> String {
    HEXLOWER.encode(&Sha256::digest(key.as_bytes()))[..16].to_string()
}

pub fn verifying_key_from_b64(s: &str) -> Result<VerifyingKey, BundleError> {
    let bytes: [u8; 32] = BASE64
        .decode(s.trim().as_bytes())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| BundleError::BadKey("public key must be 32 bytes, base64".into()))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| BundleError::BadKey(e.to_string()))
}

/// Parse a base64 32-byte ed25519 seed (`ZENITH_RELEASE_SIGNING_KEY`).
pub fn signing_key_from_b64(s: &str) -> Result<SigningKey, BundleError> {
    let bytes: [u8; 32] = BASE64
        .decode(s.trim().as_bytes())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| BundleError::BadKey("signing key must be a base64 32-byte seed".into()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Deterministic JSON: object keys sorted, no insignificant whitespace.
/// Independent of serde_json's `preserve_order` feature.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&map[k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Whether a key name suggests a credential.
pub fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["secret", "token", "password", "passwd", "private", "credential", "api_key", "apikey", "seed", "mnemonic"]
        .iter()
        .any(|p| n.contains(p))
        || n.ends_with("_key")
        || n == "key"
}

fn is_stellar_strkey(s: &str, prefix: char) -> bool {
    s.len() == 56 && s.starts_with(prefix) && s.bytes().all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
}

/// Whether a value looks like a credential regardless of its key name.
pub fn is_secret_value(s: &str) -> bool {
    is_stellar_strkey(s, 'S')
        || s.contains("-----BEGIN")
        || s.to_ascii_lowercase().starts_with("bearer ")
        || s.starts_with("ghp_")
        || s.starts_with("AKIA")
}

/// First path (dotted) holding a secret-looking value, if any. Public
/// Stellar keys (`G...`) and contract addresses (`C...`) under a key-like
/// name are allowed: permission flags legitimately name admin keys.
pub fn find_secret(v: &Value, path: &str) -> Option<String> {
    match v {
        Value::Object(map) => map.iter().find_map(|(k, child)| {
            let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
            let public = matches!(child, Value::String(s) if is_stellar_strkey(s, 'G') || is_stellar_strkey(s, 'C'));
            // Boolean/null/empty values under a key-like name are flags, not secrets.
            let carries_value = match child {
                Value::String(s) => !s.is_empty(),
                Value::Number(_) => true,
                _ => false,
            };
            if is_secret_name(k) && carries_value && !public {
                return Some(p);
            }
            find_secret(child, &p)
        }),
        Value::Array(items) => {
            items.iter().enumerate().find_map(|(i, child)| find_secret(child, &format!("{path}[{i}]")))
        }
        Value::String(s) if is_secret_value(s) => Some(path.to_string()),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const IMAGE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    pub(crate) fn spec(env: &str, contract_id: &str) -> EnvironmentSpec {
        let mut contracts = BTreeMap::new();
        contracts.insert(
            "options_pool".to_string(),
            ContractRef { contract_id: contract_id.to_string(), wasm_hash: "b".repeat(64) },
        );
        EnvironmentSpec {
            environment: env.to_string(),
            network_passphrase: format!("{env} passphrase"),
            contracts,
            permissions: [("upgrade_requires_multisig".to_string(), json!(true))].into(),
            feature_flags: [("physical_settlement".to_string(), false)].into(),
            limits: [("max_open_interest".to_string(), json!(1_000_000))].into(),
            config: [("rpc_url".to_string(), json!(format!("https://rpc.{env}.example")))].into(),
            network_specific: vec![
                "network_passphrase".into(),
                "contracts.options_pool.contract_id".into(),
                "config.rpc_url".into(),
            ],
        }
    }

    pub(crate) fn migrations() -> MigrationSet {
        MigrationSet {
            latest: "0002_b".into(),
            files: vec!["0001_a".into(), "0002_b".into()],
            digest: "sha256:00".into(),
            forward_only: vec!["0001_a".into()],
        }
    }

    pub(crate) fn bundle(release: &str) -> ReleaseBundle {
        ReleaseBundle::new(
            release,
            "deadbeef",
            "2026-10-21T00:00:00Z",
            IMAGE,
            migrations(),
            vec![spec("testnet", "CTEST"), spec("mainnet", "CMAIN")],
        )
        .unwrap()
    }

    pub(crate) fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub(crate) fn trusted(k: &SigningKey) -> TrustedKeys {
        [(key_id(&k.verifying_key()), BASE64.encode(k.verifying_key().as_bytes()))].into()
    }

    #[test]
    fn canonical_json_sorts_keys_recursively() {
        let v = json!({"b": 1, "a": {"d": [3, {"z": 1, "y": 2}], "c": "x"}});
        assert_eq!(canonical_json(&v), r#"{"a":{"c":"x","d":[3,{"y":2,"z":1}]},"b":1}"#);
    }

    #[test]
    fn digest_is_stable_and_content_addressed() {
        assert_eq!(bundle("r1").digest(), bundle("r1").digest());
        assert_ne!(bundle("r1").digest(), bundle("r2").digest());
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let k = key(7);
        let signed = bundle("r1").sign(&k);
        signed.verify(&trusted(&k)).unwrap();
        let json = serde_json::to_string(&signed).unwrap();
        let back: SignedBundle = serde_json::from_str(&json).unwrap();
        back.verify(&trusted(&k)).unwrap();
    }

    #[test]
    fn tampering_is_detected() {
        let k = key(7);
        let mut signed = bundle("r1").sign(&k);
        signed.bundle.environments.get_mut("mainnet").unwrap().feature_flags.insert("physical_settlement".into(), true);
        assert!(matches!(signed.verify(&trusted(&k)), Err(BundleError::DigestMismatch { .. })));

        // Re-computing the digest without the key still fails the signature.
        signed.digest = signed.bundle.digest();
        assert_eq!(signed.verify(&trusted(&k)), Err(BundleError::BadSignature));

        let other = bundle("r1").sign(&key(9));
        assert!(matches!(other.verify(&trusted(&k)), Err(BundleError::UnknownSigningKey(_))));
    }

    #[test]
    fn refuses_secrets_and_mutable_tags() {
        let mut s = spec("testnet", "CTEST");
        s.config.insert("webhook_hmac_key".into(), json!("abc123"));
        let err = ReleaseBundle::new("r", "c", "t", IMAGE, migrations(), vec![s]).unwrap_err();
        assert_eq!(err, BundleError::SecretInBundle("environments.testnet.config.webhook_hmac_key".into()));

        let mut s = spec("testnet", "CTEST");
        s.config.insert("operator".into(), json!(format!("S{}", "A".repeat(55))));
        assert!(matches!(
            ReleaseBundle::new("r", "c", "t", IMAGE, migrations(), vec![s]),
            Err(BundleError::SecretInBundle(_))
        ));

        // Public admin keys and boolean flags are fine.
        let mut s = spec("testnet", "CTEST");
        s.permissions.insert("admin_key".into(), json!(format!("G{}", "A".repeat(55))));
        s.feature_flags.insert("rotate_api_key".into(), true);
        ReleaseBundle::new("r", "c", "t", IMAGE, migrations(), vec![s]).unwrap();

        assert!(matches!(
            ReleaseBundle::new("r", "c", "t", "ghcr.io/x:latest", migrations(), vec![]),
            Err(BundleError::InvalidImageDigest(_))
        ));
    }

    #[test]
    fn write_new_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("bundle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bundle.json");
        let signed = bundle("r1").sign(&key(1));
        signed.write_new(&path).unwrap();
        assert_eq!(SignedBundle::load(&path).unwrap(), signed);
        assert!(matches!(signed.write_new(&path), Err(BundleError::AlreadyExists(_))));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_set_pins_files_and_flags_forward_only() {
        let dir = std::env::temp_dir().join(format!("migr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("postgres")).unwrap();
        std::fs::write(dir.join("0001_a.sql"), "CREATE TABLE a (x INT);").unwrap();
        std::fs::write(dir.join("0002_b.sql"), "CREATE TABLE b (x INT);").unwrap();
        std::fs::write(dir.join("0002_b.down.sql"), "DROP TABLE b;").unwrap();
        let m = MigrationSet::from_dir(&dir).unwrap();
        assert_eq!(m.latest, "0002_b");
        assert_eq!(m.files, ["0001_a", "0002_b"]);
        assert_eq!(m.forward_only, ["0001_a"]);
        std::fs::write(dir.join("0001_a.sql"), "CREATE TABLE a (y INT);").unwrap();
        assert_ne!(MigrationSet::from_dir(&dir).unwrap().digest, m.digest);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
