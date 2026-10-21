# Keeper bounty accounting

`keeper_bounties` records one accrued reward per completed keeper job. The
job id is the primary key, so retries cannot accrue the same work twice.
Amounts are integer stroops and must be positive. The accounting endpoint is
`GET /api/v1/keeper/bounties`; it uses the caller's active session wallet and
returns only that wallet's balance and job history.

Accrual is accepted only when the corresponding `keeper_job_executions` row
has status `completed`. The job-type schedule is owned by the keeper runner
configuration and is supplied when the runner records a completed job.

The current chain submission boundary does not construct or sign Stellar
payments. Accordingly, this change does not mark any bounty paid or treat an
unverified transaction hash as payment. A chain-backed payout executor must
confirm a transfer before the ledger is transitioned to `paid`.
