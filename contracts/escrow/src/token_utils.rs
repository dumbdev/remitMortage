use soroban_sdk::{token, Address, Env};

/// Creates a token client for the configured USDC token.
pub fn get_token_client<'a>(env: &'a Env, token_address: &'a Address) -> token::Client<'a> {
    token::Client::new(env, token_address)
}
