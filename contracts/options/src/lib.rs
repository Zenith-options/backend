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
            || expiry <= env.ledger().timestamp()
            || max_payout_per_contract <= 0
        {
            return Err(ContractError::InvalidSeries);
        }

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

    /// Publish the immutable expiry fixing for a series.
    pub fn settle(
        env: Env,
        oracle: Address,
        series_id: u64,
        fixing_price: i128,
    ) -> Result<(), ContractError> {
        let configured_oracle: Address = env
            .storage()
            .instance()
            .get(&DataKey::Oracle)
            .ok_or(ContractError::NotInitialized)?;
        if oracle != configured_oracle {
            return Err(ContractError::Unauthorized);
        }
        oracle.require_auth();
        if fixing_price <= 0 {
            return Err(ContractError::InvalidFixing);
        }
        let mut series = load_series(&env, series_id)?;
        if series.settled {
            return Err(ContractError::AlreadySettled);
        }
        if env.ledger().timestamp() < series.expiry {
            return Err(ContractError::NotExpired);
        }
        series.fixing_price = fixing_price;
        series.settled = true;
        save_series(&env, &series);
        env.events().publish(
            (symbol_short!("settled"), series_id),
            fixing_price,
        );
        Ok(())
    }

    /// Burn holder units and transfer their fixed-point cash payout.
    /// The final claimant also returns unused reserve collateral to the issuer.
    pub fn claim(
        env: Env,
        holder: Address,
        series_id: u64,
        quantity: i128,
    ) -> Result<i128, ContractError> {
        holder.require_auth();
        if quantity <= 0 {
            return Err(ContractError::InvalidQuantity);
        }
        let mut series = load_series(&env, series_id)?;
        if !series.settled {
            return Err(ContractError::NotSettled);
        }
        let balance = load_balance(&env, series_id, &holder);
        if balance < quantity || series.remaining_supply < quantity {
            return Err(ContractError::InsufficientBalance);
        }

        let payout = settlement_payout(
            series.is_call,
            series.strike,
            series.fixing_price,
            series.max_payout_per_contract,
            quantity,
        )?;
        let new_paid_total = series
            .paid_total
            .checked_add(payout)
            .ok_or(ContractError::ArithmeticOverflow)?;
        if new_paid_total > series.collateral_total {
            return Err(ContractError::CollateralInvariant);
        }
        series.remaining_supply -= quantity;
        series.paid_total = new_paid_total;
        save_balance(&env, series_id, &holder, balance - quantity);

        let token = token::Client::new(&env, &usdc_address(&env)?);
        let contract = env.current_contract_address();
        if payout > 0 {
            token.transfer(&contract, &holder, &payout);
        }

        let mut residual = 0;
        if series.remaining_supply == 0 && !series.residual_released {
            residual = series
                .collateral_total
                .checked_sub(series.paid_total)
                .ok_or(ContractError::CollateralInvariant)?;
            if residual > 0 {
                token.transfer(&contract, &series.issuer, &residual);
            }
            series.residual_released = true;
        }
        save_series(&env, &series);
        env.events().publish(
            (symbol_short!("exercise"), series_id),
            (holder, quantity, payout),
        );
        if residual > 0 {
            env.events()
                .publish((symbol_short!("refund"), series_id), residual);
        }
        Ok(payout)
    }

    pub fn series(env: Env, series_id: u64) -> Result<OptionSeries, ContractError> {
        load_series(&env, series_id)
    }

    pub fn balance(env: Env, series_id: u64, holder: Address) -> i128 {
        load_balance(&env, series_id, &holder)
    }
}

fn require_admin(env: &Env, provided: &Address) -> Result<(), ContractError> {
    let admin: Address = env
        .storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(ContractError::NotInitialized)?;
    if &admin != provided {
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
    let key = DataKey::Series(series_id);
    let series = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(ContractError::NotFound)?;
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    bump_instance(env);
    Ok(series)
}

fn save_series(env: &Env, series: &OptionSeries) {
    let key = DataKey::Series(series.id);
    env.storage().persistent().set(&key, series);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    bump_instance(env);
}

fn load_balance(env: &Env, series_id: u64, holder: &Address) -> i128 {
    let key = DataKey::Balance(series_id, holder.clone());
    let balance = env.storage().persistent().get(&key).unwrap_or(0);
    if balance > 0 {
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    }
    balance
}

fn save_balance(env: &Env, series_id: u64, holder: &Address, balance: i128) {
    let key = DataKey::Balance(series_id, holder.clone());
    env.storage().persistent().set(&key, &balance);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    bump_instance(env);
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn ceil_mul_scaled(price: i128, quantity: i128) -> Result<i128, ContractError> {
    if price <= 0 || quantity <= 0 {
        return Err(ContractError::InvalidQuantity);
    }
    let product = price
        .checked_mul(quantity)
        .ok_or(ContractError::ArithmeticOverflow)?;
    product
        .checked_add(SCALE - 1)
        .ok_or(ContractError::ArithmeticOverflow)
        .map(|rounded| rounded / SCALE)
}

fn settlement_payout(
    is_call: bool,
    strike: i128,
    fixing_price: i128,
    cap: i128,
    quantity: i128,
) -> Result<i128, ContractError> {
    if strike <= 0 || fixing_price <= 0 || cap <= 0 || quantity <= 0 {
        return Err(ContractError::InvalidQuantity);
    }
    let raw = if is_call {
        fixing_price.checked_sub(strike)
    } else {
        strike.checked_sub(fixing_price)
    }
    .ok_or(ContractError::ArithmeticOverflow)?
    .max(0)
    .min(cap);
    raw.checked_mul(quantity)
        .ok_or(ContractError::ArithmeticOverflow)
        .map(|amount| amount / SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_call_and_put_payoffs_use_seven_decimal_units() {
        assert_eq!(
            settlement_payout(true, 100 * SCALE, 140 * SCALE, 25 * SCALE, 2 * SCALE).unwrap(),
            50 * SCALE
        );
        assert_eq!(
            settlement_payout(false, 100 * SCALE, 70 * SCALE, 50 * SCALE, SCALE / 2).unwrap(),
            15 * SCALE
        );
        assert_eq!(
            settlement_payout(true, 100 * SCALE, 90 * SCALE, 25 * SCALE, SCALE).unwrap(),
            0
        );
    }

    #[test]
    fn collateral_rounds_up_for_fractional_mints() {
        assert_eq!(ceil_mul_scaled(3, SCALE / 2).unwrap(), 2);
    }

    #[test]
    fn invalid_values_and_overflow_are_rejected() {
        assert_eq!(
            settlement_payout(true, SCALE, SCALE, SCALE, 0),
            Err(ContractError::InvalidQuantity)
        );
        assert_eq!(
            ceil_mul_scaled(i128::MAX, 2),
            Err(ContractError::ArithmeticOverflow)
        );
    }
}
