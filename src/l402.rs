use anyhow::Result;
use bitcoin::hashes::hex::FromHex;
use bitcoin::hashes::{sha256, Hash};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tonic_openssl_lnd::lnrpc;
use tonic_openssl_lnd::LndLightningClient;

#[derive(Clone)]
pub struct L402Config {
    pub enabled: bool,
    pub invoice_amount_sats: u64,
}

#[derive(Serialize, Deserialize)]
pub struct L402Claims {
    pub payment_hash: String,
    pub exp: usize,
    pub iat: usize,
}

pub struct L402TokenResponse {
    pub invoice: String,
    pub token: String,
}

pub async fn generate_l402_token(
    mainnet_client: &LndLightningClient,
    jwt_secret: &str,
    amount_sats: u64,
) -> Result<L402TokenResponse> {
    let inv = lnrpc::Invoice {
        memo: "Mutinynet Faucet L402 Auth".to_string(),
        value: amount_sats as i64,
        expiry: 600, // 10 minutes
        ..Default::default()
    };

    let response = mainnet_client.clone().add_invoice(inv).await?.into_inner();
    let payment_hash = sha256::Hash::from_slice(&response.r_hash)
        .map_err(|e| anyhow::anyhow!("Invalid payment hash from LND: {}", e))?
        .to_string();

    let now = chrono::Utc::now().timestamp() as usize;
    let claims = L402Claims {
        payment_hash,
        exp: (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp() as usize,
        iat: now,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )?;

    Ok(L402TokenResponse {
        invoice: response.payment_request,
        token,
    })
}

pub fn verify_l402_preimage(preimage_hex: &str, payment_hash_hex: &str) -> bool {
    let preimage_bytes = match Vec::<u8>::from_hex(preimage_hex) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let expected = match sha256::Hash::from_str(payment_hash_hex) {
        Ok(h) => h,
        Err(_) => return false,
    };

    sha256::Hash::hash(&preimage_bytes) == expected
}

#[derive(Debug, PartialEq)]
pub enum L402Error {
    InvalidToken,
    TokenExpired,
    InvalidPreimage,
}

/// Validates L402 credentials: decodes the JWT, checks expiry, and verifies the preimage.
/// Returns the payment_hash on success.
pub fn validate_l402_credentials(
    token: &str,
    preimage_hex: &str,
    jwt_secret: &str,
) -> Result<String, L402Error> {
    let token_data = decode::<L402Claims>(
        token,
        &DecodingKey::from_secret(jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => L402Error::TokenExpired,
        _ => L402Error::InvalidToken,
    })?;

    if !verify_l402_preimage(preimage_hex, &token_data.claims.payment_hash) {
        return Err(L402Error::InvalidPreimage);
    }

    Ok(token_data.claims.payment_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &str = "test_secret";
    const TEST_PREIMAGE: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    fn test_payment_hash() -> String {
        let preimage_bytes = Vec::<u8>::from_hex(TEST_PREIMAGE).unwrap();
        sha256::Hash::hash(&preimage_bytes).to_string()
    }

    fn make_test_token(payment_hash: &str, secret: &str, exp_offset_hours: i64) -> String {
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = L402Claims {
            payment_hash: payment_hash.to_string(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(exp_offset_hours)).timestamp()
                as usize,
            iat: now,
        };
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    // -- verify_l402_preimage tests --

    #[test]
    fn test_verify_preimage_valid() {
        assert!(verify_l402_preimage(TEST_PREIMAGE, &test_payment_hash()));
    }

    #[test]
    fn test_verify_preimage_wrong_hash() {
        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!verify_l402_preimage(TEST_PREIMAGE, wrong_hash));
    }

    #[test]
    fn test_verify_preimage_bad_hex() {
        assert!(!verify_l402_preimage("not_hex", "also_not_hex"));
    }

    // -- validate_l402_credentials tests --

    #[test]
    fn test_validate_valid_credentials() {
        let payment_hash = test_payment_hash();
        let token = make_test_token(&payment_hash, TEST_SECRET, 24);

        let result = validate_l402_credentials(&token, TEST_PREIMAGE, TEST_SECRET);
        assert_eq!(result, Ok(payment_hash));
    }

    #[test]
    fn test_validate_wrong_preimage() {
        let payment_hash = test_payment_hash();
        let token = make_test_token(&payment_hash, TEST_SECRET, 24);

        let wrong_preimage = "0000000000000000000000000000000000000000000000000000000000000002";
        let result = validate_l402_credentials(&token, wrong_preimage, TEST_SECRET);
        assert_eq!(result, Err(L402Error::InvalidPreimage));
    }

    #[test]
    fn test_validate_expired_token() {
        let payment_hash = test_payment_hash();
        let token = make_test_token(&payment_hash, TEST_SECRET, -1);

        let result = validate_l402_credentials(&token, TEST_PREIMAGE, TEST_SECRET);
        assert_eq!(result, Err(L402Error::TokenExpired));
    }

    #[test]
    fn test_validate_wrong_secret() {
        let payment_hash = test_payment_hash();
        let token = make_test_token(&payment_hash, "secret_a", 24);

        let result = validate_l402_credentials(&token, TEST_PREIMAGE, "secret_b");
        assert_eq!(result, Err(L402Error::InvalidToken));
    }

    #[test]
    fn test_validate_garbage_token() {
        let result = validate_l402_credentials("not.a.jwt", TEST_PREIMAGE, TEST_SECRET);
        assert_eq!(result, Err(L402Error::InvalidToken));
    }

    #[test]
    fn test_validate_github_jwt_rejected() {
        // A GitHub-style JWT (has `sub`, no `payment_hash`) must not decode as L402Claims
        let github_claims = serde_json::json!({
            "sub": "user@example.com",
            "exp": 9999999999u64,
            "iat": 1000000000u64,
        });
        let token = encode(
            &Header::default(),
            &github_claims,
            &EncodingKey::from_secret(TEST_SECRET.as_bytes()),
        )
        .unwrap();

        let result = validate_l402_credentials(&token, TEST_PREIMAGE, TEST_SECRET);
        assert_eq!(result, Err(L402Error::InvalidToken));
    }
}
