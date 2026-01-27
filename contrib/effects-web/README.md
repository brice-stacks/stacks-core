# Effects Preview UI (hackathon prototype)

This is a lightweight standalone web page that:
- pulls a contract ABI from the Hiro API
- calls `stacks-inspect contract-effects --json`
- renders available functions, arguments, and side‑effects with a simple call graph
- can also load a transaction by txid (and optional block id) to show effects

## Prereqs
- Node.js 18+ (for built‑in `fetch`)
- A built `stacks-inspect` binary
- Local chainstate data (same path you use with `stacks-inspect contract-effects`)

## Build `stacks-inspect`
```bash
cargo build -p stacks-inspect
```

## Run the server
```bash
node contrib/effects-web/server.js \
  --db /path/to/chainstate/root \
  --network xenon \
  --port 3939
```

Then open: http://localhost:3939

Notes:
- `--network` is passed through to `stacks-inspect` (e.g. `mainnet`, `helium`, `xenon`, `mocknet`).
- If your `stacks-inspect` binary is not at `target/debug/stacks-inspect`, set:
  ```bash
  STACKS_INSPECT_BIN=/path/to/stacks-inspect
  ```
