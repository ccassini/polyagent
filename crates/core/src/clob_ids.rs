//! Canonical decimal string form for CLOB outcome token ids (uint256).

use std::str::FromStr;

use alloy_primitives::U256;

/// Parses `raw` as a uint256 when possible so Gamma / WS strings match the same key (e.g. leading zeros).
#[must_use]
pub fn normalize_clob_token_id(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    match U256::from_str(s) {
        Ok(u) => u.to_string(),
        Err(_) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_normalizes_leading_zeros() {
        let a = normalize_clob_token_id("001234");
        let b = normalize_clob_token_id("1234");
        assert_eq!(a, b);
    }
}
