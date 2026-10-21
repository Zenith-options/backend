UPDATE positions
SET contracts = CAST(ROUND(contracts * 10000000.0) AS INTEGER);
