use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::input::MAX_PORTS;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchRole {
    Player,
    Spectator,
    Observer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchClaims {
    pub match_id: String,
    pub player_id: String,
    #[serde(default = "default_role")]
    pub role: MatchRole,
    #[serde(default)]
    pub allowed_ports: Vec<u32>,
    pub exp_unix_ms: u64,
    #[serde(default)]
    pub iat_unix_ms: Option<u64>,
}

pub fn validate_match_token(token: &str, secret: &[u8], now_unix_ms: u64) -> Result<MatchClaims> {
    let (payload_part, signature_part) = split_token(token)?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_part)
        .context("invalid token payload encoding")?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature_part)
        .context("invalid token signature encoding")?;

    let mut mac = HmacSha256::new_from_slice(secret).context("invalid auth secret")?;
    mac.update(&payload_bytes);
    mac.verify_slice(&signature)
        .context("token signature verification failed")?;

    let claims: MatchClaims =
        serde_json::from_slice(&payload_bytes).context("invalid token payload JSON")?;

    validate_claims(claims, now_unix_ms)
}

fn validate_claims(mut claims: MatchClaims, now_unix_ms: u64) -> Result<MatchClaims> {
    claims.match_id = claims.match_id.trim().to_owned();
    claims.player_id = claims.player_id.trim().to_owned();

    if claims.match_id.is_empty() {
        bail!("token missing match_id");
    }
    if claims.player_id.is_empty() {
        bail!("token missing player_id");
    }
    if claims.exp_unix_ms <= now_unix_ms {
        bail!("token expired");
    }

    claims.allowed_ports.sort_unstable();
    claims.allowed_ports.dedup();

    if claims.allowed_ports.iter().any(|port| *port >= MAX_PORTS) {
        bail!("token has out-of-range port");
    }

    if matches!(claims.role, MatchRole::Player) && claims.allowed_ports.is_empty() {
        bail!("player token must include at least one allowed port");
    }

    Ok(claims)
}

fn split_token(token: &str) -> Result<(&str, &str)> {
    let mut parts = token.split('.');

    let first = parts.next().context("missing token payload")?;
    let second = parts.next().context("missing token signature")?;
    let third = parts.next();
    let fourth = parts.next();

    if fourth.is_some() {
        bail!("invalid token format");
    }

    match (third, first) {
        (None, _) => Ok((first, second)),
        (Some(signature), "v1") => Ok((second, signature)),
        _ => bail!("invalid token format"),
    }
}

fn default_role() -> MatchRole {
    MatchRole::Player
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sign(payload_json: &str, secret: &[u8]) -> String {
        let payload = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let mut mac = HmacSha256::new_from_slice(secret).expect("hmac");
        mac.update(payload_json.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{payload}.{signature}")
    }

    #[test]
    fn validates_player_token() {
        let secret = b"test-secret";
        let now = 1_700_000_000_000u64;
        let payload = json!({
            "match_id": "m-1",
            "player_id": "p-1",
            "role": "player",
            "allowed_ports": [0],
            "exp_unix_ms": now + 1000
        })
        .to_string();
        let token = sign(&payload, secret);
        let claims = validate_match_token(&token, secret, now).expect("valid token");
        assert_eq!(claims.match_id, "m-1");
        assert_eq!(claims.allowed_ports, vec![0]);
    }

    #[test]
    fn rejects_expired_token() {
        let secret = b"test-secret";
        let now = 1_700_000_000_000u64;
        let payload = json!({
            "match_id": "m-1",
            "player_id": "p-1",
            "role": "player",
            "allowed_ports": [0],
            "exp_unix_ms": now - 1
        })
        .to_string();
        let token = sign(&payload, secret);
        assert!(validate_match_token(&token, secret, now).is_err());
    }
}
