# Zone forced withdrawals

A forced withdrawal requests the full liquid balance of one token from a Zone
account through Tempo L1, without a user L2 transaction or outbox allowance.
It still requires live Zone execution and settlement; it is not an exit path
when the operators stop running.

This crate provides payload encoding, root-signature authorization, and hashing
helpers. Stateful execution lives in the Zone inbox and outbox precompiles.

## Flow

1. **Request on L1.** The account signs an authorization and encrypts it with
   the Zone's published encryption key. `ZonePortal.requestForcedExit` collects
   compensation from the fee payer, pays the portal admin, and appends the
   request to the shared inbox. The fee payer may differ from the account.
2. **Execute on the Zone.** The inbox verifies the decryption witness, decrypts
   the request, and checks authorization, the admission deadline, replay, and
   applicable policies. A successful request burns the account's full liquid
   balance at execution time and creates an ordinary withdrawal. Rejected and
   empty requests advance the inbox without a withdrawal; fatal execution
   errors roll back the transition.
3. **Settle on L1.** The sequencer submits the inbox progress and ordinary
   withdrawal commitments through the existing quorum-attested batch path.
   There is no separate forced-withdrawal outcome array or public rejection
   status.
4. **Deliver or recover.** Ordinary withdrawal processing pays the L1 recipient.
   Failed delivery queues a bounce-back to the debited Zone account, using the
   existing pending-credit recovery path if policy prevents an immediate mint.
   Admission compensation is not refunded.

## Activation

L1 admission activates when a coordinated Tempo hard fork installs the forced-exit
portal runtime. Historical portal runtimes remain in use before that fork and do
not support forced requests. `forcedExitVersion()` is a pure format accessor that
always returns 1 in the new runtime; it is not an activation flag. There is no
version storage slot, operator enable transaction, or activation-time version bump.

## Status and usage

Production admission remains disabled until a coordinated protocol upgrade
activates it. Native end-to-end tests use an explicitly activated test portal.
The current L1 `Verifier` is a stub; execution-proof enforcement is not provided
by this implementation.

`cargo run -p tempo-xtask -- forced-withdraw --help` describes the client command.
It needs only L1 RPC. Its optional `--wait-for-processing` confirms settled inbox
progress, not successful payout.

## Code

- [Codec and authorization](src/lib.rs)
- [L1 admission](../contracts/src/runtime/tempo/ZonePortal.sol)
- [Inbox execution](../precompiles/src/inbox/forced.rs) and
  [withdrawal construction](../precompiles/src/outbox/forced.rs)
- [Settlement](../sequencer/src/settlement.rs)
- [Native end-to-end tests](../node/tests/it/forced_exit_e2e.rs)
