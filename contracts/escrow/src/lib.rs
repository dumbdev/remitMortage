#![no_std]

mod errors;
mod token_utils;
mod types;

use crate::errors::EscrowError;
use crate::token_utils::get_token_client;
use crate::types::{BorrowerRecord, DataKey, EscrowConfig};
use soroban_sdk::{contract, contractimpl, Address, Env};

const INSTANCE_BUMP_AMOUNT: u32 = 518_400; // ~30 days
const INSTANCE_LIFETIME_THRESHOLD: u32 = 129_600; // ~7.5 days

/// Escrow Contract
///
/// Holds borrower contributions toward a 30% down-payment savings target.
/// Accepts USDC deposits, tracks individual balances, and releases funds
/// once the savings target is met — or refunds the borrower on early withdrawal.
#[contract]
pub struct EscrowContract;

/// Internal helpers.
impl EscrowContract {
    /// Read the contract config or panic if not initialized.
    fn get_config(env: &Env) -> Result<EscrowConfig, EscrowError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(EscrowError::NotInitialized)
    }

    /// Read a borrower's record, returning a default if none exists.
    fn get_borrower(env: &Env, borrower: &Address) -> BorrowerRecord {
        env.storage()
            .persistent()
            .get(&DataKey::Borrower(borrower.clone()))
            .unwrap_or(BorrowerRecord {
                deposited: 0,
                start_ledger: 0,
                released: false,
                withdrawn: false,
            })
    }

    /// Write a borrower's record to persistent storage.
    fn set_borrower(env: &Env, borrower: &Address, record: &BorrowerRecord) {
        env.storage()
            .persistent()
            .set(&DataKey::Borrower(borrower.clone()), record);
    }

    /// Read the total pooled balance.
    fn get_total_pooled(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalPooled)
            .unwrap_or(0i128)
    }
}

#[contractimpl]
impl EscrowContract {
    /// Initialize the escrow contract with configuration parameters.
    ///
    /// # Arguments
    /// - `admin` — The address authorized to release funds and manage the contract.
    /// - `token` — The USDC token contract address.
    /// - `savings_target` — The target amount each borrower must save (in token units).
    /// - `max_duration_ledgers` — Maximum number of ledgers for the savings period.
    /// - `early_withdrawal_penalty_bps` — Penalty for early withdrawal in basis points.
    pub fn initialize(
        env: Env,
        admin: Address,
        token: Address,
        savings_target: i128,
        max_duration_ledgers: u32,
        early_withdrawal_penalty_bps: u32,
    ) -> Result<(), EscrowError> {
        // Prevent re-initialization.
        if env.storage().instance().has(&DataKey::Config) {
            return Err(EscrowError::AlreadyInitialized);
        }

        // Validate inputs.
        if savings_target <= 0 {
            return Err(EscrowError::InvalidAmount);
        }

        admin.require_auth();

        let config = EscrowConfig {
            admin,
            token,
            savings_target,
            max_duration_ledgers,
            early_withdrawal_penalty_bps,
        };

        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::TotalPooled, &0i128);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        Ok(())
    }

    /// Deposit USDC into the escrow toward the borrower's savings target.
    ///
    /// The borrower must authorize this call. USDC is transferred from the
    /// borrower's wallet to this contract. The borrower's balance and the
    /// total pooled amount are updated accordingly.
    pub fn deposit(env: Env, borrower: Address, amount: i128) -> Result<(), EscrowError> {
        borrower.require_auth();

        if amount <= 0 {
            return Err(EscrowError::InvalidAmount);
        }

        let config = Self::get_config(&env)?;
        let mut record = Self::get_borrower(&env, &borrower);

        // Cannot deposit if already released or withdrawn.
        if record.released {
            return Err(EscrowError::AlreadyReleased);
        }
        if record.withdrawn {
            return Err(EscrowError::AlreadyWithdrawn);
        }

        // Transfer USDC from borrower to this contract.
        let token = get_token_client(&env, &config.token);
        token.transfer(&borrower, &env.current_contract_address(), &amount);

        // Set start ledger on first deposit.
        if record.deposited == 0 {
            record.start_ledger = env.ledger().sequence();
        }

        record.deposited += amount;
        Self::set_borrower(&env, &borrower, &record);

        // Update total pooled.
        let total = Self::get_total_pooled(&env) + amount;
        env.storage().instance().set(&DataKey::TotalPooled, &total);

        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        Ok(())
    }

    /// Returns the contract version.
    pub fn version(env: Env) -> u32 {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        1
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, token::StellarAssetClient, Env};

    /// Helper: deploy a test USDC token, mint to borrower, initialize escrow.
    fn setup_with_token(env: &Env) -> (Address, Address, Address, EscrowContractClient<'_>) {
        let admin = Address::generate(env);
        let borrower = Address::generate(env);

        // Deploy a test SAC token (simulates USDC).
        let token_admin = Address::generate(env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
        let token_address = token_id.address();
        let sac_client = StellarAssetClient::new(env, &token_address);

        // Mint 50,000 USDC to borrower.
        sac_client.mint(&borrower, &50_000_0000000i128);

        // Register and initialize escrow.
        let contract_id = env.register(EscrowContract, ());
        let client = EscrowContractClient::new(env, &contract_id);
        client.initialize(
            &admin,
            &token_address,
            &10_000_0000000i128, // 10,000 USDC target
            &518_400u32,
            &500u32,
        );

        (admin, borrower, token_address, client)
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(EscrowContract, ());
        let client = EscrowContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(
            &admin,
            &token,
            &10_000_0000000i128,
            &518_400u32,
            &500u32,
        );

        // Verify config was stored by reading from the contract's context.
        env.as_contract(&contract_id, || {
            let stored_config: EscrowConfig = env
                .storage()
                .instance()
                .get(&DataKey::Config)
                .unwrap();

            assert_eq!(stored_config.admin, admin);
            assert_eq!(stored_config.token, token);
            assert_eq!(stored_config.savings_target, 10_000_0000000i128);
            assert_eq!(stored_config.max_duration_ledgers, 518_400u32);
            assert_eq!(stored_config.early_withdrawal_penalty_bps, 500u32);
        });
    }

    #[test]
    fn test_double_initialize_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(EscrowContract, ());
        let client = EscrowContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin, &token, &10_000_0000000i128, &518_400u32, &500u32);

        let result = client.try_initialize(&admin, &token, &10_000_0000000i128, &518_400u32, &500u32);
        assert!(result.is_err());
    }

    #[test]
    fn test_deposit() {
        let env = Env::default();
        env.mock_all_auths();

        let (_admin, borrower, token_address, client) = setup_with_token(&env);
        let token = soroban_sdk::token::Client::new(&env, &token_address);

        // Deposit 2,000 USDC.
        client.deposit(&borrower, &2_000_0000000i128);

        // Check borrower balance in contract.
        let contract_balance = token.balance(&client.address);
        assert_eq!(contract_balance, 2_000_0000000i128);

        // Deposit again.
        client.deposit(&borrower, &3_000_0000000i128);

        let contract_balance = token.balance(&client.address);
        assert_eq!(contract_balance, 5_000_0000000i128);
    }

    #[test]
    fn test_deposit_zero_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let (_admin, borrower, _token_address, client) = setup_with_token(&env);

        let result = client.try_deposit(&borrower, &0i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_version() {
        let env = Env::default();
        let contract_id = env.register(EscrowContract, ());
        let client = EscrowContractClient::new(&env, &contract_id);
        assert_eq!(client.version(), 1);
    }
}
