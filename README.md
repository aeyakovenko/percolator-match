# percolator-match

> **⚠️ DISCLAIMER: FOR EDUCATIONAL PURPOSES ONLY**
>
> This code has **NOT been audited**. Do NOT use in production or with real funds. This is experimental software provided for learning and testing purposes only. Use at your own risk.

Passive LP matcher for [Percolator](https://github.com/aeyakovenko/percolator-prog).

## Overview

A Solana program that provides passive market making by quoting ±50 basis points off the oracle price. Called via CPI from Percolator's `TradeCpi` instruction.

## Features

- **Passive quoting**: Bid/ask spread of 50 bps (0.5%) around oracle price
- **Integer-only math**: Deterministic, no floating point
- **Rounding**: Bid rounds down, ask rounds up (both passive-favorable)
- **LP PDA binding**: Context account stores expected LP PDA, verified on each call
- **ABI compatible**: Implements Percolator's matcher context account interface

## Quote Calculation

```
bid = floor(oracle_price × 9950 / 10000)
ask = ceil(oracle_price × 10050 / 10000)
```

Example with oracle price of 100,000:
- Bid: 99,500
- Ask: 100,500

## Building

```bash
# Build for Solana
cargo-build-sbf

# Run tests
cargo test
```

## Deployment

The compiled program is output to `target/deploy/percolator_match.so`.

### Setup

1. Deploy the matcher program
2. Create a context account owned by the matcher program (minimum 96 bytes, recommended 320 bytes)
   and initialize it with the LP PDA (Tag 2 instruction) in the same transaction, signed by the
   context account's keypair. Initialization requires that signature: a context created in one
   transaction and initialized in another could otherwise be initialized first by anyone.
3. Register the LP with Percolator via `InitLP`, setting:
   - `matcher_program`: This program's deployed address
   - `matcher_context`: The initialized context account

## Context Account Layout

| Offset | Field | Size | Description |
|--------|-------|------|-------------|
| 0-63 | return | 64 | Matcher return data (written on each call) - ABI required |
| 64-95 | lp_pda | 32 | Stored LP PDA (set on init, verified on calls) |

Minimum size: 96 bytes (percolator requires 320 bytes)

## Instructions

### Tag 0: Matcher Call (from Percolator CPI)

Executes passive matching logic. Requires LP PDA to be a signer and match the stored PDA.

#### Accounts

| Index | Name | Type | Description |
|-------|------|------|-------------|
| 0 | lp_pda | Signer | LP PDA (must match stored PDA) |
| 1 | matcher_ctx | Writable | Context account owned by this program |

#### Instruction Data (67 bytes)

| Offset | Field | Type | Description |
|--------|-------|------|-------------|
| 0 | tag | u8 | Always 0 |
| 1-9 | req_id | u64 | Request ID (echoed) |
| 9-11 | asset_index | u16 | Market asset index, echoed in the response |
| 11-19 | lp_account_id | u64 | LP account ID (echoed) |
| 19-27 | oracle_price_e6 | u64 | Oracle price (1e6 scaled) |
| 27-43 | req_size | i128 | Requested size (+buy/-sell) |
| 43-67 | reserved | [u8;24] | Must be zero |

#### Response (64 bytes at offset 0 in context account)

| Offset | Field | Type | Description |
|--------|-------|------|-------------|
| 0-4 | abi_version | u32 | Always 3 |
| 4-8 | flags | u32 | VALID=1, PARTIAL_OK=2, REJECTED=4 |
| 8-16 | exec_price_e6 | u64 | Execution price |
| 16-32 | exec_size | i128 | Executed size |
| 32-40 | req_id | u64 | Echo of req_id |
| 40-48 | lp_account_id | u64 | Echo of lp_account_id |
| 48-56 | oracle_price_e6 | u64 | Echo of oracle_price_e6 |
| 56-64 | asset_index | u64 | Echo of asset_index |

### Tag 3: Batched Matcher Call (atomic multi-leg, from Percolator CPI)

Fills `n` legs against this LP's inventory in a single CPI. The LP PDA signs once for the whole batch; each leg runs the same passive matching logic as Tag 0, with inventory carried across legs in request order. The `n` returns are emitted via `sol_set_return_data` (the context account's 64-byte return slot holds only one), so `n` ≤ 16 (16 × 64 = the 1024-byte return-data cap). ABI version is unchanged (3).

#### Accounts

| Index | Name | Type | Description |
|-------|------|------|-------------|
| 0 | lp_pda | Signer | LP PDA (must match stored PDA) |
| 1 | matcher_ctx | Writable | Context account owned by this program |

#### Instruction Data (18 + n×26 bytes)

| Offset | Field | Type | Description |
|--------|-------|------|-------------|
| 0 | tag | u8 | Always 3 |
| 1 | n | u8 | Leg count (1..=16) |
| 2-10 | req_id | u64 | Batch request ID (echoed on every leg) |
| 10-18 | lp_account_id | u64 | LP account ID (echoed on every leg) |
| 18… | legs[n] | — | `n` legs, 26 bytes each (below) |

Each leg (26 bytes):

| Offset | Field | Type | Description |
|--------|-------|------|-------------|
| 0-2 | asset_index | u16 | Market asset index |
| 2-10 | oracle_price_e6 | u64 | Oracle price (1e6 scaled) |
| 10-26 | req_size | i128 | Requested size (+buy/-sell) |

#### Response (n × 64 bytes via return data)

`set_return_data` carries `n` back-to-back 64-byte `MatcherReturn` records — same layout as the Tag 0 response — one per leg in request order. The caller reads them with `get_return_data` (not from the context account).

### Tag 2: Initialize

Stores the LP PDA in the context account. The context account must sign; the LP PDA does not (passive init: it is a Percolator-derived delegate that signs only inside Percolator's CPI). Can only be called once.

#### Accounts

| Index | Name | Type | Description |
|-------|------|------|-------------|
| 0 | lp_pda | - | LP PDA to store (no signature required) |
| 1 | matcher_ctx | Signer, Writable | Context account owned by this program |

#### Instruction Data (1 byte)

| Offset | Field | Type | Description |
|--------|-------|------|-------------|
| 0 | tag | u8 | Always 2 |

## Security

- **LP PDA binding**: The matcher verifies that the LP PDA signer matches the PDA stored during initialization
- **One-time init**: Context can only be initialized once (prevents rebinding)
- **Init authorization**: The context account signs its own initialization, so nobody else can initialize it first
- **Inventory bounds**: `max_inventory_abs` must fit in `i128`, and inventory updates fail instead of clamping on overflow
- **Program ownership**: Context account must be owned by this program

## License

Apache 2.0
