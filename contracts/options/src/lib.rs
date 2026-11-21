#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, String,
};

const SCALE: i128 = 10_000_000;
const TTL_THRESHOLD: u32 = 100_000;
const TTL_EXTEND_TO: u32 = 500_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotFound = 3,
    InvalidSeries = 4,
    InvalidQuantity = 5,
    InsufficientBalance = 6,
    NotSettled = 7,
    AlreadySettled = 8,
    NotExpired = 9,
    InvalidFixing = 10,
    ArithmeticOverflow = 11,
    CollateralInvariant = 12,
    Unauthorized = 13,
    InvalidRecipient = 14,
    InvalidExpiry = 15,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionSeries {
    pub id: u64,
    pub underlying: String,
    /// Strike price in USDC fixed-point units (7 decimal places).
    pub strike: i128,
    pub expiry: u64,
    pub is_call: bool,
    /// Maximum cash payout per whole option, in USDC fixed-point units.
    pub max_payout_per_contract: i128,
    pub issuer: Address,
    pub total_supply: i128,
    pub remaining_supply: i128,
    pub collateral_total: i128,
    pub paid_total: i128,
    pub fixing_price: i128,
    pub settled: bool,
    pub residual_released: bool,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Oracle,
    Usdc,
    NextSeries,
    Series(u64),
    Balance(u64, Address),
}

#[contract]
pub struct ZenithOptions;

#[contractimpl]
impl ZenithOptions {
    /// Configure the collateral token and the two privileged protocol roles.
    pub fn initialize(env: Env, admin: Address, oracle: Address, usdc: Address) -> Result<(), ContractError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(ContractError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        env.storage().instance().set(&DataKey::Usdc, &usdc);
        env.storage().instance().set(&DataKey::NextSeries, &1_u64);
        bump_instance(&env);
        Ok(())
    }

    /// Rotate the settlement authority. Existing series keep their recorded fixings.
    pub fn set_oracle(env: Env, admin: Address, oracle: Address) -> Result<(), ContractError> {
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        bump_instance(&env);
        Ok(())
    }

    /// Register a capped, cash-settled option series.
    ///
    /// Every issued whole option is fully collateralized to its configured
    /// maximum payout. The cap is explicit because an uncapped short call
    /// cannot be fully collateralized with a finite amount of USDC.
    pub fn create_series(
        env: Env,
        admin: Address,
        underlying: String,
        strike: i128,
        expiry: u64,
        is_call: bool,
        max_payout_per_contract: i128,
    ) -> Result<u64, ContractError> {
        require_admin(&env, &admin)?;
        if underlying.len() == 0
            || strike <= 0
            || max_payout_per_contract <= 0
        {
            return Err(ContractError::InvalidSeries);
        }
        validate_expiry(&env, expiry)?;

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextSeries)
            .ok_or(ContractError::NotInitialized)?;
        let next_id = id.checked_add(1).ok_or(ContractError::ArithmeticOverflow)?;
        let series = OptionSeries {
            id,
            underlying,
            strike,
            expiry,
            is_call,
            max_payout_per_contract,
            issuer: admin,
            total_supply: 0,
            remaining_supply: 0,
            collateral_total: 0,
            paid_total: 0,
            fixing_price: 0,
            settled: false,
            residual_released: false,
        };

        env.storage().instance().set(&DataKey::NextSeries, &next_id);
        save_series(&env, &series);
        env.events().publish(
            (symbol_short!("created"), id),
            (
                series.underlying.clone(),
                series.strike,
                series.expiry,
                series.is_call,
                series.max_payout_per_contract,
            ),
        );
        Ok(id)
    }

    /// Deposit capped-payout collateral and mint option units to `holder`.
    /// Quantities are fixed-point values with seven decimal places.
    pub fn mint(
        env: Env,
        admin: Address,
        series_id: u64,
        holder: Address,
        quantity: i128,
    ) -> Result<i128, ContractError> {
        require_admin(&env, &admin)?;
        if quantity <= 0 {
            return Err(ContractError::InvalidQuantity);
        }
        let mut series = load_series(&env, series_id)?;
        if series.settled || env.ledger().timestamp() >= series.expiry {
            return Err(ContractError::InvalidSeries);
        }

        let collateral = ceil_mul_scaled(series.max_payout_per_contract, quantity)?;
        let new_total_collateral = series
            .collateral_total
            .checked_add(collateral)
            .ok_or(ContractError::ArithmeticOverflow)?;
        let new_supply = series
            .total_supply
            .checked_add(quantity)
            .ok_or(ContractError::ArithmeticOverflow)?;
        let new_remaining = series
            .remaining_supply
            .checked_add(quantity)
            .ok_or(ContractError::ArithmeticOverflow)?;
        let balance = load_balance(&env, series_id, &holder);
        let new_balance = balance
            .checked_add(quantity)
            .ok_or(ContractError::ArithmeticOverflow)?;

        token::Client::new(&env, &usdc_address(&env)?).transfer(
            &admin,
            &env.current_contract_address(),
            &collateral,
        );

        series.total_supply = new_supply;
        series.remaining_supply = new_remaining;
        series.collateral_total = new_total_collateral;
        save_series(&env, &series);
        save_balance(&env, series_id, &holder, new_balance);
        env.events()
            .publish(
                (symbol_short!("mint"), series_id),
                (
                    series.strike,
                    series.expiry,
                    series.is_call,
                    quantity,
                    admin.clone(),
                    holder.clone(),
                ),
            );
        env.events().publish(
            (symbol_short!("deposit"), series_id),
            (admin, collateral),
        );
        Ok(collateral)
    }

    /// Transfer option units before settlement.
    pub fn transfer(
        env: Env,
        from: Address,
        to: Address,
        series_id: u64,
        quantity: i128,
    ) -> Result<(), ContractError> {
        from.require_auth();
        if quantity <= 0 {
            return Err(ContractError::InvalidQuantity);
        }
        let series = load_series(&env, series_id)?;
        if series.settled || env.ledger().timestamp() >= series.expiry {
            return Err(ContractError::InvalidSeries);
        }
        if to == env.current_contract_address() {
            return Err(ContractError::InvalidRecipient);
        }
        let from_balance = load_balance(&env, series_id, &from);
        if from_balance < quantity {
            return Err(ContractError::InsufficientBalance);
        }
        if from == to {
            return Ok(());
        }
        let to_balance = load_balance(&env, series_id, &to);
        let new_to_balance = to_balance
            .checked_add(quantity)
            .ok_or(ContractError::ArithmeticOverflow)?;
        save_balance(&env, series_id, &from, from_balance - quantity);
        save_balance(&env, series_id, &to, new_to_balance);
        env.events()
            .publish((symbol_short!("transfer"), series_id), (from, to, quantity));
        Ok(())
    }
}

/// Reject an expiry that is at or before the current ledger time.
fn validate_expiry(env: &Env, expiry: u64) -> Result<(), ContractError> {
    if expiry <= env.ledger().timestamp() {
        return Err(ContractError::InvalidExpiry);
    }
    Ok(())
}

fn require_admin(env: &Env, admin: &Address) -> Result<(), ContractError> {
    let stored: Address = env
        .storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(ContractError::NotInitialized)?;
    if &stored != admin {
        return Err(ContractError::Unauthorized);
    }
    admin.require_auth();
    Ok(())
}

fn usdc_address(env: &Env) -> Result<Address, ContractError> {
    env.storage()
        .instance()
        .get(&DataKey::Usdc)
        .ok_or(ContractError::NotInitialized)
}

fn load_series(env: &Env, series_id: u64) -> Result<OptionSeries, ContractError> {
    env.storage()
        .instance()
        .get(&DataKey::Series(series_id))
        .ok_or(ContractError::NotFound)
}

fn save_series(env: &Env, series: &OptionSeries) {
    env.storage()
        .instance()
        .set(&DataKey::Series(series.id), series);
    bump_instance(env);
}

fn load_balance(env: &Env, series_id: u64, holder: &Address) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::Balance(series_id, holder.clone()))
        .unwrap_or(0)
}

fn save_balance(env: &Env, series_id: u64, holder: &Address, balance: i128) {
    env.storage()
        .instance()
        .set(&DataKey::Balance(series_id, holder.clone()), &balance);
    bump_instance(env);
}

fn ceil_mul_scaled(a: i128, b: i128) -> Result<i128, ContractError> {
    let product = a.checked_mul(b).ok_or(ContractError::ArithmeticOverflow)?;
    let scaled = product
        .checked_add(SCALE - 1)
        .ok_or(ContractError::ArithmeticOverflow)?;
    Ok(scaled / SCALE)
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Ledger;

    #[test]
    fn rejects_expiry_one_second_in_the_past() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        assert_eq!(validate_expiry(&env, 999), Err(ContractError::InvalidExpiry));
    }

    #[test]
    fn rejects_expiry_at_exactly_now() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        assert_eq!(validate_expiry(&env, 1_000), Err(ContractError::InvalidExpiry));
    }

    #[test]
    fn accepts_future_expiry() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        assert_eq!(validate_expiry(&env, 1_001), Ok(()));
    }
}
