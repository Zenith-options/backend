-- Legs opened together as one multi-leg strategy (straddle, spread, iron
-- condor, ...) share a strategy_id so they can be grouped/rolled/closed
-- together later. NULL for an ordinary single-leg trade.
ALTER TABLE positions ADD COLUMN strategy_id TEXT;

CREATE INDEX idx_positions_strategy ON positions(strategy_id) WHERE strategy_id IS NOT NULL;
