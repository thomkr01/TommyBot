//! Authenticator codes (TOTP: SHA-1, 6 digits, 30 s steps, one step of skew). The only file that
//! touches `totp_rs`, so a new major version is fixed here.
use totp_rs::{Builder, Secret, Totp};

fn totp(secret_b32: &str, user: &str) -> anyhow::Result<Totp> {
    let secret = Secret::try_from_base32(secret_b32).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(Builder::new()
        .with_secret(secret)
        .with_issuer(Some("TommyBot"))
        .with_account_name(user)
        .build()?)
}

/// A new random secret, base32 as stored in the database.
pub fn new_secret() -> String {
    Secret::generate().to_base32()
}

/// The `otpauth://` URI an authenticator app scans.
pub fn uri(secret_b32: &str, user: &str) -> anyhow::Result<String> {
    Ok(totp(secret_b32, user)?.to_url()?)
}

/// The time step `code` is valid for (now, or one step either side), so a caller can refuse a replay.
pub fn step(secret_b32: &str, code: &str) -> anyhow::Result<Option<u64>> {
    Ok(totp(secret_b32, "")?.check_current(code))
}

#[cfg(test)]
pub fn current(secret_b32: &str) -> String {
    totp(secret_b32, "").unwrap().generate_current().to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn current_code_checks_and_wrong_code_does_not() {
        let s = super::new_secret();
        assert!(super::step(&s, &super::current(&s)).unwrap().is_some());
        assert!(super::uri(&s, "me")
            .unwrap()
            .starts_with("otpauth://totp/TommyBot:me?"));
        let wrong = if super::current(&s) == "000000" {
            "111111"
        } else {
            "000000"
        };
        assert_eq!(super::step(&s, wrong).unwrap(), None);
    }

    #[test]
    fn secrets_saved_before_the_upgrade_give_the_same_codes() {
        // RFC 6238 test vector: secret "12345678901234567890", T = 59 s, last 6 digits.
        let old = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        assert_eq!(
            super::totp(old, "").unwrap().generate(59).to_string(),
            "287082"
        );
    }
}
