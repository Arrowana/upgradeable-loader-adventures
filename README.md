# upgradeable-loader-adventures

Small validator-backed tests for upgradeable loader edge cases.

## Why `solana-test-validator`

These tests use `solana-test-validator` on purpose instead of a lighter mock harness because the behavior under test lives in the real upgradeable loader program.

The validator already ships the real upgradeable loader program, and this repo starts it with a preloaded upgradeable memo program:

- program id: `MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr`
- program file: [`memo.so`](/Users/arowana/projects/experiments/upgradable-program-adventures/memo.so)
- upgrade authority: [`test-upgrade-authority.json`](/Users/arowana/projects/experiments/upgradable-program-adventures/test-upgrade-authority.json)

That setup keeps the tests focused on loader-v3 account-state transitions instead of spending time deploying a program inside each test.

The cases in this repo depend on loader-v3 runtime details such as:

- how `Close` truncates loader-owned accounts
- how `Upgrade` drains and preserves buffer metadata
- how a drained buffer behaves after a trailing system transfer
- which sequences fail because the loader sees the account as `Buffer` versus `Uninitialized`

`solana-test-validator` gives us the actual loader implementation that ships with the validator, so the tests exercise real account-state transitions and real instruction validation paths rather than approximations.

That matters here because several interesting outcomes are loader-specific:

- `closed_buffer_can_be_reused_after_close`
- `atomic_close_and_recreate_with_zero_authority`
- `trailing_system_transfer_keeps_upgraded_buffer_tombstoned`

In particular, the third test shows that after `Upgrade`, a trailing system transfer can fund the drained buffer again, but that does not restore write capacity. The account remains tombstoned loader state, and a later `Write` still fails.

## Running

```bash
cargo test --offline -- --test-threads=1
```
