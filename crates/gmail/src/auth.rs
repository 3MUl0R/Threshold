//! OAuth 2.0 authentication for Gmail API.
//!
//! Handles the full OAuth consent flow with PKCE (RFC 7636) and state
//! parameter for CSRF protection:
//! 1. User runs `threshold gmail auth --inbox user@gmail.com`
//! 2. CLI prints Google OAuth URL for user to visit
//! 3. Local HTTP server receives callback with authorization code
//! 4. State parameter is verified to prevent CSRF
//! 5. Code is exchanged for access + refresh tokens (with PKCE verifier)
//! 6. Tokens are stored in OS keychain, namespaced by inbox
//!
//! On subsequent API calls, access tokens are retrieved from keychain
//! and automatically refreshed when expired.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use threshold_core::SecretStore;
use url::Url;

use crate::types::{TokenErrorResponse, TokenResponse};

/// Google OAuth 2.0 endpoints.
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Local callback server for OAuth redirect.
const REDIRECT_HOST: &str = "127.0.0.1";
const REDIRECT_PORT: u16 = 8085;

/// Gmail API scopes.
pub const SCOPE_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub const SCOPE_SEND: &str = "https://www.googleapis.com/auth/gmail.send";

/// Keychain key names.
const KEY_CLIENT_ID: &str = "gmail-oauth-client-id";
const KEY_CLIENT_SECRET: &str = "gmail-oauth-client-secret";

/// Build inbox-specific keychain key for access token.
pub fn access_token_key(inbox: &str) -> String {
    format!("gmail-oauth-access-token-{}", inbox)
}

/// Build inbox-specific keychain key for refresh token.
pub fn refresh_token_key(inbox: &str) -> String {
    format!("gmail-oauth-refresh-token-{}", inbox)
}

/// Gmail OAuth 2.0 authentication manager.
///
/// Each instance is bound to a specific inbox (email address).
pub struct GmailAuth {
    secret_store: Arc<SecretStore>,
    inbox: String,
    http: reqwest::Client,
}

impl GmailAuth {
    /// Create a new auth manager for the given inbox.
    pub fn new(secret_store: Arc<SecretStore>, inbox: &str) -> Self {
        Self {
            secret_store,
            inbox: inbox.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Run the interactive OAuth consent flow with PKCE and state.
    ///
    /// 1. Resolves client ID and secret from keychain
    /// 2. Generates PKCE code verifier/challenge and random state
    /// 3. Prints authorization URL for user to visit (to stderr)
    /// 4. Starts local HTTP server to receive callback
    /// 5. Verifies state parameter matches to prevent CSRF
    /// 6. Exchanges authorization code for tokens (with PKCE verifier)
    /// 7. Stores tokens in keychain
    pub async fn authorize(&self, include_send_scope: bool) -> Result<(), AuthError> {
        let client_id = self.resolve_client_id()?;
        let client_secret = self.resolve_client_secret()?;

        let mut scopes = vec![SCOPE_READONLY];
        if include_send_scope {
            scopes.push(SCOPE_SEND);
        }

        // Generate PKCE code verifier and challenge (RFC 7636)
        let code_verifier = generate_pkce_verifier();
        let code_challenge = generate_pkce_challenge(&code_verifier);

        // Generate random state for CSRF protection
        let state = generate_random_state();

        let redirect_uri = format!("http://{}:{}", REDIRECT_HOST, REDIRECT_PORT);
        let auth_url =
            build_auth_url(&client_id, &scopes, &redirect_uri, &state, &code_challenge);

        // Interactive prompts go to stderr to keep stdout JSON-only
        eprintln!("Open this URL in your browser to authorize Gmail access:\n");
        eprintln!("  {}\n", auth_url);
        eprintln!("Waiting for authorization callback on {}...", redirect_uri);

        let (code, returned_state) = receive_authorization_code().await?;

        // Verify state to prevent CSRF
        if returned_state.as_deref() != Some(state.as_str()) {
            return Err(AuthError::CallbackError(
                "OAuth state mismatch — possible CSRF attack. Please try again.".into(),
            ));
        }

        tracing::info!("Received authorization code, exchanging for tokens...");

        let token_response = exchange_code(
            &self.http,
            &code,
            &client_id,
            &client_secret,
            &redirect_uri,
            &code_verifier,
        )
        .await?;

        // Store access token
        self.secret_store
            .set(&access_token_key(&self.inbox), &token_response.access_token)
            .map_err(|e| AuthError::KeychainError(e.to_string()))?;

        // Store refresh token (if provided — always present on first auth)
        if let Some(ref refresh) = token_response.refresh_token {
            self.secret_store
                .set(&refresh_token_key(&self.inbox), refresh)
                .map_err(|e| AuthError::KeychainError(e.to_string()))?;
        }

        tracing::info!("OAuth tokens stored for inbox: {}", self.inbox);
        Ok(())
    }

    /// Get a valid access token, refreshing if needed.
    ///
    /// Tries the stored access token first. If that fails (expired/missing),
    /// uses the refresh token to obtain a new access token.
    pub async fn get_access_token(&self) -> Result<String, AuthError> {
        // Try stored access token
        match self
            .secret_store
            .get(&access_token_key(&self.inbox))
            .map_err(|e| AuthError::KeychainError(e.to_string()))?
        {
            Some(token) if !token.is_empty() => {
                // We don't track expiry locally — if the token is rejected by
                // the API, the caller should call refresh_access_token().
                Ok(token)
            }
            _ => {
                // No stored access token, try refresh
                self.refresh_access_token().await
            }
        }
    }

    /// Refresh the access token using the stored refresh token.
    pub async fn refresh_access_token(&self) -> Result<String, AuthError> {
        let refresh = self
            .secret_store
            .get(&refresh_token_key(&self.inbox))
            .map_err(|e| AuthError::KeychainError(e.to_string()))?
            .ok_or_else(|| {
                AuthError::NotAuthorized(format!(
                    "No refresh token for inbox '{}'. Run: threshold gmail auth --inbox {}",
                    self.inbox, self.inbox
                ))
            })?;

        let client_id = self.resolve_client_id()?;
        let client_secret = self.resolve_client_secret()?;

        let token_response =
            refresh_token(&self.http, &refresh, &client_id, &client_secret).await?;

        // Store the new access token
        self.secret_store
            .set(
                &access_token_key(&self.inbox),
                &token_response.access_token,
            )
            .map_err(|e| AuthError::KeychainError(e.to_string()))?;

        // If a new refresh token was issued, store it too
        if let Some(ref new_refresh) = token_response.refresh_token {
            self.secret_store
                .set(&refresh_token_key(&self.inbox), new_refresh)
                .map_err(|e| AuthError::KeychainError(e.to_string()))?;
        }

        Ok(token_response.access_token)
    }

    /// The inbox this auth manager is bound to.
    pub fn inbox(&self) -> &str {
        &self.inbox
    }

    fn resolve_client_id(&self) -> Result<String, AuthError> {
        self.secret_store
            .resolve(KEY_CLIENT_ID, "GMAIL_OAUTH_CLIENT_ID")
            .map_err(|e| AuthError::KeychainError(e.to_string()))?
            .ok_or_else(|| {
                AuthError::MissingCredentials(
                    "Gmail OAuth client ID not configured. \
                     Store it with: threshold-core keychain or set GMAIL_OAUTH_CLIENT_ID env var"
                        .into(),
                )
            })
    }

    fn resolve_client_secret(&self) -> Result<String, AuthError> {
        self.secret_store
            .resolve(KEY_CLIENT_SECRET, "GMAIL_OAUTH_CLIENT_SECRET")
            .map_err(|e| AuthError::KeychainError(e.to_string()))?
            .ok_or_else(|| {
                AuthError::MissingCredentials(
                    "Gmail OAuth client secret not configured. \
                     Store it with: threshold-core keychain or set GMAIL_OAUTH_CLIENT_SECRET env var"
                        .into(),
                )
            })
    }
}

/// Generate a PKCE code verifier (43-128 char URL-safe random string).
fn generate_pkce_verifier() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("getrandom failed");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate the PKCE code challenge (S256) from a verifier.
pub fn generate_pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hash)
}

/// Generate a random state string for CSRF protection.
fn generate_random_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("getrandom failed");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the Google OAuth authorization URL with PKCE and state.
pub fn build_auth_url(
    client_id: &str,
    scopes: &[&str],
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> String {
    let mut url = Url::parse(GOOGLE_AUTH_URL).expect("valid static URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &scopes.join(" "))
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    url.to_string()
}

/// Start a local HTTP server and wait for the OAuth callback.
///
/// Returns the authorization code and state from the callback query parameters.
async fn receive_authorization_code() -> Result<(String, Option<String>), AuthError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind((REDIRECT_HOST, REDIRECT_PORT))
        .await
        .map_err(|e| {
            AuthError::CallbackError(format!(
                "Failed to bind {}:{}: {}",
                REDIRECT_HOST, REDIRECT_PORT, e
            ))
        })?;

    let (mut stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(300), // 5 minute timeout
        listener.accept(),
    )
    .await
    .map_err(|_| AuthError::CallbackError("Timed out waiting for OAuth callback".into()))?
    .map_err(|e| AuthError::CallbackError(format!("Accept failed: {}", e)))?;

    // Read the HTTP request (loop until we have the full request line)
    let mut buf = vec![0u8; 4096];
    let mut total = 0;
    loop {
        if total >= buf.len() {
            return Err(AuthError::CallbackError("Request too large".into()));
        }
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read(&mut buf[total..]),
        )
        .await
        .map_err(|_| AuthError::CallbackError("Read timed out".into()))?
        .map_err(|e| AuthError::CallbackError(format!("Read failed: {}", e)))?;

        if n == 0 {
            break;
        }
        total += n;

        // We only need the first line, check if we have \r\n or \n
        if buf[..total].windows(2).any(|w| w == b"\r\n")
            || buf[..total].contains(&b'\n')
        {
            break;
        }
    }

    let request = String::from_utf8_lossy(&buf[..total]);

    // Parse the GET request line to extract query parameters
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| AuthError::CallbackError("Invalid HTTP request".into()))?;

    let (code, state) = extract_code_and_state_from_path(path)?;

    // Send a success response (no leading whitespace in headers)
    let response = "HTTP/1.1 200 OK\r\n\
Content-Type: text/html\r\n\
Connection: close\r\n\
\r\n\
<html><body><h2>Authorization successful!</h2>\
<p>You can close this tab and return to the terminal.</p>\
</body></html>";

    let _ = stream.write_all(response.as_bytes()).await;

    Ok((code, state))
}

/// Extract the authorization code and state from the callback URL path.
fn extract_code_and_state_from_path(path: &str) -> Result<(String, Option<String>), AuthError> {
    let url = Url::parse(&format!("http://localhost{}", path))
        .map_err(|e| AuthError::CallbackError(format!("Invalid callback URL: {}", e)))?;

    // Check for error response
    if let Some(error) = url.query_pairs().find(|(k, _)| k == "error") {
        return Err(AuthError::OAuthRejected(error.1.to_string()));
    }

    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| AuthError::CallbackError("No 'code' parameter in callback".into()))?;

    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string());

    Ok((code, state))
}

/// Exchange an authorization code for access and refresh tokens (with PKCE verifier).
async fn exchange_code(
    http: &reqwest::Client,
    code: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenResponse, AuthError> {
    let response = http
        .post(GOOGLE_TOKEN_URL)
        .form(&[
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", code_verifier),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AuthError::TokenExchangeFailed(format!("HTTP request failed: {}", e)))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if let Ok(err_resp) = serde_json::from_str::<TokenErrorResponse>(&body) {
            return Err(AuthError::TokenExchangeFailed(format!(
                "{}: {}",
                err_resp.error,
                err_resp.error_description.unwrap_or_default()
            )));
        }
        return Err(AuthError::TokenExchangeFailed(format!(
            "HTTP {}: {}",
            status, body
        )));
    }

    response
        .json::<TokenResponse>()
        .await
        .map_err(|e| AuthError::TokenExchangeFailed(format!("Parse response: {}", e)))
}

/// Refresh an access token using a refresh token.
async fn refresh_token(
    http: &reqwest::Client,
    refresh: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<TokenResponse, AuthError> {
    let response = http
        .post(GOOGLE_TOKEN_URL)
        .form(&[
            ("refresh_token", refresh),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "refresh_token"),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AuthError::TokenRefreshFailed(format!("HTTP request failed: {}", e)))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if let Ok(err_resp) = serde_json::from_str::<TokenErrorResponse>(&body) {
            return Err(AuthError::TokenRefreshFailed(format!(
                "{}: {}",
                err_resp.error,
                err_resp.error_description.unwrap_or_default()
            )));
        }
        return Err(AuthError::TokenRefreshFailed(format!(
            "HTTP {}: {}",
            status, body
        )));
    }

    response
        .json::<TokenResponse>()
        .await
        .map_err(|e| AuthError::TokenRefreshFailed(format!("Parse response: {}", e)))
}

/// Errors specific to Gmail authentication.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("OAuth credentials not configured: {0}")]
    MissingCredentials(String),

    #[error("Not authorized for inbox: {0}")]
    NotAuthorized(String),

    #[error("OAuth callback error: {0}")]
    CallbackError(String),

    #[error("User rejected OAuth consent: {0}")]
    OAuthRejected(String),

    #[error("Token exchange failed: {0}")]
    TokenExchangeFailed(String),

    #[error("Token refresh failed: {0}")]
    TokenRefreshFailed(String),

    #[error("Keychain error: {0}")]
    KeychainError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_key_includes_inbox() {
        assert_eq!(
            access_token_key("user@gmail.com"),
            "gmail-oauth-access-token-user@gmail.com"
        );
    }

    #[test]
    fn refresh_token_key_includes_inbox() {
        assert_eq!(
            refresh_token_key("user@gmail.com"),
            "gmail-oauth-refresh-token-user@gmail.com"
        );
    }

    #[test]
    fn different_inboxes_get_different_keys() {
        let key1 = access_token_key("alice@gmail.com");
        let key2 = access_token_key("bob@company.com");
        assert_ne!(key1, key2);
    }

    #[test]
    fn build_auth_url_contains_required_params() {
        let url = build_auth_url(
            "test-client-id",
            &[SCOPE_READONLY],
            "http://127.0.0.1:8085",
            "test-state",
            "test-challenge",
        );

        assert!(url.contains("client_id=test-client-id"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("gmail.readonly"));
        assert!(url.contains("state=test-state"));
        assert!(url.contains("code_challenge=test-challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.starts_with(GOOGLE_AUTH_URL));
    }

    #[test]
    fn build_auth_url_with_send_scope() {
        let url = build_auth_url(
            "test-client-id",
            &[SCOPE_READONLY, SCOPE_SEND],
            "http://127.0.0.1:8085",
            "state",
            "challenge",
        );

        assert!(url.contains("gmail.readonly"));
        assert!(url.contains("gmail.send"));
    }

    #[test]
    fn build_auth_url_encodes_redirect_uri() {
        let url = build_auth_url(
            "test-client-id",
            &[SCOPE_READONLY],
            "http://127.0.0.1:8085",
            "state",
            "challenge",
        );

        // URL should contain the redirect_uri parameter
        assert!(url.contains("redirect_uri="));
    }

    #[test]
    fn pkce_challenge_is_deterministic() {
        let verifier = "test-verifier-string";
        let c1 = generate_pkce_challenge(verifier);
        let c2 = generate_pkce_challenge(verifier);
        assert_eq!(c1, c2);
        assert!(!c1.is_empty());
    }

    #[test]
    fn pkce_challenge_differs_for_different_verifiers() {
        let c1 = generate_pkce_challenge("verifier-a");
        let c2 = generate_pkce_challenge("verifier-b");
        assert_ne!(c1, c2);
    }

    #[test]
    fn extract_code_and_state_from_valid_path() {
        let path = "/?code=4/P7q7W91a-oMsCeLvIaQm6bTrgtp7&state=abc123&scope=email%20profile";
        let (code, state) = extract_code_and_state_from_path(path).unwrap();
        assert_eq!(code, "4/P7q7W91a-oMsCeLvIaQm6bTrgtp7");
        assert_eq!(state.unwrap(), "abc123");
    }

    #[test]
    fn extract_code_and_state_without_state() {
        let path = "/?code=4/P7q7W91a-oMsCeLvIaQm6bTrgtp7";
        let (code, state) = extract_code_and_state_from_path(path).unwrap();
        assert_eq!(code, "4/P7q7W91a-oMsCeLvIaQm6bTrgtp7");
        assert!(state.is_none());
    }

    #[test]
    fn extract_code_from_path_with_error() {
        let path = "/?error=access_denied";
        let result = extract_code_and_state_from_path(path);
        assert!(result.is_err());
        match result.unwrap_err() {
            AuthError::OAuthRejected(msg) => assert_eq!(msg, "access_denied"),
            other => panic!("Expected OAuthRejected, got: {:?}", other),
        }
    }

    #[test]
    fn extract_code_from_path_missing_code() {
        let path = "/?state=xyz";
        let result = extract_code_and_state_from_path(path);
        assert!(result.is_err());
        match result.unwrap_err() {
            AuthError::CallbackError(msg) => assert!(msg.contains("No 'code' parameter")),
            other => panic!("Expected CallbackError, got: {:?}", other),
        }
    }

    #[test]
    fn token_response_parsing() {
        let json = r#"{
            "access_token": "ya29.a0Af...",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "1//0e...",
            "scope": "https://www.googleapis.com/auth/gmail.readonly"
        }"#;

        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "ya29.a0Af...");
        assert_eq!(resp.expires_in, 3600);
        assert!(resp.refresh_token.is_some());
    }

    #[test]
    fn token_error_response_parsing() {
        let json = r#"{
            "error": "invalid_grant",
            "error_description": "Token has been expired or revoked."
        }"#;

        let resp: TokenErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error, "invalid_grant");
    }

    #[test]
    fn auth_error_display() {
        let err = AuthError::MissingCredentials("no client ID".into());
        assert!(err.to_string().contains("no client ID"));

        let err = AuthError::NotAuthorized("user@gmail.com".into());
        assert!(err.to_string().contains("user@gmail.com"));

        let err = AuthError::OAuthRejected("access_denied".into());
        assert!(err.to_string().contains("access_denied"));
    }
}
