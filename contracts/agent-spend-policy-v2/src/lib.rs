#![no_std]

use soroban_sdk::{
    auth::Context, contract, contractclient, contractimpl, contracttype, Address, Env, IntoVal,
    Map, Symbol, TryFromVal, Val, Vec,
};

// ============================================================================
// SmartAccountPolicy trait (inlined to avoid SDK 22 dependency chain)
// Source: stellar-smart-account/contracts/smart-account-interfaces
// ============================================================================

#[contractclient(name = "SmartAccountPolicyClient")]
pub trait SmartAccountPolicy {
    fn on_add(env: &Env, source: Address);
    fn on_revoke(env: &Env, source: Address);
    fn is_authorized(env: &Env, source: Address, contexts: Vec<Context>) -> bool;
}

// ============================================================================
// Storage Keys
// ============================================================================

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    // ── Instance (contract config, lives forever) ──
    Admin,
    Paused,         // bool: kill switch — blocks all is_authorized calls
    UsdcAddr,
    RouterAddr,
    DailyLimitUsdc, // i128: max USDC-equivalent stroops per day

    // ── Persistent (admin-set price cache) ──
    // Map<Address, i128>: token contract → USDC stroops per 1e7 token units
    PriceMap,

    // ── Temporary (auto-expiring daily spend) ──
    // Each (signer, day_number) pair is a separate entry with ~48h TTL.
    // day_number = ledger_sequence / LEDGERS_PER_DAY
    // When the day rolls over, a new key is used and the old one expires.
    DailySpend(Address, u64),
}

// ============================================================================
// Constants
// ============================================================================

/// ~24 hours at 5-second ledger close times
const LEDGERS_PER_DAY: u64 = 17_280;

/// 7-decimal unit (1.0000000 in stroops)
const UNIT: i128 = 10_000_000;

/// TTL for temporary spend entries: ~48h of ledgers.
/// We use 2x the day length so entries survive through end-of-day boundary.
const SPEND_TTL_LEDGERS: u32 = 34_560;

/// TTL for persistent price map: ~30 days
const PRICE_TTL_LEDGERS: u32 = 518_400;

// ============================================================================
// Contract
// ============================================================================

#[contract]
pub struct AgentSpendPolicyV2;

#[contractimpl]
impl AgentSpendPolicyV2 {
    /// One-time initialization. Sets admin, USDC contract, optional router, and daily limit.
    ///
    /// - `daily_limit_usdc`: limit in USDC stroops (e.g. 500_0000000 = $500)
    /// - `router`: Soroswap router address for live price fallback. Pass the contract's
    ///   own address as a sentinel if no router is available.
    pub fn initialize(
        env: &Env,
        admin: Address,
        usdc: Address,
        router: Address,
        daily_limit_usdc: i128,
    ) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        admin.require_auth();

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::UsdcAddr, &usdc);
        env.storage().instance().set(&DataKey::RouterAddr, &router);
        env.storage()
            .instance()
            .set(&DataKey::DailyLimitUsdc, &daily_limit_usdc);
    }

    /// Admin: set cached price for a token.
    ///
    /// `usdc_per_unit` = how many USDC stroops you get for 1e7 (1.0) token units.
    /// Example: XLM at $0.15 → usdc_per_unit = 1_500_000
    pub fn set_price(env: &Env, token: Address, usdc_per_unit: i128) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();

        let mut prices: Map<Address, i128> = env
            .storage()
            .persistent()
            .get(&DataKey::PriceMap)
            .unwrap_or(Map::new(env));

        prices.set(token, usdc_per_unit);
        env.storage()
            .persistent()
            .set(&DataKey::PriceMap, &prices);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::PriceMap, PRICE_TTL_LEDGERS, PRICE_TTL_LEDGERS);
    }

    /// Admin: batch-set prices for multiple tokens at once.
    pub fn set_prices(env: &Env, tokens: Vec<Address>, prices_vec: Vec<i128>) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();

        assert!(tokens.len() == prices_vec.len(), "length mismatch");

        let mut prices: Map<Address, i128> = env
            .storage()
            .persistent()
            .get(&DataKey::PriceMap)
            .unwrap_or(Map::new(env));

        for i in 0..tokens.len() {
            prices.set(tokens.get(i).unwrap(), prices_vec.get(i).unwrap());
        }

        env.storage()
            .persistent()
            .set(&DataKey::PriceMap, &prices);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::PriceMap, PRICE_TTL_LEDGERS, PRICE_TTL_LEDGERS);
    }

    /// Admin: kill switch — immediately blocks ALL is_authorized calls.
    /// Pausing one policy instance only affects signers using that instance.
    pub fn pause(env: &Env) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
    }

    /// Admin: unpause — re-enables is_authorized checks.
    pub fn unpause(env: &Env) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
    }

    /// View: whether the policy is currently paused.
    pub fn is_paused(env: &Env) -> bool {
        env.storage().instance().get(&DataKey::Paused).unwrap_or(false)
    }

    /// Admin: update the daily limit (in USDC stroops).
    pub fn set_limit(env: &Env, new_limit: i128) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::DailyLimitUsdc, &new_limit);
    }

    /// Admin: update the router address.
    pub fn set_router(env: &Env, router: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::RouterAddr, &router);
    }

    /// View: USDC-equivalent spent today by a signer.
    pub fn spent_today(env: &Env, signer: Address) -> i128 {
        let day = current_day(env);
        let key = DataKey::DailySpend(signer, day);
        env.storage().temporary().get(&key).unwrap_or(0)
    }

    /// View: remaining USDC-equivalent budget for a signer today.
    pub fn remaining(env: &Env, signer: Address) -> i128 {
        let limit: i128 = env
            .storage()
            .instance()
            .get(&DataKey::DailyLimitUsdc)
            .unwrap_or(0);
        let spent = Self::spent_today(env, signer);
        if limit > spent {
            limit - spent
        } else {
            0
        }
    }

    /// View: current daily limit.
    pub fn daily_limit(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::DailyLimitUsdc)
            .unwrap_or(0)
    }

    /// View: cached price for a token (0 if not set).
    pub fn get_price(env: &Env, token: Address) -> i128 {
        let prices: Map<Address, i128> = env
            .storage()
            .persistent()
            .get(&DataKey::PriceMap)
            .unwrap_or(Map::new(env));
        prices.get(token).unwrap_or(0)
    }
}

// ============================================================================
// SmartAccountPolicy Implementation
// ============================================================================

#[contractimpl]
impl SmartAccountPolicy for AgentSpendPolicyV2 {
    fn on_add(_env: &Env, source: Address) {
        source.require_auth();
    }

    fn on_revoke(_env: &Env, source: Address) {
        source.require_auth();
    }

    fn is_authorized(env: &Env, source: Address, contexts: Vec<Context>) -> bool {
        source.require_auth();

        // Kill switch — admin can freeze all signers instantly
        if env.storage().instance().get::<_, bool>(&DataKey::Paused).unwrap_or(false) {
            return false;
        }

        // ================================================================
        // V2 CHANGE: Sum USDC value of ALL transfer AND approve calls.
        //
        // V1 bug: approve() was treated as a non-transfer op and passed
        // through unchecked. An agent signer could call:
        //   approve(malicious_contract, huge_amount)
        // Then the malicious contract drains via transfer_from() —
        // completely bypassing the daily spend limit.
        //
        // V2 fix: approve() amounts are counted against the daily limit
        // using the same USDC valuation logic as transfers. This prevents
        // drain-via-allowance attacks while still allowing legitimate
        // DEX interactions (within the daily budget).
        // ================================================================
        let usdc_value = sum_gated_usdc_value(env, &contexts);

        // No gated calls found → non-transfer/approve op → allow
        if usdc_value == 0 {
            return true;
        }

        // Negative means an unknown token was encountered → reject
        if usdc_value < 0 {
            return false;
        }

        // Daily limit check using temporary storage
        let limit: i128 = env
            .storage()
            .instance()
            .get(&DataKey::DailyLimitUsdc)
            .unwrap_or(0);
        let day = current_day(env);
        let key = DataKey::DailySpend(source.clone(), day);

        let spent: i128 = env.storage().temporary().get(&key).unwrap_or(0);

        if spent + usdc_value > limit {
            return false;
        }

        // Commit the spend
        env.storage()
            .temporary()
            .set(&key, &(spent + usdc_value));
        env.storage()
            .temporary()
            .extend_ttl(&key, SPEND_TTL_LEDGERS, SPEND_TTL_LEDGERS);

        true
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Current "day number" derived from ledger sequence.
fn current_day(env: &Env) -> u64 {
    u64::from(env.ledger().sequence()) / LEDGERS_PER_DAY
}

/// Sum USDC-equivalent value of ALL `transfer` AND `approve` calls in the
/// auth contexts.
///
/// V2 change: Both `transfer(from, to, amount)` and `approve(from, spender,
/// amount, expiration_ledger)` are gated. The amount is extracted from arg
/// index 2 in both cases (same position in both function signatures).
///
/// The V4 router batches multiple operations (e.g. transfer + fee_transfer)
/// into a single auth entry. We must count EVERY gated call, not just the first.
///
/// Returns:
///   > 0  — total USDC value of all gated calls
///   = 0  — no gated calls found (non-transfer/approve operation)
///   < 0  — at least one gated call has an unknown/un-priceable token (reject)
fn sum_gated_usdc_value(env: &Env, contexts: &Vec<Context>) -> i128 {
    let transfer_sym = Symbol::new(env, "transfer");
    let approve_sym = Symbol::new(env, "approve");
    let mut total: i128 = 0;

    for ctx in contexts.iter() {
        if let Context::Contract(c) = ctx {
            let is_transfer = c.fn_name == transfer_sym && c.args.len() >= 3;
            let is_approve = c.fn_name == approve_sym && c.args.len() >= 3;

            if is_transfer || is_approve {
                // Amount is at index 2 for both:
                //   transfer(from, to, amount)
                //   approve(from, spender, amount, expiration_ledger)
                if let Ok(amount) = i128::try_from_val(env, &c.args.get_unchecked(2)) {
                    if amount == 0 {
                        continue;
                    }
                    let value = to_usdc_value(env, &c.contract, amount);
                    if value <= 0 {
                        // Unknown token → entire batch is rejected
                        return -1;
                    }
                    total += value;
                }
            }
        }
    }
    total
}

/// Convert a token amount to USDC-equivalent stroops.
///
/// Resolution order:
/// 1. Token IS USDC → identity (free)
/// 2. Admin-set cached price → multiply (cheap, ~50K CPU)
/// 3. Soroswap router fallback → cross-contract call (~1-2M CPU)
/// 4. No price available → return -1 (caller should reject)
fn to_usdc_value(env: &Env, token: &Address, amount: i128) -> i128 {
    let usdc: Address = env.storage().instance().get(&DataKey::UsdcAddr).unwrap();

    // 1. Identity: token IS USDC
    if *token == usdc {
        return amount;
    }

    // 2. Cached admin-set price
    let prices: Map<Address, i128> = env
        .storage()
        .persistent()
        .get(&DataKey::PriceMap)
        .unwrap_or(Map::new(env));

    if let Some(price_per_unit) = prices.get(token.clone()) {
        // price_per_unit = USDC stroops per 1e7 (1.0) token units
        // value = amount * price / UNIT
        return amount * price_per_unit / UNIT;
    }

    // 3. Soroswap router fallback: router_get_amounts_out(amount, [token, usdc])
    let router: Address = match env.storage().instance().get(&DataKey::RouterAddr) {
        Some(r) => r,
        None => return -1,
    };

    // Don't call router if it's set to our own address (sentinel for "no router")
    if router == env.current_contract_address() {
        return -1;
    }

    // Build path: [token, usdc]
    let mut path = Vec::new(env);
    path.push_back(token.clone());
    path.push_back(usdc);

    let fn_name = Symbol::new(env, "router_get_amounts_out");

    // try_invoke_contract returns Result — if the pool doesn't exist, we get Err
    let args: Vec<Val> = Vec::from_array(
        env,
        [amount.into_val(env), path.into_val(env)],
    );

    let result = env.try_invoke_contract::<Vec<i128>, Val>(&router, &fn_name, args);

    match result {
        Ok(Ok(amounts)) => {
            // Last element = USDC output amount
            amounts.last().unwrap_or(-1)
        }
        _ => -1, // Router call failed (no pool, etc.) → reject
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        auth::ContractContext,
        testutils::{Address as _, Ledger},
        IntoVal,
    };

    // ── Test helpers ──

    fn setup_policy(env: &Env, daily_limit: i128) -> (Address, AgentSpendPolicyV2Client<'_>, Address) {
        let contract_id = env.register(AgentSpendPolicyV2, ());
        let client = AgentSpendPolicyV2Client::new(env, &contract_id);

        let admin = Address::generate(env);
        let usdc = Address::generate(env);

        // Use contract's own address as router sentinel (no router)
        client.initialize(&admin, &usdc, &contract_id, &daily_limit);

        (usdc, client, admin)
    }

    fn make_transfer_ctx(env: &Env, token: &Address, amount: i128) -> Context {
        Context::Contract(ContractContext {
            contract: token.clone(),
            fn_name: Symbol::new(env, "transfer"),
            args: (
                Address::generate(env), // from
                Address::generate(env), // to
                amount,
            )
                .into_val(env),
        })
    }

    fn make_approve_ctx(env: &Env, token: &Address, amount: i128) -> Context {
        Context::Contract(ContractContext {
            contract: token.clone(),
            fn_name: Symbol::new(env, "approve"),
            args: (
                Address::generate(env), // from
                Address::generate(env), // spender
                amount,
                1000u32,                // expiration_ledger
            )
                .into_val(env),
        })
    }

    fn make_contexts(env: &Env, ctx: Context) -> Vec<Context> {
        let mut v = Vec::new(env);
        v.push_back(ctx);
        v
    }

    // ════════════════════════════════════════════════════════════════════
    // V2 APPROVE GATING TESTS
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn test_approve_counted_against_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000); // $500

        let signer = Address::generate(&env);

        // Approve $200 USDC → should count against limit
        let ctx = make_approve_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
        assert_eq!(client.spent_today(&signer), 200_0000000);
        assert_eq!(client.remaining(&signer), 300_0000000);
    }

    #[test]
    fn test_approve_over_limit_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Approve $600 USDC → over limit → rejected
        let ctx = make_approve_ctx(&env, &usdc, 600_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));
        assert_eq!(client.spent_today(&signer), 0); // nothing recorded
    }

    #[test]
    fn test_approve_cumulative_with_transfer() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Transfer $200
        let ctx1 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));
        assert_eq!(client.spent_today(&signer), 200_0000000);

        // Approve $200 → cumulative $400, within limit
        let ctx2 = make_approve_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));
        assert_eq!(client.spent_today(&signer), 400_0000000);

        // Approve $200 more → $600 > $500 → rejected
        let ctx3 = make_approve_ctx(&env, &usdc, 200_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx3)));
        assert_eq!(client.spent_today(&signer), 400_0000000); // unchanged
    }

    #[test]
    fn test_approve_with_cached_price() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);
        // XLM at $0.15
        client.set_price(&xlm, &1_500_000);

        let signer = Address::generate(&env);

        // Approve 100 XLM ($15) → should pass and count
        let ctx = make_approve_ctx(&env, &xlm, 100_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
        assert_eq!(client.spent_today(&signer), 15_0000000); // $15
    }

    #[test]
    fn test_approve_unknown_token_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let unknown = Address::generate(&env);
        let signer = Address::generate(&env);

        // Approve unknown token → rejected (no price, no router)
        let ctx = make_approve_ctx(&env, &unknown, 1_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }

    #[test]
    fn test_approve_zero_amount_allowed() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Approve 0 → treated as no-op, allowed
        let ctx = make_approve_ctx(&env, &usdc, 0);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
        assert_eq!(client.spent_today(&signer), 0);
    }

    #[test]
    fn test_mixed_transfer_and_approve_in_batch() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Batch: transfer $100 + approve $200 = $300 total
        let mut v = Vec::new(&env);
        v.push_back(make_transfer_ctx(&env, &usdc, 100_0000000));
        v.push_back(make_approve_ctx(&env, &usdc, 200_0000000));

        assert!(client.is_authorized(&signer, &v));
        assert_eq!(client.spent_today(&signer), 300_0000000);
    }

    #[test]
    fn test_batch_approve_exceeds_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Batch: transfer $300 + approve $300 = $600 > $500 → reject
        let mut v = Vec::new(&env);
        v.push_back(make_transfer_ctx(&env, &usdc, 300_0000000));
        v.push_back(make_approve_ctx(&env, &usdc, 300_0000000));

        assert!(!client.is_authorized(&signer, &v));
        assert_eq!(client.spent_today(&signer), 0); // nothing recorded
    }

    #[test]
    fn test_approve_paused_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        client.pause();

        let signer = Address::generate(&env);
        let ctx = make_approve_ctx(&env, &usdc, 100_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }

    #[test]
    fn test_approve_day_rollover_resets() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Day 0: approve $400
        let ctx1 = make_approve_ctx(&env, &usdc, 400_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));
        assert_eq!(client.spent_today(&signer), 400_0000000);

        // Day 1: reset
        env.ledger().set_sequence_number(17_280);
        assert_eq!(client.spent_today(&signer), 0);

        // Approve $400 again → should work
        let ctx2 = make_approve_ctx(&env, &usdc, 400_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));
    }

    // ════════════════════════════════════════════════════════════════════
    // V1 PARITY TESTS (ensuring existing behavior is preserved)
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn test_usdc_transfer_within_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);
        let ctx = make_transfer_ctx(&env, &usdc, 100_0000000);
        let contexts = make_contexts(&env, ctx);

        assert!(client.is_authorized(&signer, &contexts));
        assert_eq!(client.spent_today(&signer), 100_0000000);
        assert_eq!(client.remaining(&signer), 400_0000000);
    }

    #[test]
    fn test_usdc_transfer_over_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);
        let ctx = make_transfer_ctx(&env, &usdc, 600_0000000);
        let contexts = make_contexts(&env, ctx);

        assert!(!client.is_authorized(&signer, &contexts));
        assert_eq!(client.spent_today(&signer), 0);
    }

    #[test]
    fn test_usdc_cumulative_spend() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        let ctx1 = make_transfer_ctx(&env, &usdc, 300_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));
        assert_eq!(client.spent_today(&signer), 300_0000000);

        let ctx2 = make_transfer_ctx(&env, &usdc, 250_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx2)));
        assert_eq!(client.spent_today(&signer), 300_0000000);

        let ctx3 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx3)));
        assert_eq!(client.spent_today(&signer), 500_0000000);

        let ctx4 = make_transfer_ctx(&env, &usdc, 1);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx4)));
    }

    #[test]
    fn test_xlm_with_cached_price() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);
        client.set_price(&xlm, &1_500_000);

        let signer = Address::generate(&env);
        let ctx = make_transfer_ctx(&env, &xlm, 100_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
        assert_eq!(client.spent_today(&signer), 15_0000000);
    }

    #[test]
    fn test_xlm_large_transfer_hits_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);
        client.set_price(&xlm, &1_500_000);

        let signer = Address::generate(&env);
        let ctx = make_transfer_ctx(&env, &xlm, 5000_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }

    #[test]
    fn test_batch_set_prices() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);
        let aqua = Address::generate(&env);

        let mut tokens = Vec::new(&env);
        tokens.push_back(xlm.clone());
        tokens.push_back(aqua.clone());

        let mut prices = Vec::new(&env);
        prices.push_back(1_500_000i128);
        prices.push_back(50_000i128);

        client.set_prices(&tokens, &prices);

        assert_eq!(client.get_price(&xlm), 1_500_000);
        assert_eq!(client.get_price(&aqua), 50_000);
    }

    #[test]
    fn test_non_gated_op_always_allowed() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // "swap" operation — neither transfer nor approve
        let ctx = Context::Contract(ContractContext {
            contract: Address::generate(&env),
            fn_name: Symbol::new(&env, "swap"),
            args: (Address::generate(&env), 1_000_000_000i128).into_val(&env),
        });
        let contexts = make_contexts(&env, ctx);

        assert!(client.is_authorized(&signer, &contexts));
        assert_eq!(client.spent_today(&signer), 0);
    }

    #[test]
    fn test_unknown_token_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let unknown_token = Address::generate(&env);
        let signer = Address::generate(&env);

        let ctx = make_transfer_ctx(&env, &unknown_token, 1_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }

    #[test]
    fn test_per_wallet_independent_limits() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let wallet_a = Address::generate(&env);
        let wallet_b = Address::generate(&env);

        let ctx_a = make_transfer_ctx(&env, &usdc, 400_0000000);
        assert!(client.is_authorized(&wallet_a, &make_contexts(&env, ctx_a)));

        let ctx_b = make_transfer_ctx(&env, &usdc, 450_0000000);
        assert!(client.is_authorized(&wallet_b, &make_contexts(&env, ctx_b)));

        let ctx_a2 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(!client.is_authorized(&wallet_a, &make_contexts(&env, ctx_a2)));

        let ctx_b2 = make_transfer_ctx(&env, &usdc, 50_0000000);
        assert!(client.is_authorized(&wallet_b, &make_contexts(&env, ctx_b2)));
    }

    #[test]
    fn test_day_rollover_resets_spend() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        let ctx1 = make_transfer_ctx(&env, &usdc, 400_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));

        env.ledger().set_sequence_number(17_280);

        assert_eq!(client.spent_today(&signer), 0);
        let ctx2 = make_transfer_ctx(&env, &usdc, 400_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));
    }

    #[test]
    fn test_same_day_different_ledgers() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        env.ledger().set_sequence_number(100);
        let ctx1 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));

        env.ledger().set_sequence_number(5000);
        let ctx2 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));
        assert_eq!(client.spent_today(&signer), 400_0000000);

        env.ledger().set_sequence_number(17000);
        let ctx3 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx3)));
    }

    #[test]
    fn test_set_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        assert_eq!(client.daily_limit(), 500_0000000);

        client.set_limit(&100_0000000);
        assert_eq!(client.daily_limit(), 100_0000000);

        let signer = Address::generate(&env);

        let ctx = make_transfer_ctx(&env, &usdc, 150_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));

        let ctx2 = make_transfer_ctx(&env, &usdc, 50_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));
    }

    #[test]
    fn test_price_update_affects_valuation() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 100_0000000);

        let xlm = Address::generate(&env);
        let signer = Address::generate(&env);

        client.set_price(&xlm, &1_000_000);
        let ctx1 = make_transfer_ctx(&env, &xlm, 100_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));
        assert_eq!(client.spent_today(&signer), 10_0000000);

        client.set_price(&xlm, &10_000_000);
        let ctx2 = make_transfer_ctx(&env, &xlm, 100_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx2)));

        let ctx3 = make_transfer_ctx(&env, &xlm, 50_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx3)));
        assert_eq!(client.spent_today(&signer), 60_0000000);
    }

    #[test]
    fn test_view_functions() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);
        let xlm = Address::generate(&env);

        assert_eq!(client.daily_limit(), 500_0000000);
        assert_eq!(client.spent_today(&signer), 0);
        assert_eq!(client.remaining(&signer), 500_0000000);
        assert_eq!(client.get_price(&xlm), 0);

        client.set_price(&xlm, &1_500_000);
        assert_eq!(client.get_price(&xlm), 1_500_000);

        let ctx = make_transfer_ctx(&env, &usdc, 123_0000000);
        client.is_authorized(&signer, &make_contexts(&env, ctx));
        assert_eq!(client.spent_today(&signer), 123_0000000);
        assert_eq!(client.remaining(&signer), 377_0000000);
    }

    #[test]
    fn test_exact_limit_amount() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        let ctx = make_transfer_ctx(&env, &usdc, 500_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
        assert_eq!(client.remaining(&signer), 0);

        let ctx2 = make_transfer_ctx(&env, &usdc, 1);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx2)));
    }

    #[test]
    fn test_zero_amount_transfer() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        let ctx = make_transfer_ctx(&env, &usdc, 0);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }

    #[test]
    #[should_panic(expected = "already initialized")]
    fn test_double_init_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AgentSpendPolicyV2, ());
        let client = AgentSpendPolicyV2Client::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let usdc = Address::generate(&env);

        client.initialize(&admin, &usdc, &contract_id, &500_0000000);
        client.initialize(&admin, &usdc, &contract_id, &500_0000000);
    }

    #[test]
    fn test_multi_day_spend_tracking() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 200_0000000);

        let signer = Address::generate(&env);

        let ctx = make_transfer_ctx(&env, &usdc, 150_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));

        env.ledger().set_sequence_number(17_280);
        let ctx2 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));

        env.ledger().set_sequence_number(34_560);
        assert_eq!(client.remaining(&signer), 200_0000000);
    }

    #[test]
    fn test_pause_blocks_all_transfers() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        let ctx1 = make_transfer_ctx(&env, &usdc, 100_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));

        client.pause();
        assert!(client.is_paused());

        let ctx2 = make_transfer_ctx(&env, &usdc, 50_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx2)));

        client.unpause();
        assert!(!client.is_paused());

        let ctx3 = make_transfer_ctx(&env, &usdc, 50_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx3)));

        assert_eq!(client.spent_today(&signer), 150_0000000);
    }

    #[test]
    fn test_pause_blocks_approve_too() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        client.pause();

        let signer = Address::generate(&env);
        let ctx = make_approve_ctx(&env, &usdc, 100_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }

    // ── V4 Router multi-transfer tests ──

    fn make_router_contexts(
        env: &Env,
        router: &Address,
        transfers: &[(Address, i128)],
    ) -> Vec<Context> {
        let mut v = Vec::new(env);

        v.push_back(Context::Contract(ContractContext {
            contract: router.clone(),
            fn_name: Symbol::new(env, "exec"),
            args: (Address::generate(env),).into_val(env),
        }));

        for (token, amount) in transfers {
            v.push_back(Context::Contract(ContractContext {
                contract: token.clone(),
                fn_name: Symbol::new(env, "transfer"),
                args: (
                    Address::generate(env),
                    Address::generate(env),
                    *amount,
                )
                    .into_val(env),
            }));
        }
        v
    }

    #[test]
    fn test_v4_router_transfer_plus_fee_both_counted() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);
        let router = Address::generate(&env);

        client.set_price(&xlm, &1_500_000);

        let wallet = Address::generate(&env);

        let contexts = make_router_contexts(&env, &router, &[
            (xlm.clone(), 100_0000000),
            (xlm.clone(), 3000000),
        ]);

        assert!(client.is_authorized(&wallet, &contexts));
        assert_eq!(client.spent_today(&wallet), 150_450000);
    }

    #[test]
    fn test_v4_router_multi_transfer_hits_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 100_0000000);

        let xlm = Address::generate(&env);
        let router = Address::generate(&env);

        client.set_price(&xlm, &1_500_000);

        let wallet = Address::generate(&env);

        let contexts = make_router_contexts(&env, &router, &[
            (xlm.clone(), 500_0000000),
            (xlm.clone(), 500_0000000),
        ]);

        assert!(!client.is_authorized(&wallet, &contexts));
        assert_eq!(client.spent_today(&wallet), 0);
    }

    #[test]
    fn test_v4_router_mixed_tokens_summed() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);
        let router = Address::generate(&env);

        client.set_price(&xlm, &1_500_000);

        let wallet = Address::generate(&env);

        let contexts = make_router_contexts(&env, &router, &[
            (xlm.clone(), 100_0000000),
            (usdc.clone(), 2_0000000),
        ]);

        assert!(client.is_authorized(&wallet, &contexts));
        assert_eq!(client.spent_today(&wallet), 17_0000000);
    }

    #[test]
    fn test_v4_router_unknown_token_in_batch_rejects_all() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);
        let unknown = Address::generate(&env);
        let router = Address::generate(&env);

        client.set_price(&xlm, &1_500_000);

        let wallet = Address::generate(&env);

        let contexts = make_router_contexts(&env, &router, &[
            (xlm.clone(), 10_0000000),
            (unknown.clone(), 10_0000000),
        ]);

        assert!(!client.is_authorized(&wallet, &contexts));
        assert_eq!(client.spent_today(&wallet), 0);
    }

    #[test]
    fn test_router_exec_with_approve_counted() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let router = Address::generate(&env);
        let wallet = Address::generate(&env);

        // Router exec with approve — V2 counts this against limit
        let mut v = Vec::new(&env);
        v.push_back(Context::Contract(ContractContext {
            contract: router.clone(),
            fn_name: Symbol::new(&env, "exec"),
            args: (Address::generate(&env),).into_val(&env),
        }));
        v.push_back(Context::Contract(ContractContext {
            contract: usdc.clone(),
            fn_name: Symbol::new(&env, "approve"),
            args: (
                Address::generate(&env), // from
                Address::generate(&env), // spender
                100_0000000i128,          // amount
                1000u32,                  // expiration_ledger
            )
                .into_val(&env),
        }));

        assert!(client.is_authorized(&wallet, &v));
        assert_eq!(client.spent_today(&wallet), 100_0000000); // $100 counted
    }

    #[test]
    fn test_non_gated_ops_still_pass() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let router = Address::generate(&env);
        let wallet = Address::generate(&env);

        // Router exec with only swap — no transfer or approve
        let mut v = Vec::new(&env);
        v.push_back(Context::Contract(ContractContext {
            contract: router.clone(),
            fn_name: Symbol::new(&env, "exec"),
            args: (Address::generate(&env),).into_val(&env),
        }));
        v.push_back(Context::Contract(ContractContext {
            contract: Address::generate(&env),
            fn_name: Symbol::new(&env, "swap"),
            args: (Address::generate(&env), 1_000_000_000i128).into_val(&env),
        }));

        assert!(client.is_authorized(&wallet, &v));
        assert_eq!(client.spent_today(&wallet), 0);
    }
}
