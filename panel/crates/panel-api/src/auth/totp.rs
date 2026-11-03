//! TOTP 2FA (see CLAUDE.md: "TOTP 2FA is built into core").

use totp_rs::{Algorithm, Secret, TOTP};

pub struct TotpEnrollment {
    pub secret_base32: String,
    pub otpauth_url: String,
}

fn build(secret_base32: &str, account_name: &str) -> anyhow::Result<TOTP> {
    let secret = Secret::Encoded(secret_base32.to_string())
        .to_bytes()
        .map_err(|e| anyhow::anyhow!("decoding TOTP secret: {e:?}"))?;
    TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret,
        Some("scontrol".to_string()),
        account_name.to_string(),
    )
    .map_err(|e| anyhow::anyhow!("building TOTP: {e}"))
}

/// Generates a new random TOTP secret for enrollment (not yet persisted;
/// callers must confirm with [`verify_code`] before enabling 2FA).
pub fn generate_enrollment(account_name: &str) -> anyhow::Result<TotpEnrollment> {
    let secret = Secret::generate_secret();
    let secret_base32 = secret.to_encoded().to_string();
    let totp = build(&secret_base32, account_name)?;
    Ok(TotpEnrollment {
        secret_base32,
        otpauth_url: totp.get_url(),
    })
}

pub fn verify_code(secret_base32: &str, account_name: &str, code: &str) -> bool {
    match build(secret_base32, account_name) {
        Ok(totp) => totp.check_current(code).unwrap_or(false),
        Err(_) => false,
    }
}
