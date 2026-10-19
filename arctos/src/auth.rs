//! "Sign in with Arctos": OAuth 2.0 authorization code flow with PKCE for a
//! native app (RFC 8252), using a loopback redirect.
//!
//! The recorder is registered on Arctos as a public app with the redirect
//! URI [`REDIRECT_PATH`] on `http://127.0.0.1` (any port matches).

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::future::BoxFuture;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{Arctos, json_response};

pub const REDIRECT_PATH: &str = "/callback";
/// Lets the recorder upload footage and see unpublished tournaments the
/// user organizes.
const SCOPE: &str = "footage";
const SIGNED_IN_PAGE: &str = "<!doctype html><title>Arctos Recorder</title>\
    <p style=\"font-family: sans-serif\">You're signed in. You can close this tab \
    and go back to Arctos Recorder.</p>";

/// Something that can hand out a currently valid access token, refreshing
/// it if needed.
pub trait TokenSource: Send + Sync {
    fn access_token(&self) -> BoxFuture<'_, Result<String>>;
}

/// A fixed access token, e.g. one handed to a server for a single job.
impl TokenSource for String {
    fn access_token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move { Ok(self.clone()) })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Rotates on every refresh: always keep the newest one.
    pub refresh_token: String,
    /// Seconds.
    pub expires_in: u64,
}

/// An Arctos account.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
}

impl Arctos {
    /// Signs the user in through their browser: `open_browser` is called
    /// with the URL of Arctos's consent page, and this returns once they
    /// have allowed (or denied) access.
    pub async fn sign_in(
        &self,
        client_id: &str,
        open_browser: impl FnOnce(&str),
    ) -> Result<Tokens> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let redirect_uri = format!(
            "http://127.0.0.1:{}{REDIRECT_PATH}",
            listener.local_addr()?.port()
        );
        let verifier = random_string();
        let state = random_string();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = reqwest::Url::parse(&format!("{}/oauth/authorize", self.url))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        open_browser(url.as_str());

        let code = loop {
            let (mut connection, _) = listener.accept().await?;
            let mut request = vec![0; 8192];
            let len = connection.read(&mut request).await?;
            let Some(query) = callback_query(&request[..len]) else {
                connection
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await?;
                continue;
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{SIGNED_IN_PAGE}",
                SIGNED_IN_PAGE.len()
            );
            connection.write_all(response.as_bytes()).await?;
            let param = |name: &str| {
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
            };
            if param("state").as_deref() != Some(&state) {
                bail!("the sign-in response didn't match the request");
            }
            if let Some(error) = param("error") {
                bail!("signing in to Arctos failed: {error}");
            }
            break param("code").context("Arctos sent no authorization code")?;
        };

        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", client_id),
            ("code_verifier", &verifier),
        ])
        .await
    }

    /// Trades a refresh token for new tokens. The old refresh token stops
    /// working.
    pub async fn refresh(&self, client_id: &str, refresh_token: &str) -> Result<Tokens> {
        self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .await
    }

    /// Signs out: the token and the rest of its grant stop working.
    pub async fn revoke(&self, client_id: &str, refresh_token: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/api/oauth/revoke", self.url))
            .form(&[("token", refresh_token), ("client_id", client_id)])
            .send()
            .await?;
        json_response::<serde_json::Value>(response).await?;
        Ok(())
    }

    pub async fn user(&self, access_token: &str) -> Result<User> {
        let response = self
            .http
            .get(format!("{}/api/oauth/userinfo", self.url))
            .bearer_auth(access_token)
            .send()
            .await?;
        json_response(response).await
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<Tokens> {
        let response = self
            .http
            .post(format!("{}/api/oauth/token", self.url))
            .form(form)
            .send()
            .await?;
        json_response(response).await
    }
}

/// The query of a `GET {REDIRECT_PATH}?...` request.
fn callback_query(request: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(request).ok()?.lines().next()?;
    let target = line.strip_prefix("GET ")?.split(' ').next()?;
    let query = target.strip_prefix(REDIRECT_PATH)?.strip_prefix('?')?;
    Some(query.to_owned())
}

fn random_string() -> String {
    let mut bytes = [0; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_query_reads_the_redirect() {
        let request = b"GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert_eq!(
            callback_query(request).as_deref(),
            Some("code=abc&state=xyz")
        );
        assert_eq!(callback_query(b"GET /favicon.ico HTTP/1.1\r\n\r\n"), None);
    }
}
