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
pub struct AgentSpendPolicy;

#[contractimpl]
impl AgentSpendPolicy {
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
impl SmartAccountPolicy for AgentSpendPolicy {
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

        // Extract token + amount from transfer context
        let (token, amount) = extract_transfer(env, &contexts);
        if amount == 0 {
            // Non-transfer operation → allow (swap auth, approve, etc.)
            return true;
        }

        // Convert to USDC equivalent
        let usdc_value = to_usdc_value(env, &token, amount);

        // Can't price it → reject (unknown value = unsafe)
        if usdc_value <= 0 {
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

/// Extract (token_contract_address, amount) from transfer contexts.
///
/// SAC `transfer(from, to, amount)` — the `contract` field in ContractContext
/// is the token contract itself.
fn extract_transfer(env: &Env, contexts: &Vec<Context>) -> (Address, i128) {
    for ctx in contexts.iter() {
        if let Context::Contract(c) = ctx {
            let fn_name = Symbol::new(env, "transfer");
            if c.fn_name == fn_name && c.args.len() >= 3 {
                if let Ok(amount) = i128::try_from_val(env, &c.args.get_unchecked(2)) {
                    // c.contract = the token being transferred
                    return (c.contract.clone(), amount);
                }
            }
        }
    }
    // No transfer found — return dummy with 0 amount
    (env.current_contract_address(), 0)
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

    fn setup_policy(env: &Env, daily_limit: i128) -> (Address, AgentSpendPolicyClient<'_>, Address) {
        let contract_id = env.register(AgentSpendPolicy, ());
        let client = AgentSpendPolicyClient::new(env, &contract_id);

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

    fn make_contexts(env: &Env, ctx: Context) -> Vec<Context> {
        let mut v = Vec::new(env);
        v.push_back(ctx);
        v
    }

    // ── USDC identity tests ──

    #[test]
    fn test_usdc_transfer_within_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000); // $500

        let signer = Address::generate(&env);
        let ctx = make_transfer_ctx(&env, &usdc, 100_0000000); // $100 USDC
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
        let ctx = make_transfer_ctx(&env, &usdc, 600_0000000); // $600 USDC
        let contexts = make_contexts(&env, ctx);

        assert!(!client.is_authorized(&signer, &contexts));
        // Spend should NOT have been recorded
        assert_eq!(client.spent_today(&signer), 0);
    }

    #[test]
    fn test_usdc_cumulative_spend() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // $300 → should pass
        let ctx1 = make_transfer_ctx(&env, &usdc, 300_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));
        assert_eq!(client.spent_today(&signer), 300_0000000);

        // $250 → should fail ($300 + $250 = $550 > $500)
        let ctx2 = make_transfer_ctx(&env, &usdc, 250_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx2)));
        assert_eq!(client.spent_today(&signer), 300_0000000); // unchanged

        // $200 → should pass ($300 + $200 = $500 = limit)
        let ctx3 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx3)));
        assert_eq!(client.spent_today(&signer), 500_0000000);

        // $1 → should fail (at limit)
        let ctx4 = make_transfer_ctx(&env, &usdc, 1);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx4)));
    }

    // ── Cached price tests ──

    #[test]
    fn test_xlm_with_cached_price() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);

        // Set XLM price: $0.15 → 1_500_000 USDC stroops per 1e7 XLM stroops
        client.set_price(&xlm, &1_500_000);

        let signer = Address::generate(&env);

        // Transfer 100 XLM (100_0000000 stroops)
        // USDC value = 100_0000000 * 1_500_000 / 10_000_000 = 15_0000000 ($15)
        let ctx = make_transfer_ctx(&env, &xlm, 100_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
        assert_eq!(client.spent_today(&signer), 15_0000000); // $15
    }

    #[test]
    fn test_xlm_large_transfer_hits_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let xlm = Address::generate(&env);
        // XLM at $0.15
        client.set_price(&xlm, &1_500_000);

        let signer = Address::generate(&env);

        // Transfer 5000 XLM → $750 USDC equivalent → over $500 limit
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
        prices.push_back(1_500_000i128); // XLM $0.15
        prices.push_back(50_000i128); // AQUA $0.005

        client.set_prices(&tokens, &prices);

        assert_eq!(client.get_price(&xlm), 1_500_000);
        assert_eq!(client.get_price(&aqua), 50_000);
    }

    // ── Non-transfer passthrough ──

    #[test]
    fn test_non_transfer_always_allowed() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // "approve" operation — not a transfer
        let ctx = Context::Contract(ContractContext {
            contract: Address::generate(&env),
            fn_name: Symbol::new(&env, "approve"),
            args: (Address::generate(&env), 1_000_000_000i128).into_val(&env),
        });
        let contexts = make_contexts(&env, ctx);

        assert!(client.is_authorized(&signer, &contexts));
        assert_eq!(client.spent_today(&signer), 0); // no spend recorded
    }

    // ── Unknown token rejection ──

    #[test]
    fn test_unknown_token_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let unknown_token = Address::generate(&env);
        let signer = Address::generate(&env);

        // No price set, no router → should reject
        let ctx = make_transfer_ctx(&env, &unknown_token, 1_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }

    // ── Per-wallet independence ──
    //
    // NOTE: `source` in is_authorized() is the WALLET C-address, NOT the
    // individual signer's G-address.  The SmartAccountPolicy trait does not
    // pass the signer identity — __check_auth always passes the wallet's own
    // address.  Therefore the daily limit is PER-WALLET, not per-signer.
    //
    // Multiple signers on the SAME wallet share one $500 budget.
    // Multiple WALLETS using the same policy instance get independent budgets.
    //
    // For per-signer limits, deploy separate policy instances (same WASM,
    // different contract addresses) and assign each signer its own instance.

    #[test]
    fn test_per_wallet_independent_limits() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        // These represent DIFFERENT WALLETS (different source addresses),
        // not different signers on the same wallet.
        let wallet_a = Address::generate(&env);
        let wallet_b = Address::generate(&env);

        // Wallet A: spend $400
        let ctx_a = make_transfer_ctx(&env, &usdc, 400_0000000);
        assert!(client.is_authorized(&wallet_a, &make_contexts(&env, ctx_a)));

        // Wallet B: spend $450 → should pass (independent limit per wallet)
        let ctx_b = make_transfer_ctx(&env, &usdc, 450_0000000);
        assert!(client.is_authorized(&wallet_b, &make_contexts(&env, ctx_b)));

        // Wallet A: try $200 more → should fail ($400 + $200 = $600 > $500)
        let ctx_a2 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(!client.is_authorized(&wallet_a, &make_contexts(&env, ctx_a2)));

        // Wallet B: $50 more → should pass ($450 + $50 = $500 = limit)
        let ctx_b2 = make_transfer_ctx(&env, &usdc, 50_0000000);
        assert!(client.is_authorized(&wallet_b, &make_contexts(&env, ctx_b2)));
    }

    // ── Day rollover via temporary storage ──

    #[test]
    fn test_day_rollover_resets_spend() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Day 0 (default ledger = 0): spend $400
        let ctx1 = make_transfer_ctx(&env, &usdc, 400_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));
        assert_eq!(client.spent_today(&signer), 400_0000000);

        // Advance to next day (LEDGERS_PER_DAY = 17280)
        env.ledger().set_sequence_number(17_280);

        // Spend is reset — $400 should work again
        assert_eq!(client.spent_today(&signer), 0);
        let ctx2 = make_transfer_ctx(&env, &usdc, 400_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));
        assert_eq!(client.spent_today(&signer), 400_0000000);
    }

    #[test]
    fn test_same_day_different_ledgers() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Ledger 100: spend $200
        env.ledger().set_sequence_number(100);
        let ctx1 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));

        // Ledger 5000 (same day, 5000 / 17280 = 0): spend $200 more
        env.ledger().set_sequence_number(5000);
        let ctx2 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));
        assert_eq!(client.spent_today(&signer), 400_0000000);

        // Ledger 17000 (still day 0): $200 more → over limit
        env.ledger().set_sequence_number(17000);
        let ctx3 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx3)));
    }

    // ── Admin functions ──

    #[test]
    fn test_set_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        assert_eq!(client.daily_limit(), 500_0000000);

        // Lower limit to $100
        client.set_limit(&100_0000000);
        assert_eq!(client.daily_limit(), 100_0000000);

        let signer = Address::generate(&env);

        // $150 should now fail
        let ctx = make_transfer_ctx(&env, &usdc, 150_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));

        // $50 should pass
        let ctx2 = make_transfer_ctx(&env, &usdc, 50_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));
    }

    #[test]
    fn test_price_update_affects_valuation() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 100_0000000); // $100 limit

        let xlm = Address::generate(&env);
        let signer = Address::generate(&env);

        // XLM at $0.10 → 100 XLM = $10 → passes
        client.set_price(&xlm, &1_000_000);
        let ctx1 = make_transfer_ctx(&env, &xlm, 100_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));
        assert_eq!(client.spent_today(&signer), 10_0000000); // $10

        // Update XLM to $1.00 → 100 XLM = $100 → would put us at $110 > $100
        client.set_price(&xlm, &10_000_000);
        let ctx2 = make_transfer_ctx(&env, &xlm, 100_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx2)));

        // But 50 XLM = $50 → $10 + $50 = $60 → passes
        let ctx3 = make_transfer_ctx(&env, &xlm, 50_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx3)));
        assert_eq!(client.spent_today(&signer), 60_0000000); // $60
    }

    // ── View functions ──

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
        assert_eq!(client.get_price(&xlm), 0); // not set

        client.set_price(&xlm, &1_500_000);
        assert_eq!(client.get_price(&xlm), 1_500_000);

        // After a spend
        let ctx = make_transfer_ctx(&env, &usdc, 123_0000000);
        client.is_authorized(&signer, &make_contexts(&env, ctx));
        assert_eq!(client.spent_today(&signer), 123_0000000);
        assert_eq!(client.remaining(&signer), 377_0000000);
    }

    // ── Edge cases ──

    #[test]
    fn test_exact_limit_amount() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Exactly $500 → should pass
        let ctx = make_transfer_ctx(&env, &usdc, 500_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
        assert_eq!(client.remaining(&signer), 0);

        // $0.0000001 more → should fail
        let ctx2 = make_transfer_ctx(&env, &usdc, 1);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx2)));
    }

    #[test]
    fn test_zero_amount_transfer() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // 0-amount transfer → treated as non-transfer, allowed
        let ctx = make_transfer_ctx(&env, &usdc, 0);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }

    #[test]
    #[should_panic(expected = "already initialized")]
    fn test_double_init_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AgentSpendPolicy, ());
        let client = AgentSpendPolicyClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let usdc = Address::generate(&env);

        client.initialize(&admin, &usdc, &contract_id, &500_0000000);
        client.initialize(&admin, &usdc, &contract_id, &500_0000000); // should panic
    }

    // ── Multi-day tracking ──

    #[test]
    fn test_multi_day_spend_tracking() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 200_0000000); // $200/day

        let signer = Address::generate(&env);

        // Day 0: spend $150
        let ctx = make_transfer_ctx(&env, &usdc, 150_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx)));

        // Day 1: full $200 budget available
        env.ledger().set_sequence_number(17_280);
        let ctx2 = make_transfer_ctx(&env, &usdc, 200_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx2)));

        // Day 2: full $200 again
        env.ledger().set_sequence_number(34_560);
        assert_eq!(client.remaining(&signer), 200_0000000);
    }

    // ── Kill switch (pause/unpause) ──

    #[test]
    fn test_pause_blocks_all_transfers() {
        let env = Env::default();
        env.mock_all_auths();
        let (usdc, client, _admin) = setup_policy(&env, 500_0000000);

        let signer = Address::generate(&env);

        // Normal transfer works
        let ctx1 = make_transfer_ctx(&env, &usdc, 100_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx1)));

        // Pause
        assert!(!client.is_paused());
        client.pause();
        assert!(client.is_paused());

        // Transfer now blocked
        let ctx2 = make_transfer_ctx(&env, &usdc, 50_0000000);
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx2)));

        // Unpause
        client.unpause();
        assert!(!client.is_paused());

        // Transfer works again
        let ctx3 = make_transfer_ctx(&env, &usdc, 50_0000000);
        assert!(client.is_authorized(&signer, &make_contexts(&env, ctx3)));

        // Spend tracking: $100 (before pause) + $50 (after unpause) = $150
        // The $50 attempted during pause was NOT recorded
        assert_eq!(client.spent_today(&signer), 150_0000000);
    }

    #[test]
    fn test_pause_blocks_non_transfer_too() {
        let env = Env::default();
        env.mock_all_auths();
        let (_usdc, client, _admin) = setup_policy(&env, 500_0000000);

        client.pause();

        let signer = Address::generate(&env);

        // Even non-transfer ops (approve, etc.) are blocked when paused
        let ctx = Context::Contract(ContractContext {
            contract: Address::generate(&env),
            fn_name: Symbol::new(&env, "approve"),
            args: (Address::generate(&env), 1_000_000_000i128).into_val(&env),
        });
        assert!(!client.is_authorized(&signer, &make_contexts(&env, ctx)));
    }
}
