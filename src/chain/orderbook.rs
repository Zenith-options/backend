use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side { Buy, Sell }
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TimeInForce { GoodTillCancel, ImmediateOrCancel, FillOrKill }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitOrder { pub id: String, pub wallet: String, pub series: String, pub side: Side, pub price_ticks: i128, pub remaining: i128, pub sequence: u64, pub time_in_force: TimeInForce, pub self_trade_prevention: bool }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingTrade { pub id: String, pub buy_order_id: String, pub sell_order_id: String, pub quantity: i128, pub price_ticks: i128, pub settlement_status: String }

pub fn match_series(mut buys: Vec<LimitOrder>, mut sells: Vec<LimitOrder>) -> Vec<PendingTrade> {
    buys.sort_by(|a, b| b.price_ticks.cmp(&a.price_ticks).then(a.sequence.cmp(&b.sequence)).then(a.id.cmp(&b.id)));
    sells.sort_by(|a, b| a.price_ticks.cmp(&b.price_ticks).then(a.sequence.cmp(&b.sequence)).then(a.id.cmp(&b.id)));
    let mut trades = Vec::new();
    let (mut buy_index, mut sell_index) = (0, 0);
    while buy_index < buys.len() && sell_index < sells.len() {
        let (buy, sell) = (&mut buys[buy_index], &mut sells[sell_index]);
        if buy.price_ticks < sell.price_ticks { break; }
        if buy.self_trade_prevention && buy.wallet == sell.wallet { sell_index += 1; continue; }
        let quantity = buy.remaining.min(sell.remaining);
        trades.push(PendingTrade { id: format!("{}:{}:{}", buy.id, sell.id, trades.len()), buy_order_id: buy.id.clone(), sell_order_id: sell.id.clone(), quantity, price_ticks: sell.price_ticks, settlement_status: "pending".into() });
        buy.remaining -= quantity; sell.remaining -= quantity;
        if buy.remaining == 0 { buy_index += 1; }
        if sell.remaining == 0 { sell_index += 1; }
    }
    trades
}
