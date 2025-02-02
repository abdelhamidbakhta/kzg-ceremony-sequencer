use crate::{
    lobby::SharedLobbyState,
    oauth::{EthOAuthClient, GithubOAuthClient, SharedAuthState},
    sessions::IdToken,
    storage::{PersistentStorage, StorageError},
    EthAuthOptions, Options, SessionId, SessionInfo,
};
use axum::{
    extract::Query,
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::DateTime;
use http::StatusCode;
use oauth2::{
    reqwest::async_http_client, AuthorizationCode, CsrfToken, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::borrow::Cow;
use thiserror::Error;
use tokio::time::Instant;

// These are the providers that are supported
// via oauth
pub enum AuthProvider {
    Github,
    Ethereum,
}

impl AuthProvider {
    pub const fn to_string(&self) -> &str {
        match self {
            Self::Github => "Github",
            Self::Ethereum => "Ethereum",
        }
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("lobby is full")]
    LobbyIsFull,
    #[error("user already contributed")]
    UserAlreadyContributed,
    #[error("invalid csrf token")]
    InvalidCsrf,
    #[error("invalid auth code")]
    InvalidAuthCode,
    #[error("could not fetch user data from auth server")]
    FetchUserDataError,
    #[error("could not extract user data from auth server")]
    CouldNotExtractUserData,
    #[error("user created after deadline")]
    UserCreatedAfterDeadline,
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

pub struct UserVerified {
    id_token: IdToken,
    session_id: String,
}

pub struct AuthUrl {
    eth_auth_url: String,
    github_auth_url: String,
}

impl IntoResponse for AuthUrl {
    fn into_response(self) -> Response {
        Json(json!({
            "eth_auth_url": self.eth_auth_url,
            "github_auth_url": self.github_auth_url,
        }))
        .into_response()
    }
}

impl IntoResponse for UserVerified {
    fn into_response(self) -> Response {
        Json(json!({
            "id_token" : self.id_token,
            "session_id" : self.session_id,
        }))
        .into_response()
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::InvalidAuthCode => {
                let body = Json(json!({
                    "error": "invalid authorisation code",
                }));
                (StatusCode::BAD_REQUEST, body)
            }
            Self::FetchUserDataError => {
                let body = Json(json!({
                    "error": "could not fetch user data from auth server",
                }));
                (StatusCode::INTERNAL_SERVER_ERROR, body)
            }
            Self::CouldNotExtractUserData => {
                let body = Json(json!({
                    "error": "could not extract user data from auth server response",
                }));
                (StatusCode::INTERNAL_SERVER_ERROR, body)
            }
            Self::LobbyIsFull => {
                let body = Json(json!({
                    "error": "lobby is full",
                }));
                (StatusCode::SERVICE_UNAVAILABLE, body)
            }
            Self::InvalidCsrf => {
                let body = Json(json!({
                    "error": "invalid csrf token",
                }));
                (StatusCode::BAD_REQUEST, body)
            }
            Self::UserAlreadyContributed => {
                let body = Json(json!({ "error": "user has already contributed" }));
                (StatusCode::BAD_REQUEST, body)
            }
            Self::UserCreatedAfterDeadline => {
                let body = Json(json!({ "error": "user account was created after the deadline"}));
                (StatusCode::UNAUTHORIZED, body)
            }
            Self::Storage(storage_error) => return storage_error.into_response(),
        };
        (status, body).into_response()
    }
}

#[derive(Debug, Deserialize)]
pub struct AuthClientLinkQueryParams {
    redirect_to: Option<String>,
}

// Returns the url that the user needs to call
// in order to get an authorisation code
pub async fn auth_client_link(
    Query(params): Query<AuthClientLinkQueryParams>,
    Extension(options): Extension<Options>,
    Extension(auth_state): Extension<SharedAuthState>,
    Extension(lobby_state): Extension<SharedLobbyState>,
    Extension(eth_client): Extension<EthOAuthClient>,
    Extension(gh_client): Extension<GithubOAuthClient>,
) -> Result<AuthUrl, AuthError> {
    // Fist check if the lobby is full before giving users an auth link
    // Note: we use CSRF tokens, so just copying the url will not work either
    //
    {
        let lobby_size = lobby_state.read().await.participants.len();
        if lobby_size >= options.lobby.max_lobby_size {
            return Err(AuthError::LobbyIsFull);
        }
    }

    let csrf_token = CsrfToken::new_random();

    let redirect_uri = params
        .redirect_to
        .and_then(|uri| RedirectUrl::new(uri).ok()); // TODO: Error handling?

    let auth_request = eth_client
        .authorize_url(|| csrf_token)
        .add_scope(Scope::new("openid".to_string()));

    let redirected_auth_request = if let Some(redirect) = &redirect_uri {
        auth_request.set_redirect_uri(Cow::Borrowed(redirect))
    } else {
        auth_request
    };

    let (auth_url, csrf_token) = redirected_auth_request.url();

    let gh_auth_request = gh_client.client.authorize_url(|| csrf_token);
    let redirected_gh_auth_request = if let Some(redirect) = &redirect_uri {
        gh_auth_request.set_redirect_uri(Cow::Borrowed(redirect))
    } else {
        gh_auth_request
    };

    let (gh_url, csrf_token) = redirected_gh_auth_request.url();

    // Store CSRF token
    // TODO These should be cleaned periodically
    auth_state
        .write()
        .await
        .csrf_tokens
        .insert(csrf_token.secret().clone());

    Ok(AuthUrl {
        eth_auth_url: auth_url.to_string(),
        github_auth_url: gh_url.to_string(),
    })
}

// This is the payload that the client will send
// to the sequencer, that will be used to generate a JWT token.
// Since we are using oAUTH, this will contain the information
// that we need to check that the user did indeed login with
// an identity provider
#[derive(Debug, Deserialize)]
pub struct AuthPayload {
    code: String,
    state: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AuthenticatedUser {
    uid: String,
    nickname: String,
}

#[derive(Debug, Deserialize)]
struct GhUserInfo {
    login: String,
    created_at: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn github_callback(
    Query(payload): Query<AuthPayload>,
    Extension(options): Extension<Options>,
    Extension(auth_state): Extension<SharedAuthState>,
    Extension(lobby_state): Extension<SharedLobbyState>,
    Extension(storage): Extension<PersistentStorage>,
    Extension(gh_oauth_client): Extension<GithubOAuthClient>,
    Extension(http_client): Extension<reqwest::Client>,
) -> Result<UserVerified, AuthError> {
    verify_csrf(&payload, &auth_state).await?;
    let token = gh_oauth_client
        .exchange_code(AuthorizationCode::new(payload.code))
        .request_async(async_http_client)
        .await
        .map_err(|_| AuthError::InvalidAuthCode)?;

    let response = http_client
        .get(options.github.gh_userinfo_url)
        .bearer_auth(token.access_token().secret())
        .header("User-Agent", "ethereum-kzg-ceremony-sequencer")
        .send()
        .await
        .map_err(|_| AuthError::FetchUserDataError)?;
    let gh_user_info = response
        .json::<GhUserInfo>()
        .await
        .map_err(|_| AuthError::CouldNotExtractUserData)?;
    let creation_time = DateTime::parse_from_rfc3339(&gh_user_info.created_at)
        .map_err(|_| AuthError::CouldNotExtractUserData)?;
    if creation_time > options.github.gh_max_account_creation_time {
        return Err(AuthError::UserCreatedAfterDeadline);
    }
    let user = AuthenticatedUser {
        uid: format!("github | {}", gh_user_info.login),
        nickname: gh_user_info.login,
    };
    post_authenticate(auth_state, lobby_state, storage, user, AuthProvider::Github).await
}

#[derive(Debug, Deserialize)]
struct EthUserInfo {
    sub: String,
    preferred_username: String,
}

// This endpoint allows one to consume an oAUTH authorisation code
//  and produce a JWT token
// So Sequencer could give out fake identities, we are trusting the sequencer
// to not do that.
//
// Now this is catchable by the client. They will clearly see that the sequencer
// was malicious. What can happen is sequencer can claim that someone
// participated when they did not. Is this Okay? Maybe that person can then just
// say they did not
#[allow(clippy::too_many_arguments)]
pub async fn eth_callback(
    Query(payload): Query<AuthPayload>,
    Extension(options): Extension<Options>,
    Extension(auth_state): Extension<SharedAuthState>,
    Extension(lobby_state): Extension<SharedLobbyState>,
    Extension(storage): Extension<PersistentStorage>,
    Extension(oauth_client): Extension<EthOAuthClient>,
    Extension(http_client): Extension<reqwest::Client>,
) -> Result<UserVerified, AuthError> {
    verify_csrf(&payload, &auth_state).await?;
    let token = oauth_client
        .exchange_code(AuthorizationCode::new(payload.code))
        .request_async(async_http_client)
        .await
        .map_err(|_| AuthError::InvalidAuthCode)?;

    let response = http_client
        .get(&options.ethereum.eth_userinfo_url)
        .bearer_auth(token.access_token().secret())
        .send()
        .await
        .map_err(|_| AuthError::FetchUserDataError)?;

    let eth_user = response
        .json::<EthUserInfo>()
        .await
        .map_err(|_| AuthError::CouldNotExtractUserData)?;

    let addr_parts: Vec<_> = eth_user.sub.split(':').collect();
    let address = (*addr_parts
        .get(2)
        .ok_or(AuthError::CouldNotExtractUserData)?)
    .to_string();

    let tx_count = get_tx_count(
        &address,
        &options.ethereum.eth_nonce_verification_block,
        &http_client,
        &options.ethereum,
    )
    .await
    .ok_or(AuthError::CouldNotExtractUserData)?;

    if tx_count < options.ethereum.eth_min_nonce {
        return Err(AuthError::UserCreatedAfterDeadline);
    }

    let user_data = AuthenticatedUser {
        uid: format!("eth | {}", address),
        nickname: eth_user.preferred_username,
    };

    post_authenticate(
        auth_state,
        lobby_state,
        storage,
        user_data,
        AuthProvider::Ethereum,
    )
    .await
}

// TODO: This has many failure modes and should return and eyre::Result.
async fn get_tx_count(
    address: &str,
    at_block: &str,
    client: &reqwest::Client,
    options: &EthAuthOptions,
) -> Option<u64> {
    let rpc_payload = json!({
        "id": 1,
        "jsonrpc": "2.0",
        "params": [&address, &at_block],
        "method": "eth_getTransactionCount"
    });

    let rpc_response = client
        .post(options.eth_rpc_url.get_secret())
        .json(&rpc_payload)
        .send()
        .await
        .ok()?;

    let rpc_response_json = rpc_response.json::<serde_json::Value>().await.ok()?;

    let rpc_result = rpc_response_json.get("result")?.as_str()?;

    u64::from_str_radix(rpc_result.trim_start_matches("0x"), 16).ok()
}

async fn verify_csrf(payload: &AuthPayload, store: &SharedAuthState) -> Result<(), AuthError> {
    let auth_state = store.read().await;
    if auth_state.csrf_tokens.contains(&payload.state) {
        Ok(())
    } else {
        Err(AuthError::InvalidCsrf)
    }
}

async fn post_authenticate(
    auth_state: SharedAuthState,
    lobby_state: SharedLobbyState,
    storage: PersistentStorage,
    user_data: AuthenticatedUser,
    auth_provider: AuthProvider,
) -> Result<UserVerified, AuthError> {
    // Check if they have already contributed
    match storage.has_contributed(&user_data.uid).await {
        Err(error) => return Err(AuthError::Storage(error)),
        Ok(true) => return Err(AuthError::UserAlreadyContributed),
        Ok(false) => (),
    }

    // Check if this user is already in the lobby
    // If so, we send them back their session id
    let session_id = {
        let mut state = auth_state.write().await;

        if let Some(session_id) = state.unique_id_session.get(&user_data.uid) {
            session_id.clone()
        } else {
            let id = SessionId::new();
            state
                .unique_id_session
                .insert(user_data.uid.clone(), id.clone());
            id
        }
    };

    let id_token = IdToken {
        sub: user_data.uid,
        provider: auth_provider.to_string().to_owned(),
        nickname: user_data.nickname,
        exp: u64::MAX,
    };

    {
        let mut lobby = lobby_state.write().await;
        lobby.participants.insert(
            session_id.clone(),
            SessionInfo {
                token: id_token.clone(),
                last_ping_time: Instant::now(),
                is_first_ping_attempt: true,
            },
        );
    }

    Ok(UserVerified {
        id_token,
        session_id: session_id.to_string(),
    })
}
