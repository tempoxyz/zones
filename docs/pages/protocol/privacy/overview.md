# Tempo Zones Overview

Tempo Zones are non-custodial sidechains that extend Tempo with scaleable, private settlement. Each zone operates as a Tempo-compatible execution environment with its own sequencer, with the ability to synchronously read from the rest of Tempo state and asynchronously withdraw.

Tempo zones offer some unique advantages:

* Non-custodial. Sequencers cannot steal funds from users.
* Scalable. All of the data and computation on Tempo zones is performed offchain.
* Fully private from third parties. Since transaction data is not posted onchain, it is impossible for third parties looking at the chain to learn any information about activity on zones. 
* Quantum-secure. Since not even encrypted data is posted to the chain, zones have forward secrecy against future advances in cryptanalysis.
* Compliant. Transactions on Tempo zones provably follow the same rules as transactions on Tempo, including blacklist policies set by token issuers.
* Deeply integrated. Users of Tempo zones can interact with any applications on Tempo. Contracts on zones have instant access to the entire state of the Tempo chain, allowing transactions in the zones to synchronously read from any contract on Tempo. Deposits and withdrawals between the zone and the Tempo chain can land within seconds.

Zones make some tradeoffs to achieve these properties. Sequencers have full visibility into activity on the zone, and are trusted for data availability and liveness.

This document provides a high-level overview of how zones work and why they are designed this way.

## What is a Privacy Zone?

A privacy zone is a dedicated blockchain environment where one sequencer controls block production and visibility. None of the data for a zone is published on the main Tempo chain. Instead, the sequencer publishes commitments to the current state of the chain, along with proofs that it was correctly executed. These proofs allow funds to be moved in and out of the zone. In crypto, this general category of scaling solution is known as a [validium](https://x.com/VitalikButerin/status/1267455602764251138).

Each zone supports one TIP-20 stablecoin, which also serves as the gas token. This stablecoin can be bridged into the zone nearly instantly, and bridged out as soon as a validity proof is posted (targeting <10 seconds).

Privacy zones are tightly integrated with Tempo. Every contract in the zone can read from any contract in the Tempo state, with that state updated immediately when a Tempo block finalizes. Withdrawals to Tempo are processed on Tempo by the sequencer, and can trigger transfers to other addresses, trades on Tempo's StablecoinDEX, and even deposits into other zones, without further interaction from the user.

Zones are non-custodial. Sequencers cannot steal funds from any user or contract on the zone, and proofs guarantee that the total balances in the zone match the total amount currently deposited into it. Zones also guarantee that sequencers must follow the permissioning set by the token issuer on Tempo.

Zones are designed for applications that want to be non-custodial and to provide their users with privacy from the rest of the world, but where users are comfortable trusting a sequencer (which could be operated by the application, by the issuer of the stablecoin, or by a third party) for liveness and privacy.

## Architecture Overview

### System Architecture

Each zone runs as a Reth Execution Extension (ExEx) colocated with a Tempo node. This tight coupling enables zones to have direct, synchronous access to Tempo state—zone contracts can read from any contract on Tempo without cross-chain messaging delays.

```
┌─────────────────────────────────────────────────────────────────┐
│                        Tempo Node                               │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │                    Tempo Execution                        │  │
│  └───────────────────────────────────────────────────────────┘  │
│                              │                                  │
│              Tempo state accessible to zones                    │
│                              │                                  │
│  ┌─────────────────┐  ┌─────────────────┐                       │
│  │   USDC Zone     │  │   USDT Zone     │  ...                  │
│  │     ExEx        │  │     ExEx        │                       │
│  └─────────────────┘  └─────────────────┘                       │
└─────────────────────────────────────────────────────────────────┘
```

The sequencer runs a Tempo node with one or more zone ExExes attached. Each ExEx:

- **Synchronizes** the zone's view of Tempo each time a Tempo block is finalized
- **Executes zone transactions** using privately submitted transactions and the zone's own state
- **Produces batches** proving state transitions on the zone, and posts them to Tempo
- **Watches for deposits** by monitoring portal events on Tempo, and creates corresponding transactions on the zone once the block is finalized
- ***Watches for withdrawals** on the zone, and submits transactions to Tempo processing them once the batch has been proven

This architecture means zones inherit Tempo's view of the world. When a Tempo block finalizes, the zone can immediately read that state—enabling use cases like checking token balances, reading oracle prices, or verifying permissions without waiting for cross-chain relay.

### Contract Architecture

The system consists of contracts on both Tempo (the main chain) and within each zone.

```
┌─────────────────────────────────────────────────────────────────┐
│                           TEMPO                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────────────┐  │
│  │  ZoneFactory │  │  ZonePortal  │  │    ZoneMessenger      │  │
│  │  (deploys)   │  │  (escrow)    │  │    (callbacks)        │  │
│  └──────────────┘  └──────────────┘  └───────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
                   deposits ↓ │ ↑ withdrawals
                              │
┌─────────────────────────────────────────────────────────────────┐
│                            ZONE                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────────────┐  │
│  │  TempoState  │  │  ZoneInbox   │  │     ZoneOutbox        │  │
│  │  (L1 view)   │  │  (deposits)  │  │    (withdrawals)      │  │
│  └──────────────┘  └──────────────┘  └───────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### Tempo-Side Contracts

**ZoneFactory** deploys new zones. Each zone gets its own portal and messenger contracts with deterministic addresses.

**ZonePortal** is the central bridge contract. It holds all deposited tokens in escrow, verifies validity proofs, and processes withdrawals. The portal maintains the authoritative state: which deposits have been made, which batches have been proven, and which withdrawals are pending.

**ZoneMessenger** handles withdrawals that include callbacks. When a user wants to withdraw tokens and trigger a contract call atomically, the messenger executes both operations together. If the callback fails, the entire withdrawal reverts and funds bounce back to the zone.

### Zone-Side Contracts

**TempoState** is a system contract at a fixed address that stores the zone's view of Tempo. The sequencer updates this contract with Tempo block headers, allowing zone contracts to read Tempo state within proofs.

**ZoneInbox** processes incoming deposits. At the start of each zone block, a system transaction calls this contract to mint tokens to recipients. The inbox validates that processed deposits match what the portal expects.

**ZoneOutbox** handles withdrawal requests. Users burn their zone tokens here and specify a Tempo recipient. At the end of each batch, the sequencer finalizes pending withdrawals into a queue that will be proven and processed on Tempo.

## How Deposits Work

Deposits move tokens from Tempo into a zone:

1. **User deposits on Tempo**: Call `ZonePortal.deposit()` with the recipient address, amount, and optional memo. The portal transfers tokens from the user into escrow and records the deposit in a hash chain.

2. **Sequencer relays deposits**: The sequencer watches for deposit events and includes them in zone blocks.

3. **Zone processes deposits**: At block start, `ZoneInbox.advanceTempo()` processes deposits in order, minting the zone's native token to each recipient. The inbox builds a hash chain that must match the portal's record.

4. **Proof validates processing**: When the sequencer submits a batch proof, it demonstrates that deposits were processed correctly by reading the portal's state from within the proof.

Deposits always succeed on the zone side. There are no callbacks or failure modes—tokens simply appear in the recipient's balance.

## How Withdrawals Work

Withdrawals move tokens from a zone back to Tempo in two stages:

### Stage 1: Request and Batch

1. **User requests withdrawal**: Call `ZoneOutbox.requestWithdrawal()` with the Tempo recipient, amount, and optional callback data. The outbox burns the tokens plus a processing fee.

2. **Sequencer finalizes batch**: At the end of a batch period, the sequencer calls `finalizeWithdrawalBatch()` to package all pending withdrawals into a hash chain.

3. **Proof submitted**: The sequencer submits a validity proof to the portal, which verifies correct execution and records the withdrawal queue.

### Stage 2: Processing

4. **Sequencer processes withdrawals**: For each withdrawal, call `ZonePortal.processWithdrawal()`. The portal verifies the withdrawal against the queue and transfers tokens to the recipient.

5. **Callbacks execute**: If the withdrawal specified a callback, the messenger transfers tokens and executes the call atomically.

6. **Failures bounce back**: If a transfer or callback fails, the portal creates a "bounce-back" deposit that returns funds to the zone. The sequencer keeps the processing fee regardless of success.

## Trust Model

Privacy zones make explicit tradeoffs between trust and performance:

| What You Trust | What Could Go Wrong |
|----------------|---------------------|
| Sequencer for liveness | Zone halts if sequencer stops. |
| Sequencer for inclusion and ordering | Transactions (including withdrawals) can be excluded from the zone, or reordered. |
| Sequencer for privacy | The sequencer can see all transactions on the zone. |
| Sequencer for data | It is impossible to reconstruct the state of the zone without the sequencer. |
| Sequencer + verifier for correctness | If there is a critical safety bug in the verifier or proving system, and the sequencer is malicious, they could exploit it to steal funds |

The sequencer cannot steal funds or forge state transitions—validity proofs prevent this. However, the sequencer can halt the zone entirely, censor specific users, or reorder transactions for MEV.

## Creating a Zone

Zones are created through the `ZoneFactory`:

1. Choose a TIP-20 token to serve as the zone's native asset
2. Select a verifier contract (ZK prover or TEE attestor)
3. Designate a sequencer address
4. Call `ZoneFactory.createZone()` with these parameters

The factory deploys a new `ZonePortal` and `ZoneMessenger` for the zone. The zone itself runs as a separate chain with the system contracts deployed at genesis.

## Further Reading

- [Contract Design](./contract-design.md) — Detailed contract specifications
- [Prover Design](./prover-design.md) — How validity proofs work
