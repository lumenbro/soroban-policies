# Soroban Policies

Soroban smart contract policies for [LumenBro](https://lumenbro.com) smart account signers. Built on the [Stellar Smart Account](https://github.com/AhaLabs/stellar-smart-account) `SmartAccountPolicy` trait.

## Contracts

### Agent Spend Policy (v1)

USD-equivalent daily spend limit enforced on-chain for agent signers (bot signers, session signers, MCP agents). Each policy instance is initialized with a configurable daily limit and converts token amounts to USDC value via:

1. **Identity** — USDC transfers are valued at face value
2. **Admin-cached prices** — fast path (~50K CPU), set via `set_price` / `set_prices`
3. **Soroswap router fallback** — on-chain DEX quote (~1-2M CPU)

**Gates:** `transfer()` only. Non-transfer operations (`approve`, `swap`, etc.) pass through unchecked.

**Use for:** Retail wallet signers operating within the LumenBro platform (internal codebase controls contract interactions — no approve bypass risk).

### Agent Spend Policy v2

Same daily spend limit engine as v1 with one critical security addition: **gates both `transfer()` AND `approve()` calls** against the daily limit.

**Why v2 exists:** v1 only inspects `transfer()` calls in `is_authorized()`. An agent signer could call `approve(malicious_contract, huge_amount)` on any token, then the malicious contract drains the wallet via `transfer_from()` — completely bypassing the daily spend limit. This is acceptable for retail signers (platform controls all contract interactions) but dangerous for autonomous agent signers operating in the wild.

**v2 fix:** `approve()` amounts are valued in USDC (same 3-tier price resolution) and counted against the daily limit. This prevents drain-via-allowance attacks while still allowing legitimate DEX interactions within the daily budget.

**Use for:** Agent operator portal signers (`agents.lumenbro.com`) — autonomous agents that may interact with arbitrary contracts.

**Drop-in compatible:** Same 15 exported functions, same `initialize()` parameters, same storage model. Only behavioral change: `approve()` is now gated.

#### Features (both v1 and v2)

- **Configurable daily limit** — set at `initialize()`, changeable via `set_limit()`
- **Kill switch** — `pause()` / `unpause()` instantly blocks all `is_authorized()` calls
- **Per-wallet isolation** — each wallet gets independent daily spend tracking via Temporary storage (zero contention, auto-expiring, no archival)
- **Tiered deployment** — same WASM, multiple instances with different limits ($50, $500, $2,000/day)
- **Network-agnostic WASM** — all addresses (USDC, router, admin) injected at `initialize()` time

#### Storage Model

| Layer | Keys | Lifetime |
|-------|------|----------|
| **Instance** | Admin, Paused, UsdcAddr, RouterAddr, DailyLimitUsdc | Forever (lives with contract) |
| **Persistent** | PriceMap (token → USDC price) | ~30 days TTL, extended on write |
| **Temporary** | DailySpend(wallet, day_number) | ~48h TTL, auto-expires, no archival |

#### Public Functions

| Function | Access | Description |
|----------|--------|-------------|
| `initialize(admin, usdc, router, daily_limit_usdc)` | Once | Set up the policy instance |
| `pause()` | Admin | Kill switch — block all operations |
| `unpause()` | Admin | Resume normal operation |
| `set_limit(new_limit)` | Admin | Change daily limit (USDC stroops) |
| `set_price(token, usdc_per_unit)` | Admin | Cache price for a token |
| `set_prices(tokens, prices)` | Admin | Batch-set prices |
| `set_router(router)` | Admin | Update Soroswap router address |
| `is_paused()` | Public | Check if paused |
| `daily_limit()` | Public | Current daily limit |
| `spent_today(wallet)` | Public | USDC spent today by a wallet |
| `remaining(wallet)` | Public | Remaining budget for a wallet today |
| `get_price(token)` | Public | Cached price for a token |

#### SmartAccountPolicy Trait

| Function | Description |
|----------|-------------|
| `on_add(source)` | Called when policy is attached to a signer |
| `on_revoke(source)` | Called when policy is detached from a signer |
| `is_authorized(source, contexts)` | Enforces daily spend limit on transfers (v1) or transfers + approvals (v2) |

#### v1 vs v2 Behavior Comparison

| Operation | v1 | v2 |
|-----------|----|----|
| `transfer(from, to, amount)` | Gated (counts against limit) | Gated (counts against limit) |
| `approve(from, spender, amount, expiry)` | **Passthrough (unchecked)** | **Gated (counts against limit)** |
| `swap(...)`, `exec(...)`, etc. | Passthrough | Passthrough |
| Paused state | Blocks everything | Blocks everything |
| Unknown token in batch | Rejects entire batch | Rejects entire batch |

## Verified Builds

This repository uses [stellar-expert/soroban-build-workflow](https://github.com/stellar-expert/soroban-build-workflow) for reproducible, verified builds. Each tagged release (`v*`) triggers a GitHub Actions workflow that:

1. Compiles the contract with a pinned Rust toolchain
2. Generates an optimized WASM artifact with SHA256 hash
3. Creates a GitHub Release with build attestation
4. Enables [Stellar Expert contract validation](https://stellar.expert/explorer/public/contract/validation)

**Deploy from release artifacts** (not local builds) to maintain the trust chain.

## Build Locally

```bash
# Prerequisites: Rust + stellar-cli
rustup target add wasm32-unknown-unknown

# Build all contracts
stellar contract build

# Build specific contract
cd contracts/agent-spend-policy      # v1
cd contracts/agent-spend-policy-v2   # v2
make build

# Test
cargo test -p agent-spend-policy      # 20 tests (v1)
cargo test -p agent-spend-policy-v2   # 36 tests (v2)
```

## Deploy

```bash
# Upload WASM (once per network)
stellar contract deploy --wasm target/wasm32v1-none/release/agent_spend_policy_v2.wasm \
  --source <deployer> --network <testnet|mainnet>

# Initialize instance ($500/day limit)
stellar contract invoke --id <contract-id> --fn initialize -- \
  --admin <admin-address> \
  --usdc <usdc-sac-address> \
  --router <soroswap-router-address> \
  --daily_limit_usdc 5000000000

# Set token prices
stellar contract invoke --id <contract-id> --fn set_price -- \
  --token <xlm-sac-address> \
  --usdc_per_unit 1500000  # $0.15
```

## License

MIT
