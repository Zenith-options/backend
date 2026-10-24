# Settlement controls

All settlement amounts use the protocol's integer asset scale. Pro-rata payouts
round down once, after multiplication and before transfer. A payout below the
configured minimum is suppressed. Division remainders and suppressed payouts
are credited to the asset's dust account, so `sum(payouts) + dust = pool debit`
exactly. Dust is swept to the configured treasury when either its threshold or
maximum ledger age is reached, and every sweep is recorded for reconciliation.

New position writes are checked atomically against the effective versioned cap
set: halt flag, global notional, optional account notional, underlying open
interest, and series open interest. Lowering a cap below current exposure
freezes new writes but never blocks releases. Exercise, expiry, transfer and
close-out release the same counters consumed by a write.

Multi-party settlement follows prepare, collect, assemble, submit. A pending
batch binds its required signers to one invocation-tree hash, expiry ledger and
deadline. Submitted XDR remains opaque at rest and is accepted only for an
expected signer, the exact invocation hash, sufficient ledger lifetime, and a
fingerprint not used by another batch. Assembly is side-effect free and occurs
only after every signer is present, preventing partial submission.
