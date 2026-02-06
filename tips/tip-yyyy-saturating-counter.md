---
id: TIP-YYYY
title: Saturating Atomic Counters
description: Introduce SaturatingCounter, a lock-free atomic counter that returns explicit errors on overflow and underflow instead of silently wrapping.
author: Artem Bogomaz (artembogomaz@gmail.com)
status: Draft
related:
protocolVersion: TBD
---

# TIP-YYYY: Saturating Atomic Counters

## Abstract

This TIP introduces `SaturatingCounter`, a lock-free atomic counter backed by `AtomicU32` that returns explicit errors on overflow and underflow via compare-and-swap loops. The primitive targets off-chain subsystems (transaction pool, payload builder) where silent integer wraparound could cause DoS vectors or incorrect resource accounting. It does not alter consensus, storage layout, or on-chain state.

## Motivation

Several Tempo subsystems use counters to enforce resource limits:

1. **Transaction pool sender tracking** (`tt_2d_pool.rs`): A `HashMap<Address, usize>` counter tracks pending transactions per sender, bounded by `max_txs_per_sender`. The decrement path uses unchecked subtraction (`*count -= 1`), which would underflow to `usize::MAX` if called with a zero count due to a bug in the add/remove logic. While the current limit of 16 makes this unlikely, a logic error in any of the 6 decrement call sites could silently corrupt the counter, bypassing the per-sender DoS limit entirely.

2. **Payload builder safety** (`payload/builder/src/lib.rs`): An `AtomicU64` tracks the highest block height with an invalid subblock. This is currently used as a flag (store/load only), but extending it to track counts of invalid subblocks per height would benefit from overflow protection.

3. **Future AMM lock tracking** (`tip_fee_manager/`): The fee manager AMM does not currently use lock counters, but liquidity lock accounting would require counters where underflow (releasing a lock that was never acquired) must be caught explicitly.

The core problem: Rust's `AtomicU32::fetch_add` and manual `*count -= 1` wrap silently on overflow/underflow. In resource-limiting contexts, wraparound can turn a counter from 0 to `u32::MAX`, effectively disabling the limit.

**Alternatives considered:**

- **Checked arithmetic (`checked_add` / `checked_sub`)**: Returns `Option` but is not atomic. A load-check-store sequence is not thread-safe without external synchronization.
- **Saturating arithmetic (`saturating_add`)**: Clamps at bounds instead of wrapping, but silently loses information — the caller never knows the operation failed. This is unsuitable for resource limits where exceeding a bound must be an error.
- **Mutex-guarded counters**: Thread-safe and explicit, but introduce lock contention. `SaturatingCounter` achieves the same safety with lock-free CAS loops.

---

# Specification

## SaturatingCounter Primitive

A lock-free atomic counter backed by `AtomicU32` that uses compare-and-swap loops to guarantee overflow and underflow are caught before they occur. All operations use `Ordering::SeqCst` for sequential consistency.

`SaturatingCounter::default()` creates a counter initialized to 0.

### Error Types

```rust
#[derive(Error, Debug)]
pub enum CounterError {
    #[error("counter overflow at maximum value")]
    Overflow,
    #[error("counter underflow at zero")]
    Underflow,
}
```

### Operations

| Operation | Signature | Behavior |
|-----------|-----------|----------|
| `new(initial)` | `u32 → SaturatingCounter` | Create counter with initial value |
| `get()` | `→ u32` | Read current value (atomic load) |
| `increment()` | `→ Result<u32, CounterError>` | Add 1; error if at MAX |
| `decrement()` | `→ Result<u32, CounterError>` | Subtract 1; error if at 0 |
| `try_add(delta)` | `u32 → Result<u32, CounterError>` | Add delta; error if overflow |
| `try_sub(delta)` | `u32 → Result<u32, CounterError>` | Subtract delta; error if underflow |

All mutating operations return the new value on success.

### CAS Loop Semantics

Each mutating operation follows the pattern:

```
loop:
  current = atomic_load(SeqCst)
  new_value = checked_op(current, delta) or return Err(Overflow/Underflow)
  if compare_exchange(current, new_value, SeqCst, SeqCst) succeeds:
    return Ok(new_value)
  else:
    retry (another thread modified the value)
```

The CAS loop guarantees:
- **Atomicity**: The check and update happen as a single logical operation
- **Progress**: At least one thread makes progress per CAS round (lock-free)
- **Correctness**: The bounds check is always performed against the actual current value, not a stale read

## Transaction Pool Sender Count

The `txs_by_sender` field in `AA2dPool` currently uses `AddressMap<usize>` with unchecked arithmetic. Replace with `AddressMap<SaturatingCounter>` to catch underflow in the 6 decrement call sites.

### Current Code

```rust
*self.txs_by_sender.entry(sender).or_insert(0) += 1;

fn decrement_sender_count(&mut self, sender: Address) {
    if let Occupied(mut entry) = self.txs_by_sender.entry(sender) {
        let count = entry.get_mut();
        *count -= 1;          // underflow possible if count is 0
        if *count == 0 {
            entry.remove();
        }
    }
}
```

### Updated Code

```rust
self.txs_by_sender
    .entry(sender)
    .or_insert_with(SaturatingCounter::default)
    .increment()?;

fn decrement_sender_count(&mut self, sender: Address) -> Result<(), CounterError> {
    if let Occupied(mut entry) = self.txs_by_sender.entry(sender) {
        let new_val = entry.get().decrement()?;
        if new_val == 0 {
            entry.remove();
        }
    }
    Ok(())
}
```

The error propagation surfaces bugs immediately instead of silently corrupting the counter.

## Payload Builder Subblock Tracking

The `highest_invalid_subblock: Arc<AtomicU64>` in `TempoPayloadBuilder` is currently store/load only. A `SaturatingCounter` is not needed here today but can be adopted if per-height invalid subblock counting is added in the future.

## AMM Liquidity Locks (Future)

When lock tracking is added to `TipFeeManager`, `SaturatingCounter` provides safe acquire/release semantics:

```rust
acquire_lock:  active_locks.increment() or return TooManyLocks
release_lock:  active_locks.decrement() or return NoActiveLock
```

This prevents the counter from going negative on double-release bugs.

## Implementation Location

- Add to: `tempo-primitives/src/lib.rs` (new `SaturatingCounter` type and `CounterError` enum)
- Apply in:
  - `tempo-transaction-pool/src/tt_2d_pool.rs` — replace `AddressMap<usize>` sender count
  - `tempo-payload-builder/src/lib.rs` — future subblock count tracking
  - `tempo-precompiles/src/tip_fee_manager/` — future liquidity lock counting

This TIP does not modify consensus rules, block validity, transaction execution, storage layout, gas costs, or subblock encoding. It affects only off-chain subsystems (mempool, block builder). Existing behavior is preserved; the only difference is that bugs that would have caused silent wraparound now return explicit errors.

---

# Invariants

1. **Overflow prevention**: `increment()` and `try_add(delta)` MUST return `CounterError::Overflow` if the result would exceed `u32::MAX`. The counter value MUST NOT change on error.

2. **Underflow prevention**: `decrement()` and `try_sub(delta)` MUST return `CounterError::Underflow` if the result would go below 0. The counter value MUST NOT change on error.

3. **Atomicity**: Each mutating operation MUST be atomic — no intermediate state is observable by other threads.

4. **Lock-freedom**: The CAS loop MUST guarantee system-wide progress. At least one thread completes its operation per CAS round.

5. **Monotonic success values**: On success, `increment()` returns a value strictly greater than the previous value. On success, `decrement()` returns a value strictly less than the previous value.

6. **Identity**: `new(n).get() == n` for any `n: u32`.

7. **Round-trip**: For any counter with value `v > 0`: `increment()` followed by `decrement()` restores value to `v`.

8. **Zero default**: `SaturatingCounter::default().get() == 0`.

9. **Sender count consistency**: After applying `SaturatingCounter` to `txs_by_sender`, the counter for any sender MUST equal the number of that sender's transactions currently in the pool.

10. **Error propagation**: All call sites MUST handle `CounterError` — silent drops or ignoring the error defeats the purpose of the primitive.
