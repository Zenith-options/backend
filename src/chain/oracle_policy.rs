#[derive(Debug, Clone, Copy)]
pub struct OracleSample { pub spot: i128, pub volatility: i128, pub observed_at: i64 }

pub fn should_publish(previous: Option<OracleSample>, current: OracleSample, now: i64, heartbeat_seconds: i64, maximum_staleness_seconds: i64, deviation_bps: i128) -> Result<bool, String> {
    if current.observed_at > now.saturating_add(30) || now.saturating_sub(current.observed_at) > maximum_staleness_seconds { return Err("oracle sample is stale or ahead of the local clock".into()); }
    let Some(previous) = previous else { return Ok(true); };
    if current.observed_at.saturating_sub(previous.observed_at) >= heartbeat_seconds { return Ok(true); }
    let moved = |old: i128, new: i128| old == 0 || new.saturating_sub(old).abs().saturating_mul(10_000) >= old.abs().saturating_mul(deviation_bps);
    Ok(moved(previous.spot, current.spot) || moved(previous.volatility, current.volatility))
}
