use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::error::{db_error, AppError, AppJson};
use crate::models::{Alert, Position};
use crate::AppState;

pub async fn get_alerts(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
) -> Result<Json<Vec<Alert>>, AppError> {
    let alerts: Vec<Alert> =
        sqlx::query_as("SELECT * FROM alerts WHERE wallet_address = ? ORDER BY created_at DESC")
            .bind(&wallet_address)
            .fetch_all(&state.db)
            .await
            .map_err(|e| db_error("load alerts", e))?;

    Ok(Json(alerts))
}

#[derive(Deserialize)]
pub struct CreateAlertRequest {
    pub underlying: Option<String>,
    pub condition: String,
    pub target_price: Option<f64>,
    pub window_seconds: Option<i64>,
    pub strike: Option<f64>,
    pub expiry_days: Option<f64>,
    pub option_type: Option<String>,
    pub position_id: Option<String>,
    pub strategy_id: Option<String>,
    pub trigger_policy: Option<String>,
    pub cooldown_seconds: Option<i64>,
    pub lower_bound: Option<f64>,
    pub upper_bound: Option<f64>,
}

pub async fn create_alert(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    AppJson(req): AppJson<CreateAlertRequest>,
) -> Result<Json<Alert>, AppError> {
    let mut underlying = req.underlying.clone();
    const CONDITIONS: &[&str] = &[
        "above",
        "below",
        "percent_change_above",
        "percent_change_below",
        "iv_above",
        "iv_below",
        "position_pnl_above",
        "position_pnl_below",
        "strategy_pnl_above",
        "strategy_pnl_below",
        "portfolio_delta_above",
        "portfolio_delta_below",
        "portfolio_delta_outside",
        "expiry_within",
    ];
    if !CONDITIONS.contains(&req.condition.as_str()) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "condition must be above, below, percent_change_above/below, iv_above/below, position_pnl_above/below, strategy_pnl_above/below, portfolio_delta_above/below/outside, or expiry_within",
        ));
    }
    let target_price = match req.target_price {
        Some(value) if value.is_finite() => value,
        None if req.condition == "portfolio_delta_outside" => 0.0,
        _ => {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "target_price must be finite",
            ));
        }
    };
    if (matches!(req.condition.as_str(), "above" | "below" | "expiry_within")
        && target_price <= 0.0)
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "spot price and expiry targets must be positive",
        ));
    }
    let policy = req.trigger_policy.as_deref().unwrap_or("once");
    if !["once", "recurring", "auto_rearm"].contains(&policy) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "trigger_policy must be once, recurring, or auto_rearm",
        ));
    }
    let cooldown = req.cooldown_seconds.unwrap_or(0);
    if cooldown < 0 || (policy != "once" && cooldown == 0) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "recurring and auto_rearm alerts require a positive cooldown_seconds",
        ));
    }
    if req.condition.starts_with("percent_change_")
        && !req
            .window_seconds
            .is_some_and(|seconds| (1..=31_536_000).contains(&seconds))
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "percent-change alerts require window_seconds between 1 and 31536000",
        ));
    }
    if req.condition.starts_with("iv_")
        && (!req
            .strike
            .is_some_and(|strike| strike.is_finite() && strike > 0.0)
            || !req
                .expiry_days
                .is_some_and(|days| days.is_finite() && days > 0.0)
            || !req
                .option_type
                .as_deref()
                .is_some_and(|t| matches!(t, "call" | "put")))
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "IV alerts require a positive strike and expiry_days and option_type call or put",
        ));
    }
    if req.condition.starts_with("position_pnl_") && req.position_id.is_none() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "position P&L alerts require position_id",
        ));
    }
    if req.condition.starts_with("strategy_pnl_") && req.strategy_id.is_none() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "strategy P&L alerts require strategy_id",
        ));
    }
    if req.condition == "portfolio_delta_outside"
        && (!req.lower_bound.is_some_and(f64::is_finite)
            || !req.upper_bound.is_some_and(f64::is_finite)
            || req
                .lower_bound
                .zip(req.upper_bound)
                .is_none_or(|(low, high)| low >= high))
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "portfolio_delta_outside requires finite lower_bound and upper_bound with lower_bound < upper_bound",
        ));
    }
    if req.condition.starts_with("position_pnl_") {
        let position_underlying: Option<String> = sqlx::query_scalar(
            "SELECT underlying FROM positions WHERE id = ? AND wallet_address = ?",
        )
        .bind(req.position_id.as_deref())
        .bind(&wallet_address)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| db_error("validate alert position", e))?;
        let position_underlying = position_underlying.ok_or_else(|| {
            AppError::new(StatusCode::NOT_FOUND, "position not found for this wallet")
        })?;
        underlying.get_or_insert(position_underlying);
    }
    if req.condition.starts_with("strategy_pnl_") {
        let strategy_underlying: Option<String> = sqlx::query_scalar(
            "SELECT underlying FROM positions
             WHERE strategy_id = ? AND wallet_address = ?
             ORDER BY opened_at LIMIT 1",
        )
        .bind(req.strategy_id.as_deref())
        .bind(&wallet_address)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| db_error("validate alert strategy", e))?;
        let strategy_underlying = strategy_underlying.ok_or_else(|| {
            AppError::new(StatusCode::NOT_FOUND, "strategy not found for this wallet")
        })?;
        underlying.get_or_insert(strategy_underlying);
    }
    if matches!(
        req.condition.as_str(),
        "above"
            | "below"
            | "percent_change_above"
            | "percent_change_below"
            | "iv_above"
            | "iv_below"
            | "expiry_within"
    ) {
        let symbol = underlying.as_deref().ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "this alert condition requires underlying",
            )
        })?;
        if !state.spot_prices.lock().unwrap().contains_key(symbol) {
            return Err(AppError::new(
                StatusCode::NOT_FOUND,
                format!("unknown underlying \"{symbol}\""),
            ));
        }
    } else if req.condition.starts_with("portfolio_delta_") {
        underlying.get_or_insert_with(|| "PORTFOLIO".to_owned());
    }

    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO alerts
            (id, wallet_address, underlying, condition, target_price, window_seconds,
             strike, expiry_days, option_type, position_id, strategy_id, trigger_policy,
             cooldown_seconds, lower_bound, upper_bound)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&wallet_address)
    .bind(underlying.as_deref())
    .bind(&req.condition)
    .bind(target_price)
    .bind(req.window_seconds)
    .bind(req.strike)
    .bind(req.expiry_days)
    .bind(req.option_type.as_deref())
    .bind(req.position_id.as_deref())
    .bind(req.strategy_id.as_deref())
    .bind(policy)
    .bind(cooldown)
    .bind(req.lower_bound)
    .bind(req.upper_bound)
    .execute(&state.db)
    .await
    .map_err(|e| db_error("create alert", e))?;

    let alert: Alert = sqlx::query_as("SELECT * FROM alerts WHERE id = ?")
        .bind(&id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| db_error("load the alert just created", e))?;

    Ok(Json(alert))
}

pub async fn delete_alert(
    State(state): State<AppState>,
    AuthUser(wallet_address): AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let result = sqlx::query("DELETE FROM alerts WHERE id = ? AND wallet_address = ?")
        .bind(&id)
        .bind(&wallet_address)
        .execute(&state.db)
        .await
        .map_err(|e| db_error("delete alert", e))?;

    if result.rows_affected() == 0 {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "no alert with that id for this wallet",
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Evaluates each alert against the current market/portfolio state,
/// persists its trigger state, and queues notifications for newly-fired
/// alerts. Returns the number of triggers recorded in this pass.
pub async fn check_once(state: &AppState) -> u64 {
    let prices = state.spot_prices.lock().unwrap().clone();
    for (underlying, spot) in &prices {
        if let Err(e) =
            sqlx::query("INSERT OR REPLACE INTO spot_history (underlying, price) VALUES (?, ?)")
                .bind(underlying)
                .bind(spot)
                .execute(&state.db)
                .await
        {
            tracing::warn!(error = %e, "record spot history for alerts failed");
        }
    }
    if let Err(e) = sqlx::query(
        "DELETE FROM spot_history WHERE recorded_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-366 days')",
    )
    .execute(&state.db)
    .await
    {
        tracing::warn!(error = %e, "prune alert spot history failed");
    }

    let alerts: Vec<AlertEvaluation> = match sqlx::query_as(
        "SELECT id, wallet_address, underlying, condition, target_price, triggered,
                window_seconds, strike, expiry_days, option_type, position_id,
                strategy_id, trigger_policy, cooldown_seconds, last_triggered_at,
                lower_bound, upper_bound
         FROM alerts",
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(alerts) => alerts,
        Err(e) => {
            tracing::warn!(error = %e, "load alerts for evaluation failed");
            return 0;
        }
    };

    let mut total_fired = 0;
    for alert in alerts {
        let spot = match prices.get(&alert.underlying) {
            Some(spot) => *spot,
            None if alert.condition.starts_with("position_pnl_")
                || alert.condition.starts_with("strategy_pnl_")
                || alert.condition.starts_with("portfolio_delta_")
                || alert.condition == "expiry_within" =>
            {
                0.0
            }
            None => continue,
        };
        let condition_met = match evaluate_condition(state, &alert, spot).await {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!(error = %e, alert_id = %alert.id, "evaluate alert failed");
                continue;
            }
        };
        let should_trigger = match alert.trigger_policy.as_str() {
            "once" => !alert.triggered && condition_met,
            "recurring" => {
                condition_met
                    && cooldown_elapsed(alert.last_triggered_at.as_deref(), alert.cooldown_seconds)
            }
            "auto_rearm" => !alert.triggered && condition_met,
            _ => false,
        };

        if alert.trigger_policy == "auto_rearm" && alert.triggered && !condition_met {
            let result = sqlx::query(
                "UPDATE alerts SET triggered = 0
                 WHERE id = ? AND triggered = 1 AND
                   (last_triggered_at IS NULL OR
                    julianday('now') - julianday(last_triggered_at) >= cooldown_seconds / 86400.0)",
            )
            .bind(&alert.id)
            .execute(&state.db)
            .await;
            if let Err(e) = result {
                tracing::warn!(error = %e, alert_id = %alert.id, "re-arm alert failed");
            }
            continue;
        }

        if should_trigger {
            match sqlx::query(
                "UPDATE alerts
                    SET triggered = 1,
                        triggered_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                        last_triggered_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?",
            )
            .bind(&alert.id)
            .execute(&state.db)
            .await
            {
                Ok(r) if r.rows_affected() > 0 => {
                    tracing::info!(alert_id = %alert.id, underlying = %alert.underlying, "alert triggered");
                    total_fired += r.rows_affected();
                    let event = serde_json::json!({
                        "alert_id": alert.id,
                        "underlying": alert.underlying,
                        "condition": alert.condition,
                        "target": alert.target_price
                    });
                    if let Err(error) = crate::delivery::emit_event(
                        state,
                        &alert.wallet_address,
                        "alert_triggered",
                        event,
                    )
                    .await
                    {
                        tracing::warn!(error = %error.message, alert_id = %alert.id, "queue alert notification failed");
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, alert_id = %alert.id, "trigger alert failed"),
            }
        }
    }
    total_fired
}

#[derive(sqlx::FromRow)]
struct AlertEvaluation {
    id: String,
    wallet_address: String,
    underlying: String,
    condition: String,
    target_price: f64,
    triggered: bool,
    window_seconds: Option<i64>,
    strike: Option<f64>,
    expiry_days: Option<f64>,
    option_type: Option<String>,
    position_id: Option<String>,
    strategy_id: Option<String>,
    trigger_policy: String,
    cooldown_seconds: i64,
    last_triggered_at: Option<String>,
    lower_bound: Option<f64>,
    upper_bound: Option<f64>,
}

fn cooldown_elapsed(last: Option<&str>, cooldown_seconds: i64) -> bool {
    let Some(last) = last else { return true };
    // SQLite timestamps are UTC and lexically ordered, so a portable
    // wall-clock comparison can use the standard library epoch clock.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let parsed = last.get(0..19).and_then(time_to_unix);
    parsed.is_some_and(|then| now - then >= cooldown_seconds)
}

fn time_to_unix(s: &str) -> Option<i64> {
    let date = s.get(0..10)?;
    let time = s.get(11..19)?;
    let mut d = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (t.next()??, t.next()??, t.next()??);
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(((era * 146097 + doe - 719468) * 86400) + hour * 3600 + minute * 60 + second)
}

async fn evaluate_condition(
    state: &AppState,
    alert: &AlertEvaluation,
    spot: f64,
) -> Result<bool, sqlx::Error> {
    let value = match alert.condition.as_str() {
        "above" | "below" => spot,
        "percent_change_above" | "percent_change_below" => {
            let Some(window) = alert.window_seconds else {
                return Ok(false);
            };
            let modifier = format!("-{window} seconds");
            let baseline: Option<f64> = sqlx::query_scalar(
                "SELECT price FROM spot_history
                 WHERE underlying = ? AND recorded_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?)
                 ORDER BY recorded_at DESC LIMIT 1",
            )
            .bind(&alert.underlying)
            .bind(modifier)
            .fetch_optional(&state.db)
            .await?;
            let Some(base) = baseline.filter(|base| *base > 0.0) else {
                return Ok(false);
            };
            (spot / base - 1.0) * 100.0
        }
        "iv_above" | "iv_below" => {
            let (Some(strike), Some(_days), Some(_option_type)) = (
                alert.strike,
                alert.expiry_days,
                alert.option_type.as_deref(),
            ) else {
                return Ok(false);
            };
            let base_vol = state
                .vol_surface
                .lock()
                .unwrap()
                .get(&alert.underlying)
                .copied();
            let Some(base_vol) = base_vol else {
                return Ok(false);
            };
            crate::smile_vol(base_vol, strike / spot)
        }
        "position_pnl_above" | "position_pnl_below" => {
            let Some(position_id) = alert.position_id.as_deref() else {
                return Ok(false);
            };
            let position: Option<Position> =
                sqlx::query_as("SELECT * FROM positions WHERE id = ? AND wallet_address = ?")
                    .bind(position_id)
                    .bind(&alert.wallet_address)
                    .fetch_optional(&state.db)
                    .await?;
            let Some(position) = position else {
                return Ok(false);
            };
            position_pnl(state, &position)
        }
        "strategy_pnl_above" | "strategy_pnl_below" => {
            let Some(strategy_id) = alert.strategy_id.as_deref() else {
                return Ok(false);
            };
            let positions: Vec<Position> = sqlx::query_as(
                "SELECT * FROM positions WHERE strategy_id = ? AND wallet_address = ?",
            )
            .bind(strategy_id)
            .bind(&alert.wallet_address)
            .fetch_all(&state.db)
            .await?;
            positions.iter().map(|p| position_pnl(state, p)).sum()
        }
        "portfolio_delta_above" | "portfolio_delta_below" => {
            let positions: Vec<Position> = sqlx::query_as(
                "SELECT * FROM positions WHERE wallet_address = ? AND status = 'open'",
            )
            .bind(&alert.wallet_address)
            .fetch_all(&state.db)
            .await?;
            positions
                .iter()
                .map(|p| {
                    let Some(bs) = crate::positions::current_bs_result(state, p) else {
                        return 0.0;
                    };
                    let sign = if p.position_type == "short" {
                        -1.0
                    } else {
                        1.0
                    };
                    sign * bs.delta * p.contracts
                })
                .sum()
        }
        "portfolio_delta_outside" => {
            let positions: Vec<Position> = sqlx::query_as(
                "SELECT * FROM positions WHERE wallet_address = ? AND status = 'open'",
            )
            .bind(&alert.wallet_address)
            .fetch_all(&state.db)
            .await?;
            let delta: f64 = positions
                .iter()
                .map(|p| {
                    let Some(bs) = crate::positions::current_bs_result(state, p) else {
                        return 0.0;
                    };
                    let sign = if p.position_type == "short" {
                        -1.0
                    } else {
                        1.0
                    };
                    sign * bs.delta * p.contracts
                })
                .sum();
            return Ok(alert.lower_bound.is_some_and(|low| delta < low)
                || alert.upper_bound.is_some_and(|high| delta > high));
        }
        "expiry_within" => {
            let expiries: Vec<(f64, String)> = sqlx::query_as(
                "SELECT expiry_days, opened_at FROM positions
                 WHERE wallet_address = ? AND underlying = ? AND status = 'open'",
            )
            .bind(&alert.wallet_address)
            .bind(&alert.underlying)
            .fetch_all(&state.db)
            .await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let remaining = expiries
                .iter()
                .filter_map(|(expiry_days, opened_at)| {
                    let opened = time_to_unix(opened_at.get(0..19)?)?;
                    Some(expiry_days - (now - opened).max(0) as f64 / 86_400.0)
                })
                .fold(None, |min: Option<f64>, days| {
                    Some(min.map_or(days, |value| value.min(days)))
                });
            let Some(days) = remaining else {
                return Ok(false);
            };
            return Ok(days <= alert.target_price);
        }
        _ => return Ok(false),
    };
    Ok(match alert.condition.as_str() {
        "above"
        | "percent_change_above"
        | "iv_above"
        | "position_pnl_above"
        | "strategy_pnl_above"
        | "portfolio_delta_above" => value >= alert.target_price,
        "below"
        | "percent_change_below"
        | "iv_below"
        | "position_pnl_below"
        | "strategy_pnl_below"
        | "portfolio_delta_below" => value <= alert.target_price,
        _ => false,
    })
}

fn position_pnl(state: &AppState, position: &Position) -> f64 {
    if position.status == "open" {
        let Some(bs) = crate::positions::current_bs_result(state, position) else {
            return 0.0;
        };
        let difference = if position.position_type == "short" {
            position.entry_premium - bs.premium
        } else {
            bs.premium - position.entry_premium
        };
        difference * position.contracts
    } else {
        position.realized_pnl.unwrap_or(0.0)
    }
}

/// Alerts stay in the table (and visible via GET) after triggering — they
/// just stop being re-checked — rather than being deleted, so the frontend
/// can show "this alert fired" instead of it silently vanishing.
pub async fn check_alerts_loop(state: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        check_once(&state).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_state() -> (AppState, std::path::PathBuf) {
        let db_path =
            std::env::temp_dir().join(format!("zenith-alerts-test-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(&format!("sqlite://{}", db_path.display())).await;
        (AppState::new(pool), db_path)
    }

    async fn insert_alert(
        state: &AppState,
        id: &str,
        underlying: &str,
        condition: &str,
        target: f64,
    ) {
        sqlx::query(
            "INSERT INTO accounts (wallet_address) VALUES ('GTEST') ON CONFLICT DO NOTHING",
        )
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO alerts (id, wallet_address, underlying, condition, target_price)
             VALUES (?, 'GTEST', ?, ?, ?)",
        )
        .bind(id)
        .bind(underlying)
        .bind(condition)
        .bind(target)
        .execute(&state.db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn check_once_fires_an_alert_whose_condition_is_met() {
        let (state, db_path) = test_state().await;
        state
            .spot_prices
            .lock()
            .unwrap()
            .insert("BTC".into(), 70_000.0);
        insert_alert(&state, "a1", "BTC", "above", 65_000.0).await;

        let fired = check_once(&state).await;
        assert_eq!(fired, 1);

        let triggered: bool = sqlx::query_scalar("SELECT triggered FROM alerts WHERE id = 'a1'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert!(triggered);

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn check_once_leaves_an_unmet_alert_untouched() {
        let (state, db_path) = test_state().await;
        state
            .spot_prices
            .lock()
            .unwrap()
            .insert("BTC".into(), 50_000.0);
        insert_alert(&state, "a1", "BTC", "above", 65_000.0).await;

        let fired = check_once(&state).await;
        assert_eq!(fired, 0);

        let triggered: bool = sqlx::query_scalar("SELECT triggered FROM alerts WHERE id = 'a1'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert!(!triggered);

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn check_once_does_not_re_fire_an_already_triggered_alert() {
        let (state, db_path) = test_state().await;
        state
            .spot_prices
            .lock()
            .unwrap()
            .insert("BTC".into(), 70_000.0);
        insert_alert(&state, "a1", "BTC", "above", 65_000.0).await;

        assert_eq!(check_once(&state).await, 1);
        // Price stays well past the trigger; a second pass must not count
        // it again now that it's already triggered.
        assert_eq!(check_once(&state).await, 0);

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn percent_change_alert_uses_the_requested_historical_window() {
        let (state, db_path) = test_state().await;
        state
            .spot_prices
            .lock()
            .unwrap()
            .insert("BTC".into(), 110.0);
        insert_alert(&state, "a1", "BTC", "percent_change_above", 5.0).await;
        sqlx::query("UPDATE alerts SET window_seconds = 60 WHERE id = 'a1'")
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO spot_history (underlying, price, recorded_at)
             VALUES ('BTC', 100.0, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-120 seconds'))",
        )
        .execute(&state.db)
        .await
        .unwrap();

        assert_eq!(check_once(&state).await, 1);
        let triggered: bool = sqlx::query_scalar("SELECT triggered FROM alerts WHERE id = 'a1'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert!(triggered);

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn recurring_alert_waits_for_its_cooldown_before_firing_again() {
        let (state, db_path) = test_state().await;
        state
            .spot_prices
            .lock()
            .unwrap()
            .insert("BTC".into(), 70_000.0);
        insert_alert(&state, "a1", "BTC", "above", 65_000.0).await;
        sqlx::query(
            "UPDATE alerts
             SET triggered = 1, trigger_policy = 'recurring', cooldown_seconds = 30,
                 last_triggered_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-60 seconds')
             WHERE id = 'a1'",
        )
        .execute(&state.db)
        .await
        .unwrap();

        assert_eq!(check_once(&state).await, 1);
        let recent = check_once(&state).await;
        assert_eq!(recent, 0);

        state.db.close().await;
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn parses_utc_timestamps_for_expiry_and_cooldown_checks() {
        assert_eq!(time_to_unix("1970-01-01T00:00:00"), Some(0));
        assert_eq!(time_to_unix("2024-02-29T12:30:15"), Some(1_709_209_815));
    }
}
