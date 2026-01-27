# Clarity Effects Analyzer

## What it does

Static analysis pass that reports:

- Read/write side-effects per function (maps, vars, assets, chain state).
- Contract-call call graph across contracts.
- Principal attribution (tx-sender, contract-caller, current-contract, arg
  principals).

Goal: make contract behavior auditable without executing the transaction.

## Why it matters

- Wallet safety: show a signer what a tx can touch before signing.
- Faster execution: enables prefetching or parallelization where safe.
- Auditing: quickly answer “what does this function read/write?”

## How it works (short)

We build the Clarity AST, run the effects analyzer, and propagate:

1. intra-contract calls,
2. inter-contract calls (recursively),
3. principal references from tx args and well-known principals.

## Sample CLI output: Contract effects

```
cargo run -p stacks-inspect -- --network xenon contract-effects /Users/brice/work/testnet-data/krypton ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-deposit
    Finished `dev` profile [optimized + debuginfo] target(s) in 0.23s
     Running `target/debug/stacks-inspect --network xenon contract-effects /Users/brice/work/testnet-data/krypton ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-deposit`
Contract: ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-deposit
Clarity version: Clarity3

Function complete-deposit-wrapper (Impure)
  reads:
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map active-protocol-contracts)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map active-protocol-roles)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map deposit-status)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-aggregate-pubkey)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-signature-threshold)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-signer-principal)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-signer-set)
    - chain-state burn-block-info
  writes:
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map completed-deposits)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map deposit-status)
    - asset ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-token.sbtc-token (ft) principal $arg[3]

Function complete-deposits-wrapper (Impure)
  reads:
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-signer-principal)

Function complete-individual-deposits-helper (Impure)
  reads:
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map active-protocol-contracts)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map active-protocol-roles)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map deposit-status)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-aggregate-pubkey)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-signature-threshold)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-signer-principal)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (var current-signer-set)
    - chain-state burn-block-info
  writes:
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map completed-deposits)
    - contract ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-registry (map deposit-status)
    - asset ST1F7QA2MDF17S807EPA36TSS8AMEFY4KA9TVGWXT.sbtc-token.sbtc-token (ft) principal $arg[3]

Function get-burn-header (Impure)
  reads:
    - chain-state burn-block-info
```

## Sample CLI output: Transaction effects (with call graph)

```
cargo run -p stacks-inspect -- --network xenon txid-effects /Users/brice/work/testnet-data/krypton e38d15d20dcbb165125d7ffed8a644b019941f44b45767f1f35e36f779531e71 --block-id a4c25ecd95ce17fc11ecf5cdfbc5a8999cfd9ab53bd54b230e0172437a5a1f23 --graph
    Finished `dev` profile [optimized + debuginfo] target(s) in 0.24s
     Running `target/debug/stacks-inspect --network xenon txid-effects /Users/brice/work/testnet-data/krypton e38d15d20dcbb165125d7ffed8a644b019941f44b45767f1f35e36f779531e71 --block-id a4c25ecd95ce17fc11ecf5cdfbc5a8999cfd9ab53bd54b230e0172437a5a1f23 --graph`
Txid: e38d15d20dcbb165125d7ffed8a644b019941f44b45767f1f35e36f779531e71 (block a4c25ecd95ce17fc11ecf5cdfbc5a8999cfd9ab53bd54b230e0172437a5a1f23)
Type: contract-call ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7.supply-usdcx
  reads:
    - contract ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7 (map positions)
    - contract ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7 (map user-position-ids)
    - contract ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7 (var next-position-id)
    - contract ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7 (var pool-state)
    - asset stx principal ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5
    - asset ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.usdcx.usdcx-token (ft) principal ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5
    - asset ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.usdcx.usdcx-token (ft) principal ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7
    - account-nonce ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5
  writes:
    - contract ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7 (map positions)
    - contract ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7 (map user-position-ids)
    - contract ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7 (var next-position-id)
    - contract ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7 (var pool-state)
    - asset stx principal ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5
    - asset ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.usdcx.usdcx-token (ft) principal ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5
    - asset ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.usdcx.usdcx-token (ft) principal ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7
    - account-nonce ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5
  call-graph (might-call):
    - ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7.supply-usdcx -> ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.usdcx.get-balance
    - ST1Q45DJT4SQF2Q7TPGQS3Q52GBM495QRR4AJYGB5.lending-pool-v7.supply-usdcx -> ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.usdcx.transfer
```

## Sample Web UI: Contract Mode

![Contract mode](images/contract-mode.png)

## Sample Web UI: Transaction Mode

![Transaction mode](images/transaction-mode.png)
