# T15 native deposit smoke test

The [first checked receipt and balance record](evm2-t15-native-deposit.json) was produced
on an isolated Tempo and Zone devnet from Tempo `5bb038c865a7938589731e7e593add402732afbb`
and Zones `393eb41cc`. EVM2 and Reth were pinned at
`1e4dbf287df62e5b24fef4f7dbae32deb14b4fc5` and
`11ad602fe1ad2cbff2d01ad21c59dec9af4e1f4a` respectively. The binary
SHA-256 values, genesis hashes, client versions, fork timestamp, transaction
hashes, and reconciled balances are in the JSON record.

Tempo ran on `127.0.0.1:28545` with a copy of the dev genesis scheduled for
T15 at Unix timestamp `1790907639`. Zone 1 used `127.0.0.1:29545`. Before
provisioning, the dev chain required the historical TIP-20 policy binding
migration; transaction `0x5bf3e0e7fe93ce313e9f8bde99a33532023abf9577e185b41e4b4030dd2c11de`
called `migrateTransferPolicyIds([pathUSD])` successfully. This is a dev-genesis
setup step, not an activation migration of existing vault or Zone positions.
Zone 1 was created after T15 activated, so this run does not demonstrate a
pre-fork Zone surviving the boundary.

The deposit transaction
`0x4bb4bbd364e91c04074cc64ecf0cac1e435cc7c777d0a2710c12cac2fe41cd57`
was included at L1 block 339, after the fork. Its direct portal call succeeded,
used 586,967 gas, and emitted three logs. `debug_traceTransaction` shows a
TIP-20 `transferFrom` child `CALL` and no portal `DELEGATECALL`, demonstrating
the EVM2 native dispatch path. The Zone credited 1,000,000 pathUSD at L2 block
107. L1 portal backing, private account balance, and Zone token supply each
equal 1,000,000.

The Zone submitted batch transaction
`0x21188db6234c5ad0872284b826875baca09fc9ba788a9c782d85974d5a6dc244`.
It settled at L1 block 403 with Tempo anchor block 339 and advanced the
deposit cursor from 0 to 1. Its verifier configuration is explicit NoProof
`0x02` with an empty proof. It does not satisfy cryptographic proof settlement.
That binary predates the verified-identity payment-lane classifier, so the
first record does not claim zero general-lane usage.

Tempo was then rebuilt from `0f6f54a45` and restarted on the same datadir.
The [second checked record](evm2-t15-payment-lane-deposit.json) covers deposit
`0x7e83e9070a1008b514c5c70a251f223f33b5100d9d20c7c2b675b0b6e953d10c`
in L1 block 1546. That block contains only the deposit. Its receipt uses
93,267 gas; the per-payload metric sample at the block's Unix timestamp reports
one included payment transaction, 93,267 payment gas, and **zero general gas**.
The native trace again has a TIP-20 `transferFrom` child and no `DELEGATECALL`.
Zone 1 credited the second 1,000,000 units, bringing private balance, portal
backing, and Zone supply to 2,000,000 each. Batch
`0xe47a1dd27b2ee9d9dbc90737082880bd8cfacb9353cb2a57de04f4cebfb4f615`
settled at L1 block 1610 with anchor 1546 and advanced the deposit cursor from
1 to 2. It also used NoProof mode. This is one deposit's lane attribution,
not a sustained mixed-workload or cryptographic-proof result. Native
withdrawals, Earn, migration, throughput, and the final scheduled upgrade
remain open.

The same checker validates a forged endpoint failure. Transaction
`0x3aaaa35fd26232aca5ee04781ebe80bb022222228f4628570121870d7e15b85b`
sent canonical deposit calldata to unregistered Zone ID 2. It reverted with
no child call, changed no real portal backing, and was the only transaction in
L1 block 2168. The matched metric sample reports zero payment gas and 29,850
general gas. This proves the candidate classifier did not grant payment
capacity to that forged portal in this devnet run; delegated-code and AA
failure cases still need testing.

Rerun the checker against the still-running isolated devnet with:

```bash
python3 scripts/native-payments/check-t15-deposit.py \
  --l1-rpc-url http://127.0.0.1:28545 \
  --zone-rpc-url http://127.0.0.1:29545 \
  --portal 0x5ad0000000000000000000000000000000000001 \
  --account 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266 \
  --deposit-tx 0x4bb4bbd364e91c04074cc64ecf0cac1e435cc7c777d0a2710c12cac2fe41cd57 \
  --settlement-tx 0x21188db6234c5ad0872284b826875baca09fc9ba788a9c782d85974d5a6dc244 \
  --fork-time 1790907639 --amount 1000000 \
  --l1-balance-block 339 --zone-balance-block 107 \
  --tempo-revision 5bb038c865a7938589731e7e593add402732afbb \
  --zones-revision 393eb41cc \
  --tempo-binary-sha256 da556a423a683fd7facf88bbcbb952cccf6149e68eaeb29826941dff259a423c \
  --zone-binary-sha256 f86905c407b5c3137ef092319917727cfe4fa2fb44349ccb10f782d585b8e17d \
  --output /tmp/evm2-t15-native-deposit-recheck.json
```

For the second run, start the checked-in metric sampler before submitting a
deposit and retain its JSONL output:

```bash
python3 scripts/native-payments/capture-lane-metrics.py \
  --metrics-url http://127.0.0.1:29001/metrics --seconds 90 \
  --output /tmp/evm2-t15-lane-metrics.jsonl &
metrics_pid=$!
# Submit a 1,000,000-unit deposit during the capture window.
wait "$metrics_pid"
```

The checker accepts `--lane-metrics-file` with that output, plus
`--cursor-before 1 --expected-total 2000000 --l1-balance-block 1546
--zone-balance-block 1258`, and requires the single-transaction deposit block's
timestamp and gas to match a sample with zero general gas. Supply
`--forged-tx`, `--forged-portal`, and `--forged-metrics-file` to check a
single-transaction forged candidate against general-lane counters.
