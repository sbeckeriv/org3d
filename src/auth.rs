//! Optional HTTP Basic auth for the headless server (`ORG3D_AUTH=user:password`).
//!
//! Basic auth over plain HTTP is readable by anyone sniffing the LAN; it keeps
//! out housemates' devices and drive-by browser requests, not a determined
//! attacker on your network. For access from outside the LAN, put org3d behind
//! a reverse proxy with TLS (and ideally its own auth) instead of exposing it.

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone)]
pub struct Credentials {
    user: String,
    password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the password, e.g. in a panic or debug log.
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Parse `user:password`. The password may itself contain `:`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (user, password) = s
            .split_once(':')
            .ok_or("expected user:password".to_string())?;
        if user.is_empty() || password.is_empty() {
            return Err("user and password must both be non-empty".into());
        }
        if password.len() < 8 {
            return Err("password must be at least 8 characters".into());
        }
        Ok(Self {
            user: user.to_string(),
            password: password.to_string(),
        })
    }

    /// Check an `Authorization` header value. Constant-time in the contents
    /// of the credentials, so timing doesn't reveal how much of a guess matched.
    pub fn verify(&self, header: Option<&HeaderValue>) -> bool {
        let Some(encoded) = header.and_then(|h| h.to_str().ok()).and_then(|h| {
            h.strip_prefix("Basic ")
                .or_else(|| h.strip_prefix("basic "))
        }) else {
            return false;
        };
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
            return false;
        };
        let Some(colon) = decoded.iter().position(|&b| b == b':') else {
            return false;
        };
        let user_ok = ct_eq(&decoded[..colon], self.user.as_bytes());
        let pass_ok = ct_eq(&decoded[colon + 1..], self.password.as_bytes());
        user_ok & pass_ok
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Paths that never need a login: the container healthcheck, and versioned
/// static JS (public anyway, and needed to render the login-failure page).
fn is_public(path: &str) -> bool {
    path == "/health_check" || path.starts_with("/static/")
}

/// Delay before answering a failed login, to slow down password guessing.
const FAIL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

pub async fn require_auth(
    State(creds): State<Option<Arc<Credentials>>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(creds) = creds else {
        return next.run(req).await;
    };
    if is_public(req.uri().path()) || creds.verify(req.headers().get(header::AUTHORIZATION)) {
        return next.run(req).await;
    }
    if req.headers().contains_key(header::AUTHORIZATION) {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_string())
            .unwrap_or_else(|| "unknown".into());
        tracing::warn!("failed login from {peer} for {}", req.uri().path());
        tokio::time::sleep(FAIL_DELAY).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            r#"Basic realm="org3d", charset="UTF-8""#,
        )],
        "Login required.",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(user: &str, pass: &str) -> HeaderValue {
        let enc = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        HeaderValue::from_str(&format!("Basic {enc}")).unwrap()
    }

    #[test]
    fn parse_rules() {
        assert!(Credentials::parse("nouser").is_err());
        assert!(Credentials::parse(":password1").is_err());
        assert!(Credentials::parse("me:short").is_err());
        let c = Credentials::parse("me:pass:with:colons").unwrap();
        assert!(c.verify(Some(&header("me", "pass:with:colons"))));
    }

    #[test]
    fn verify_rejects_wrong_or_malformed() {
        let c = Credentials::parse("me:correct-horse").unwrap();
        assert!(c.verify(Some(&header("me", "correct-horse"))));
        assert!(!c.verify(Some(&header("me", "correct-hors"))));
        assert!(!c.verify(Some(&header("you", "correct-horse"))));
        assert!(!c.verify(None));
        assert!(!c.verify(Some(&HeaderValue::from_static("Basic !!!"))));
        assert!(!c.verify(Some(&HeaderValue::from_static("Bearer abc"))));
    }

    #[test]
    fn debug_hides_password() {
        let c = Credentials::parse("me:correct-horse").unwrap();
        assert!(!format!("{c:?}").contains("correct-horse"));
    }
}
