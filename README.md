# upgradeable-loader-adventures

LiteSVM-backed tests for Solana upgradeable loader edge cases.

## Fixture

- program id: `MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr`
- program file: [`memo.so`](memo.so)

## Coverage

The tests cover loader-v3 account-state transitions around:

- `Close` truncating loader-owned accounts
- `Upgrade` draining buffers and preserving metadata
- trailing system transfers to drained buffers
- failure paths that depend on `Buffer` versus `Uninitialized` state

LiteSVM keeps these cases in-process, so the suite avoids validator startup time and RPC polling.

## Running

```bash
cargo test --offline -- --test-threads=1
```
