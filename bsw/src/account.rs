//! The Arctos account the base station acts as, signed in with "Sign in with
//! Arctos". Only the refresh token is saved (in [`ACCOUNT_FILE`]); access
//! tokens are kept in memory.
//!
//! The recorder's OAuth client id is `$ARCTOS_CLIENT_ID`, or the one it was
//! built with (`ARCTOS_CLIENT_ID` at build time).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arctos::{Arctos, TokenSource, Tokens, User};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::AbortHandle;

pub const ACCOUNT_FILE: &str = "account.json";
/// Access tokens are refreshed this long before they expire.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub enum AccountStatus {
    SignedOut {
        error: Option<String>,
    },
    /// Waiting for the user to allow access in their browser.
    SigningIn,
    SignedIn(User),
}

pub struct Account {
    arctos: Arctos,
    path: PathBuf,
    pub status: watch::Sender<AccountStatus>,
    /// Held while refreshing: refresh tokens rotate, and Arctos revokes the
    /// whole sign-in if an old one is used again.
    session: tokio::sync::Mutex<Option<Session>>,
    sign_in_task: Mutex<Option<AbortHandle>>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    refresh_token: String,
    user: User,
}

struct Session {
    refresh_token: String,
    access_token: Option<(String, Instant)>,
}

impl Account {
    pub fn load(arctos: Arctos, path: PathBuf) -> Result<Self> {
        let saved: Option<Saved> = match std::fs::read(&path) {
            Ok(json) => Some(serde_json::from_slice(&json)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let status = match &saved {
            Some(saved) => AccountStatus::SignedIn(saved.user.clone()),
            None => AccountStatus::SignedOut { error: None },
        };
        Ok(Account {
            arctos,
            path,
            status: watch::Sender::new(status),
            session: tokio::sync::Mutex::new(saved.map(|saved| Session {
                refresh_token: saved.refresh_token,
                access_token: None,
            })),
            sign_in_task: Mutex::default(),
        })
    }

    /// Starts signing in through the browser, replacing any sign-in in
    /// progress.
    pub fn sign_in(self: &std::sync::Arc<Self>) {
        let account = self.clone();
        let task = tokio::spawn(async move {
            account.status.send_replace(AccountStatus::SigningIn);
            let status = match account.complete_sign_in().await {
                Ok(user) => AccountStatus::SignedIn(user),
                Err(e) => {
                    tracing::warn!("signing in to Arctos: {e:#}");
                    AccountStatus::SignedOut {
                        error: Some(format!("{e:#}")),
                    }
                }
            };
            account.status.send_replace(status);
        });
        if let Some(previous) = self
            .sign_in_task
            .lock()
            .unwrap()
            .replace(task.abort_handle())
        {
            previous.abort();
        }
    }

    pub fn cancel_sign_in(&self) {
        if let Some(task) = self.sign_in_task.lock().unwrap().take() {
            task.abort();
        }
        self.status.send_if_modified(|status| {
            let signing_in = *status == AccountStatus::SigningIn;
            if signing_in {
                *status = AccountStatus::SignedOut { error: None };
            }
            signing_in
        });
    }

    async fn complete_sign_in(&self) -> Result<User> {
        let client_id = client_id()?;
        let tokens = self
            .arctos
            .sign_in(&client_id, |url| {
                if let Err(e) = webbrowser::open(url) {
                    tracing::error!("opening the browser to sign in: {e}");
                }
            })
            .await?;
        let user = self.arctos.user(&tokens.access_token).await?;
        let mut session = self.session.lock().await;
        self.save(&tokens, &user)?;
        *session = Some(Session::new(tokens));
        Ok(user)
    }

    /// Signs out, revoking the sign-in on Arctos.
    pub async fn sign_out(&self) -> Result<()> {
        let mut session = self.session.lock().await;
        if let Some(session) = session.take() {
            std::fs::remove_file(&self.path)?;
            self.status
                .send_replace(AccountStatus::SignedOut { error: None });
            self.arctos
                .revoke(&client_id()?, &session.refresh_token)
                .await?;
        }
        Ok(())
    }

    fn save(&self, tokens: &Tokens, user: &User) -> Result<()> {
        let saved = Saved {
            refresh_token: tokens.refresh_token.clone(),
            user: user.clone(),
        };
        std::fs::write(&self.path, serde_json::to_vec_pretty(&saved)?)?;
        Ok(())
    }
}

impl Session {
    fn new(tokens: Tokens) -> Self {
        let expires = Instant::now() + Duration::from_secs(tokens.expires_in);
        Session {
            refresh_token: tokens.refresh_token,
            access_token: Some((tokens.access_token, expires)),
        }
    }
}

impl TokenSource for Account {
    fn access_token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let mut session = self.session.lock().await;
            let current = session.as_mut().context("not signed in to Arctos")?;
            if let Some((token, expires)) = &current.access_token
                && Instant::now() + EXPIRY_MARGIN < *expires
            {
                return Ok(token.clone());
            }
            let tokens = self
                .arctos
                .refresh(&client_id()?, &current.refresh_token)
                .await
                .context("Arctos didn't renew the sign-in; sign in again")?;
            if let AccountStatus::SignedIn(user) = &*self.status.borrow() {
                self.save(&tokens, user)?;
            }
            *current = Session::new(tokens);
            Ok(current.access_token.clone().expect("just refreshed").0)
        })
    }
}

fn client_id() -> Result<String> {
    std::env::var("ARCTOS_CLIENT_ID")
        .ok()
        .or_else(|| option_env!("ARCTOS_CLIENT_ID").map(str::to_owned))
        .filter(|id| !id.is_empty())
        .context("no Arctos client id: set ARCTOS_CLIENT_ID")
}
