//! Sign-up, login, logout, password reset and the Keeper Forge connection.

use super::{
    AccessError, Account, Arc, Deserialize, ForgeError, HeaderMap, Hub, IntoResponse, Json,
    LoginError, MIN_PASSWORD_LENGTH, Membership, PairingError, PairingState, Response,
    SESSION_LIFETIME_HOURS, SET_COOKIE, SignupError, State, StatusCode, TokenError, denied,
    denied_for, json, session_cookie, session_token, unavailable,
};

/// Argon2 uses about 19 MiB per hash, so only a few logins may hash at the same time; the rest
/// wait here instead of allocating memory.
pub(super) static HASHING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(super) const SESSION_COOKIE: &str = "ck_session";

#[derive(Deserialize)]
pub(super) struct LoginBody {
    email: String,
    password: String,
}

pub(super) async fn login(State(hub): State<Arc<Hub>>, Json(body): Json<LoginBody>) -> Response {
    let Ok(_permit) = HASHING.acquire().await else {
        return unavailable();
    };
    // Hashing a password is slow on purpose, so it runs off the request threads.
    let result = tokio::task::spawn_blocking(move || hub.login(&body.email, &body.password)).await;
    match result {
        Ok(Ok((token, account))) => (
            [(
                SET_COOKIE,
                session_cookie(&token, SESSION_LIFETIME_HOURS * 3600),
            )],
            Json(json!({ "username": account.email, "email": account.email })),
        )
            .into_response(),
        Ok(Err(LoginError::BadCredentials)) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "wrong email or password" })),
        )
            .into_response(),
        Ok(Err(LoginError::NotVerified)) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "confirm your email first: open the link we mailed you" })),
        )
            .into_response(),
        Ok(Err(LoginError::TooManyAttempts)) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "too many wrong passwords, wait a few minutes" })),
        )
            .into_response(),
        Ok(Err(LoginError::Unavailable)) | Err(_) => unavailable(),
    }
}

#[derive(Deserialize)]
pub(super) struct RegisterBody {
    email: String,
    password: String,
}

#[derive(Deserialize)]
pub(super) struct EmailBody {
    email: String,
}

#[derive(Deserialize)]
pub(super) struct TokenBody {
    pub(super) token: String,
}

#[derive(Deserialize)]
pub(super) struct ResetBody {
    token: String,
    password: String,
}

pub(super) fn bad_request(text: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": text }))).into_response()
}

pub(super) fn no_mail() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "this Hub cannot send email, so this is closed" })),
    )
        .into_response()
}

pub(super) fn short_password() -> Response {
    bad_request(&format!(
        "the password needs at least {MIN_PASSWORD_LENGTH} characters"
    ))
}

pub(super) fn bad_link() -> Response {
    bad_request("this link is not valid any more")
}

/// The same answer whether or not the address has an account.
pub(super) fn accepted_mail(text: &str) -> Response {
    (StatusCode::ACCEPTED, Json(json!({ "status": text }))).into_response()
}

pub(super) async fn sign_up(
    State(hub): State<Arc<Hub>>,
    Json(body): Json<RegisterBody>,
) -> Response {
    let Ok(_permit) = HASHING.acquire().await else {
        return unavailable();
    };
    let result =
        tokio::task::spawn_blocking(move || hub.register_account(&body.email, &body.password))
            .await;
    match result {
        Ok(Ok(())) => accepted_mail("check your email"),
        Ok(Err(SignupError::BadEmail)) => bad_request("that is not an email address"),
        Ok(Err(SignupError::PasswordTooShort)) => short_password(),
        Ok(Err(SignupError::MailNotSetUp)) => no_mail(),
        Ok(Err(SignupError::Unavailable)) | Err(_) => unavailable(),
    }
}

pub(super) async fn verify_email(
    State(hub): State<Arc<Hub>>,
    Json(body): Json<TokenBody>,
) -> Response {
    match tokio::task::spawn_blocking(move || hub.verify_email(&body.token)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(TokenError::Invalid | TokenError::PasswordTooShort)) => bad_link(),
        Ok(Err(TokenError::Unavailable)) | Err(_) => unavailable(),
    }
}

pub(super) async fn password_reset(
    State(hub): State<Arc<Hub>>,
    Json(body): Json<EmailBody>,
) -> Response {
    match tokio::task::spawn_blocking(move || hub.request_password_reset(&body.email)).await {
        Ok(Ok(())) => accepted_mail("if that address has an account, a link is on its way"),
        Ok(Err(SignupError::MailNotSetUp)) => no_mail(),
        Ok(Err(_)) | Err(_) => unavailable(),
    }
}

pub(super) async fn password_reset_confirm(
    State(hub): State<Arc<Hub>>,
    Json(body): Json<ResetBody>,
) -> Response {
    let Ok(_permit) = HASHING.acquire().await else {
        return unavailable();
    };
    let result =
        tokio::task::spawn_blocking(move || hub.reset_password(&body.token, &body.password)).await;
    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(TokenError::Invalid)) => bad_link(),
        Ok(Err(TokenError::PasswordTooShort)) => short_password(),
        Ok(Err(TokenError::Unavailable)) | Err(_) => unavailable(),
    }
}

pub(super) async fn logout(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        let _ = tokio::task::spawn_blocking(move || hub.logout(&token)).await;
    }
    // The browser is told to forget the cookie whether or not there was a session.
    (
        [(SET_COOKIE, session_cookie("", 0))],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

pub(super) async fn me(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    let Some(token) = session_token(&headers) else {
        return denied(denied_for(&AccessError::Unauthorized));
    };
    let result = tokio::task::spawn_blocking(move || {
        let account = hub.account(&token)?;
        let memberships = hub.organizations_of(&account)?;
        Ok::<_, AccessError>((account, memberships))
    })
    .await;
    match result {
        Ok(Ok((account, memberships))) => {
            Json(account_json(&account, &memberships)).into_response()
        }
        Ok(Err(reason)) => denied(denied_for(&reason)),
        Err(_) => denied(denied_for(&AccessError::Unavailable)),
    }
}

#[derive(Deserialize)]
pub(super) struct ForgeConnectBody {
    port: u16,
    state: String,
}

/// The signed-in admin agrees to connect Keeper Forge. The answer is the address on this PC that
/// the browser goes to next. The browser's cookie is the proof, so this is only reachable from
/// the Hub's own page.
pub(super) async fn forge_connect(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<ForgeConnectBody>,
) -> Response {
    let Some(token) = session_token(&headers) else {
        return denied(denied_for(&AccessError::Unauthorized));
    };
    let result =
        tokio::task::spawn_blocking(move || hub.forge_redirect(&token, body.port, &body.state))
            .await;
    match result {
        Ok(Ok(redirect)) => Json(json!({ "redirect": redirect })).into_response(),
        Ok(Err(ForgeError::Unauthorized)) => denied(denied_for(&AccessError::Unauthorized)),
        Ok(Err(ForgeError::BadRequest)) => bad_request("that is not a request from Keeper Forge"),
        Ok(Err(ForgeError::Unavailable)) | Err(_) => unavailable(),
    }
}

#[derive(Deserialize)]
pub(super) struct ForgeTokenBody {
    code: String,
}

/// Keeper Forge trades its code for a session. It answers with a token the forge keeps in memory
/// and sends as `Authorization: Bearer`.
pub(super) async fn forge_token(
    State(hub): State<Arc<Hub>>,
    Json(body): Json<ForgeTokenBody>,
) -> Response {
    match tokio::task::spawn_blocking(move || hub.forge_session(&body.code)).await {
        Ok(Ok((token, account))) => Json(json!({
            "token": token,
            "email": account.email,
            "expires_in": SESSION_LIFETIME_HOURS * 3600,
        }))
        .into_response(),
        Ok(Err(TokenError::Invalid | TokenError::PasswordTooShort)) => {
            bad_request("this code is not valid any more")
        }
        Ok(Err(TokenError::Unavailable)) | Err(_) => unavailable(),
    }
}

#[derive(Deserialize)]
pub(super) struct PollBody {
    poll_key: String,
}

#[derive(Deserialize)]
pub(super) struct AnswerBody {
    code: String,
    approve: bool,
}

fn pairing_failed(error: &PairingError) -> Response {
    match error {
        PairingError::Ended => (
            StatusCode::GONE,
            Json(json!({ "error": "that code is not waiting any more" })),
        )
            .into_response(),
        PairingError::TooManyAttempts => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "too many tries, wait a few minutes" })),
        )
            .into_response(),
        PairingError::Unauthorized => denied(denied_for(&AccessError::Unauthorized)),
        PairingError::Unavailable => unavailable(),
    }
}

/// Keeper Forge starts a pairing. Nobody is signed in yet; the code only becomes useful once an
/// admin agrees to it in the browser.
pub(super) async fn pairing_start(State(hub): State<Arc<Hub>>) -> Response {
    match tokio::task::spawn_blocking(move || hub.start_pairing()).await {
        Ok(Ok((code, poll_key))) => Json(json!({
            "code": code,
            "poll_key": poll_key,
            "expires_in": crate::forge::PAIRING_LIFETIME_SECONDS,
        }))
        .into_response(),
        Ok(Err(error)) => pairing_failed(&error),
        Err(_) => unavailable(),
    }
}

/// Keeper Forge asks whether the admin agreed. A session is handed out once.
pub(super) async fn pairing_poll(
    State(hub): State<Arc<Hub>>,
    Json(body): Json<PollBody>,
) -> Response {
    match tokio::task::spawn_blocking(move || hub.poll_pairing(&body.poll_key)).await {
        Ok(Ok(PairingState::Waiting)) => Json(json!({ "status": "waiting" })).into_response(),
        Ok(Ok(PairingState::Approved(token, account))) => Json(json!({
            "status": "approved",
            "token": token,
            "email": account.email,
            "expires_in": SESSION_LIFETIME_HOURS * 3600,
        }))
        .into_response(),
        Ok(Err(error)) => pairing_failed(&error),
        Err(_) => unavailable(),
    }
}

/// The signed-in admin agrees or refuses. The browser's cookie is the proof.
pub(super) async fn pairing_answer(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // The session is checked before the body is read, so a request without one is always a 401.
    let Some(token) = session_token(&headers) else {
        return denied(denied_for(&AccessError::Unauthorized));
    };
    let Ok(body) = serde_json::from_slice::<AnswerBody>(&body) else {
        return bad_request("send the code and whether you agree");
    };
    match tokio::task::spawn_blocking(move || hub.answer_pairing(&token, &body.code, body.approve))
        .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => pairing_failed(&error),
        Err(_) => unavailable(),
    }
}

pub(super) fn account_json(account: &Account, memberships: &[Membership]) -> serde_json::Value {
    let organizations: Vec<_> = memberships
        .iter()
        .map(|m| {
            json!({
                "slug": m.organization.slug,
                "name": m.organization.name,
                "role": m.role.as_str(),
            })
        })
        .collect();
    let mut answer = json!({
        "username": account.email,
        "email": account.email,
        "organizations": organizations,
    });
    if account.staff {
        answer["staff"] = json!(true);
    }
    // The role of an account with one organization, as the page and the admin tool read it today.
    if let [only] = memberships {
        answer["role"] = json!(only.role.as_str());
    }
    answer
}
