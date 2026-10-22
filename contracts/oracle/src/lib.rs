#![no_std]

extern crate alloc;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Bytes, BytesN,
    Env, String,
};

const MAX_SYMBOL_LEN: usize = 16;
const PUBLICATION_GRACE_SECS: u64 = 5 * 60;
const TTL_THRESHOLD_LEDGERS: u32 = 17_280;
const TTL_EXTEND_TO_LEDGERS: u32 = 518_400;

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OracleError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidSymbol = 3,
    InvalidPrice = 4,
    InvalidExpiry = 5,
    FixingAlreadyPublished = 6,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fixing {
    pub price: i128,
    pub expiry: u64,
    pub published_at: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Publisher,
    Fixing(String, u64),
}

#[contract]
pub struct OracleContract;

#[contractimpl]
impl OracleContract {
    /// Initialize the contract with an admin and an Ed25519 publisher key.
    pub fn initialize(env: Env, admin: Address, publisher: BytesN<32>) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, OracleError::AlreadyInitialized);
        }

        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Publisher, &publisher);
        extend_instance_ttl(&env);
    }

    /// Store an immutable, signed expiry fixing during the five-minute
    /// publication window. The signature format matches the backend publisher.
    pub fn publish_fixing(
        env: Env,
        underlying: String,
        expiry: u64,
        price: i128,
        signature: BytesN<64>,
    ) {
        extend_instance_ttl(&env);
        validate_symbol(&env, &underlying);
        if price <= 0 {
            panic_with_error!(&env, OracleError::InvalidPrice);
        }

        let now = env.ledger().timestamp();
        if expiry > now || now.saturating_sub(expiry) > PUBLICATION_GRACE_SECS {
            panic_with_error!(&env, OracleError::InvalidExpiry);
        }

        let key = DataKey::Fixing(underlying.clone(), expiry);
        if env.storage().persistent().has(&key) {
            panic_with_error!(&env, OracleError::FixingAlreadyPublished);
        }

        let publisher = get_publisher(&env);
        let message = canonical_message(&env, &underlying, expiry, price);
        env.crypto()
            .ed25519_verify(&publisher, &message, &signature);

        env.storage().persistent().set(
            &key,
            &Fixing {
                price,
                expiry,
                published_at: now,
            },
        );
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
        env.events().publish((underlying, expiry), price);
    }

    /// Read a published fixing by underlying and expiry.
    pub fn get_fixing(env: Env, underlying: String, expiry: u64) -> Option<Fixing> {
        extend_instance_ttl(&env);
        let key = DataKey::Fixing(underlying, expiry);
        let fixing = env.storage().persistent().get(&key);
        if fixing.is_some() {
            env.storage().persistent().extend_ttl(
                &key,
                TTL_THRESHOLD_LEDGERS,
                TTL_EXTEND_TO_LEDGERS,
            );
        }
        fixing
    }

    /// Rotate the key authorized to publish fixings.
    pub fn set_publisher(env: Env, publisher: BytesN<32>) {
        let admin = get_admin(&env);
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::Publisher, &publisher);
        extend_instance_ttl(&env);
    }

    /// Transfer administration to a new address.
    pub fn set_admin(env: Env, new_admin: Address) {
        let admin = get_admin(&env);
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        extend_instance_ttl(&env);
    }
}

fn get_admin(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .unwrap_or_else(|| panic_with_error!(env, OracleError::NotInitialized))
}

fn get_publisher(env: &Env) -> BytesN<32> {
    env.storage()
        .instance()
        .get(&DataKey::Publisher)
        .unwrap_or_else(|| panic_with_error!(env, OracleError::NotInitialized))
}

fn validate_symbol(env: &Env, symbol: &String) {
    let len = symbol.len() as usize;
    if len == 0 || len > MAX_SYMBOL_LEN {
        panic_with_error!(env, OracleError::InvalidSymbol);
    }

    let mut bytes = [0u8; MAX_SYMBOL_LEN];
    symbol.copy_into_slice(&mut bytes[..len]);
    if bytes[..len].iter().any(|byte| {
        !byte.is_ascii_uppercase()
            && !byte.is_ascii_digit()
            && *byte != b'/'
            && *byte != b'-'
            && *byte != b'_'
    }) {
        panic_with_error!(env, OracleError::InvalidSymbol);
    }
}

fn canonical_message(env: &Env, underlying: &String, expiry: u64, price: i128) -> Bytes {
    let mut symbol = [0u8; MAX_SYMBOL_LEN];
    let len = underlying.len() as usize;
    underlying.copy_into_slice(&mut symbol[..len]);

    let expiry = alloc::format!("{expiry}");
    let price = alloc::format!("{price}");
    let mut message = Bytes::from_slice(env, b"zenith:fixing:v1|");
    message.append(&Bytes::from_slice(env, &symbol[..len]));
    message.push_back(b'|');
    message.append(&Bytes::from_slice(env, expiry.as_bytes()));
    message.push_back(b'|');
    message.append(&Bytes::from_slice(env, price.as_bytes()));
    message
}

fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD_LEDGERS, TTL_EXTEND_TO_LEDGERS);
}

#[cfg(test)]
mod test {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    const LEDGER_TIME: u64 = 1_000;

    fn setup() -> (Env, Address, Address, SigningKey) {
        let env = Env::default();
        env.ledger().set_timestamp(LEDGER_TIME);
        env.mock_all_auths();

        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let publisher = BytesN::from_array(&env, &signing_key.verifying_key().to_bytes());
        let admin = Address::generate(&env);
        let contract_id = env.register(OracleContract, ());
        let client = OracleContractClient::new(&env, &contract_id);
        client.initialize(&admin, &publisher);
        (env, contract_id, admin, signing_key)
    }

    fn signed_fixing(
        env: &Env,
        signing_key: &SigningKey,
        underlying: &str,
        expiry: u64,
        price: i128,
    ) -> (String, BytesN<64>) {
        let symbol = String::from_str(env, underlying);
        let payload = alloc::format!("zenith:fixing:v1|{underlying}|{expiry}|{price}");
        let signature = signing_key.sign(payload.as_bytes()).to_bytes();
        (symbol, BytesN::from_array(env, &signature))
    }

    #[test]
    fn publishes_and_reads_a_valid_signed_fixing_once() {
        let (env, contract_id, _, signing_key) = setup();
        let client = OracleContractClient::new(&env, &contract_id);
        let (symbol, signature) = signed_fixing(&env, &signing_key, "BTC", 900, 42_000_000);

        client.publish_fixing(&symbol, &900, &42_000_000, &signature);

        assert_eq!(
            client.get_fixing(&symbol, &900),
            Some(Fixing {
                price: 42_000_000,
                expiry: 900,
                published_at: LEDGER_TIME,
            })
        );
        assert!(client
            .try_publish_fixing(&symbol, &900, &42_000_000, &signature)
            .is_err());
    }

    #[test]
    fn rejects_invalid_prices_and_publication_outside_the_window() {
        let (env, contract_id, _, signing_key) = setup();
        let client = OracleContractClient::new(&env, &contract_id);
        let (symbol, signature) = signed_fixing(&env, &signing_key, "BTC", 900, 0);
        assert!(client
            .try_publish_fixing(&symbol, &900, &0, &signature)
            .is_err());

        let (future_symbol, future_signature) = signed_fixing(&env, &signing_key, "ETH", 1_001, 10);
        assert!(client
            .try_publish_fixing(&future_symbol, &1_001, &10, &future_signature)
            .is_err());

        let (stale_symbol, stale_signature) = signed_fixing(&env, &signing_key, "SOL", 699, 10);
        assert!(client
            .try_publish_fixing(&stale_symbol, &699, &10, &stale_signature)
            .is_err());
    }

    #[test]
    fn rejects_a_signature_from_an_unconfigured_publisher() {
        let (env, contract_id, _, _) = setup();
        let client = OracleContractClient::new(&env, &contract_id);
        let attacker_key = SigningKey::from_bytes(&[8; 32]);
        let (symbol, signature) = signed_fixing(&env, &attacker_key, "BTC", 900, 42_000_000);

        assert!(client
            .try_publish_fixing(&symbol, &900, &42_000_000, &signature)
            .is_err());
    }

    #[test]
    fn only_the_admin_can_rotate_the_publisher_key() {
        let (env, contract_id, _, _) = setup();
        let client = OracleContractClient::new(&env, &contract_id);
        env.mock_auths(&[]);
        let replacement = BytesN::from_array(&env, &[9; 32]);

        assert!(client.try_set_publisher(&replacement).is_err());
    }
}
