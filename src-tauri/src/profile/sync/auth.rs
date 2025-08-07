use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use base64::{prelude::BASE64_URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use eyre::{eyre, Context, OptionExt, Result};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Url};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::{db::Db, state::ManagerExt};

pub struct State {
    creds: Mutex<Option<AuthCredentials>>,
    callback_channel: broadcast::Sender<String>,
}

impl State {
    pub fn new(stored_creds: Option<AuthCredentials>) -> Self {
        Self {
            creds: Mutex::new(stored_creds),
            callback_channel: broadcast::channel(1).0,
        }
    }

    fn creds(&self) -> MutexGuard<Option<AuthCredentials>> {
        self.creds.lock().unwrap()
    }

    pub fn set_creds(&self, creds: Option<AuthCredentials>, db: &Db) -> Result<()> {
        db.save_auth(creds.as_ref())?;
        *self.creds() = creds;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthCredentials {
    user: User,
    access_token: String,
    token_expiry: i64,
    refresh_token: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub discord_id: String,
    pub name: String,
    pub display_name: String,
    pub avatar: Option<String>,
}

impl AuthCredentials {
    fn from_tokens(access_token: String, refresh_token: String) -> Result<Self> {
        let JwtPayload { exp, user } = decode_jwt(&access_token).context("failed to decode jwt")?;

        Ok(Self {
            access_token,
            refresh_token,
            token_expiry: exp,
            user,
        })
    }
}

const OAUTH_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn login_with_oauth(app: &AppHandle) -> Result<User> {
    let url = format!("{}/auth/login", super::API_URL);
    open::that(url).context("failed to open url in browser")?;

    let mut channel = app.sync_auth().callback_channel.subscribe();

    tokio::select! {
        url = channel.recv() => {
         let url = url?;
         let url = Url::parse(&url).context("invalid url")?;
         let query: HashMap<_, _> = url.query_pairs().collect();

         let access_token = query
             .get("access_token")
             .ok_or_eyre("access_token parameter is missing")?
             .clone()
             .into_owned();

         let refresh_token = query
             .get("refresh_token")
             .ok_or_eyre("refresh_token parameter is missing")?
             .clone()
             .into_owned();

         app.get_webview_window("main").unwrap().set_focus().ok();

         let creds = AuthCredentials::from_tokens(access_token, refresh_token)?;
         let user = creds.user.clone();

         info!("logged in as {}", user.name);

         app.sync_auth().set_creds(Some(creds), app.db())?;

         Ok(user)
        }
        _ = tokio::time::sleep(OAUTH_TIMEOUT) => {
            Err(eyre!("auth callback timed out"))
        }
    }
}

pub async fn handle_callback(url: String, app: &AppHandle) -> Result<()> {
    app.sync_auth().callback_channel.send(url)?;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct JwtPayload {
    exp: i64,

    #[serde(flatten)]
    user: User,
}

fn decode_jwt(token: &str) -> Result<JwtPayload> {
    let payload = token.split(".").nth(1).ok_or_eyre("token is malformed")?;

    let bytes = BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .context("failed to decode base64")?;

    serde_json::from_slice(&bytes).context("failed to deserialize json")
}

pub fn user_info(app: &AppHandle) -> Option<User> {
    app.sync_auth()
        .creds()
        .as_ref()
        .map(|state| state.user.clone())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

pub async fn access_token(app: &AppHandle) -> Option<String> {
    let refresh_token = {
        let state = app.sync_auth();
        let creds = state.creds.lock().unwrap();
        let creds = creds.as_ref()?;

        let Some(expiry) = DateTime::from_timestamp(creds.token_expiry, 0) else {
            warn!("token expiry date is invalid");
            return None;
        };

        if Utc::now() < expiry {
            return Some(creds.access_token.clone());
        }

        creds.refresh_token.clone()
    };

    match request_token(refresh_token, app).await {
        Ok(token) => Some(token),
        Err(err) => {
            error!("failed to refresh access token: {:#}", err);
            None
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrantTokenRequest {
    refresh_token: String,
}

async fn request_token(refresh_token: String, app: &AppHandle) -> Result<String> {
    debug!("refreshing access token");

    let response: TokenResponse = app
        .http()
        .post(format!("{}/auth/token", super::API_URL))
        .json(&GrantTokenRequest { refresh_token })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let creds =
        AuthCredentials::from_tokens(response.access_token.clone(), response.refresh_token)?;

    app.sync_auth().set_creds(Some(creds), app.db())?;

    Ok(response.access_token)
}
