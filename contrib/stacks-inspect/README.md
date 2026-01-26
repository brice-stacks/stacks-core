# stacks-inspect

A multifunction inspection CLI for Stacks chain data and networking.

Highlights:
- Decode primitives: Bitcoin headers/txs/blocks, Stacks blocks/microblocks, P2P net messages
- Chain queries: ancestors, MARF lookups, tenure info, PoX anchor evaluation
- Mining helpers: `try-mine`, `tip-mine`, sortition (anti-MEV) analysis
- Shadow chain tools: build, patch, repair, and verify shadow chainstate
- Replay: re-execute blocks and microblocks for diagnostics
- Effects analysis: per-contract and per-transaction read/write summaries (with optional call graph)

Build:
```bash
cargo build -p stacks-inspect
```

Basic usage:
```bash
# Show version
cargo run -p stacks-inspect -- --version

# Example: decode a bitcoin header from file
cargo run -p stacks-inspect -- decode-bitcoin-header <HEIGHT> <PATH>

# Example: analyze anti-MEV behavior over a height range
cargo run -p stacks-inspect -- analyze-sortition-mev <burn_db> <sort_db> <chainstate_db> <start> <end> [miner advantage ...]

# Example: show contract effects from chainstate
cargo run -p stacks-inspect -- contract-effects <chainstate_db> <SP...contract> --json

# Example: show effects for a raw transaction hex (string, @file, or stdin "-")
cargo run -p stacks-inspect -- tx-effects <chainstate_db> <tx-hex|@file|-> --json --graph

# Example: show effects by txid (txindex on), with optional block-id fallback
cargo run -p stacks-inspect -- txid-effects <chainstate_db> <txid-hex> --json
cargo run -p stacks-inspect -- txid-effects <chainstate_db> <txid-hex> --block-id <index-block-hash> --json
```

For detailed commands and flags, run:
```bash
cargo run -p stacks-inspect -- --help
```

Notes:
- Some commands expect mainnet data paths by default and may require specific network contexts.
- `txid-effects` requires txindex; if it is disabled, pass `--block-id` to scan a specific block.
- Operations that write data (e.g., shadow chain tools) are destructive—use copies of data directories when experimenting.
