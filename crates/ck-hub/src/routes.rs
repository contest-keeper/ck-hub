//! The HTTP side: turns requests into calls on the [`Hub`] and the Hub's answers into responses.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{CACHE_CONTROL, COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use ck_protocol::{HEARTBEAT_PATH, SIGNATURE_HEADER, STICK_HEADER};
use serde::Deserialize;
use serde_json::json;

use crate::accounts::{
    Access, AccessError, Account, LoginError, MIN_PASSWORD_LENGTH, SESSION_LIFETIME_HOURS,
};
use crate::hub::{Hub, Rejection};
use crate::invite::{AcceptError, InviteError, MemberError, StaffError};
use crate::register::{NewStick, RegisterError};
use crate::signup::{SignupError, TokenError};
use crate::store::{Membership, Role};

/// The page organizers watch. It is one file with no outside resources, so it works on a network
/// with no internet.
const PAGE: &str = include_str!("page.html");

/// Argon2 uses about 19 MiB per hash, so only a few logins may hash at the same time; the rest
/// wait here instead of allocating memory.
static HASHING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// The installer's place in the private container. The forge pipeline keeps the name the same.
const FORGE_INSTALLER: &str = "forge/keeper-forge-setup-x64.exe";

/// Where sticks send events. The stick's agent uses the same address.
const EVENTS_PATH: &str = "/api/v1/events";

/// A whole registry of a few hundred sticks. Only the admin-only registering route allows this much.
const MAX_REGISTRY_BYTES: usize = 2 * 1024 * 1024;

/// A heartbeat is a few hundred bytes. Anything much bigger is not one.
const MAX_BODY_BYTES: usize = 64 * 1024;

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/download/forge", get(download_forge))
        .route(HEARTBEAT_PATH, post(heartbeat))
        .route(EVENTS_PATH, post(event))
        .route(
            "/api/v1/sticks",
            get(sticks)
                .post(register)
                .layer(DefaultBodyLimit::max(MAX_REGISTRY_BYTES)),
        )
        .route(
            "/api/v1/orgs/{slug}/sticks",
            get(sticks_of)
                .post(register_in)
                .layer(DefaultBodyLimit::max(MAX_REGISTRY_BYTES)),
        )
        .route(
            "/api/v1/orgs/{slug}/invitations",
            get(invitations_of).post(invite),
        )
        .route(
            "/api/v1/orgs/{slug}/invitations/{id}",
            delete(revoke_invitation),
        )
        .route("/api/v1/orgs/{slug}/members", get(members_of))
        .route(
            "/api/v1/orgs/{slug}/members/{user_id}",
            delete(remove_member),
        )
        .route("/api/v1/invitations/accept", post(accept_invitation))
        .route(
            "/api/v1/staff/organizations",
            get(staff_organizations).post(staff_create_organization),
        )
        .route("/api/v1/login", post(login))
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/me", get(me))
        .route("/api/v1/register", post(sign_up))
        .route("/api/v1/verify-email", post(verify_email))
        .route("/api/v1/password-reset", post(password_reset))
        .route(
            "/api/v1/password-reset/confirm",
            post(password_reset_confirm),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::map_response(security_headers))
        .with_state(hub)
}

/// The page is one file with inline script and style and talks only to its own server, so nothing
/// else may load, and no other site may put it in a frame.
async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        "content-security-policy",
        "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
         connect-src 'self'; frame-ancestors 'none'"
            .parse()
            .expect("a valid header"),
    );
    headers.insert("x-content-type-options", "nosniff".parse().expect("valid"));
    headers.insert("x-frame-options", "DENY".parse().expect("valid"));
    response
}

/// The Keeper Forge installer. The visitor is sent to a read-only link that works for a few
/// minutes, so the storage address is never published and a copied link soon stops working.
async fn download_forge(State(hub): State<Arc<Hub>>) -> Response {
    let Some(download) = hub.download.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let link = download.link(FORGE_INSTALLER, (hub.clock)());
    (
        StatusCode::FOUND,
        [(LOCATION, link), (CACHE_CONTROL, "no-store".to_owned())],
    )
        .into_response()
}

async fn heartbeat(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    let (stick, signature) = auth_headers(&headers);
    // The store may talk to a database, which blocks. That belongs on a thread of its own, never
    // on the threads that serve requests.
    let result = tokio::task::spawn_blocking(move || {
        hub.accept_heartbeat(stick.as_deref(), signature.as_deref(), &body)
    })
    .await;
    accepted(result)
}

async fn event(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    let (stick, signature) = auth_headers(&headers);
    let result = tokio::task::spawn_blocking(move || {
        hub.accept_event(stick.as_deref(), signature.as_deref(), &body)
    })
    .await;
    accepted(result)
}

/// The answer to a heartbeat or an event. A 409 means the Hub already has it, which a stick counts
/// as sent.
fn accepted(result: Result<Result<u64, Rejection>, tokio::task::JoinError>) -> Response {
    match result {
        Ok(Ok(seq)) => Json(json!({ "accepted": seq })).into_response(),
        Ok(Err(Rejection::BadRequest(why))) => (StatusCode::BAD_REQUEST, why).into_response(),
        Ok(Err(Rejection::Unauthorized)) => {
            (StatusCode::UNAUTHORIZED, "unknown stick or bad signature").into_response()
        }
        Ok(Err(Rejection::Conflict(why))) => (StatusCode::CONFLICT, why).into_response(),
        Ok(Err(Rejection::TooManyRequests)) => {
            (StatusCode::TOO_MANY_REQUESTS, "too many messages").into_response()
        }
        Ok(Err(Rejection::Unavailable)) | Err(_) => unavailable(),
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

/// The sticks of the only organization the caller belongs to. The page and the admin tool used this
/// before organizations had addresses.
async fn sticks(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    match require(&hub, &headers, Role::Viewer).await {
        Ok(access) => list_sticks(hub, access).await,
        Err(reason) => denied(reason),
    }
}

async fn sticks_of(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => list_sticks(hub, access).await,
        Err(reason) => denied(reason),
    }
}

async fn list_sticks(hub: Arc<Hub>, access: Access) -> Response {
    match tokio::task::spawn_blocking(move || hub.sticks(&access.organization.id)).await {
        Ok(Ok(sticks)) => Json(sticks).into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

/// The stick keeps what it could not send and tries again later.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "the Hub cannot keep or read its data right now",
    )
        .into_response()
}

/// The stick id and the signature, as owned text so they can move to the blocking thread.
fn auth_headers(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    (get(STICK_HEADER), get(SIGNATURE_HEADER))
}

const SESSION_COOKIE: &str = "ck_session";

#[derive(Deserialize)]
struct LoginBody {
    // The page and the admin tool still send `username`.
    #[serde(alias = "username")]
    email: String,
    password: String,
}

async fn login(State(hub): State<Arc<Hub>>, Json(body): Json<LoginBody>) -> Response {
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
struct RegisterBody {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct EmailBody {
    email: String,
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

#[derive(Deserialize)]
struct ResetBody {
    token: String,
    password: String,
}

fn bad_request(text: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": text }))).into_response()
}

fn no_mail() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "this Hub cannot send email, so this is closed" })),
    )
        .into_response()
}

fn short_password() -> Response {
    bad_request(&format!(
        "the password needs at least {MIN_PASSWORD_LENGTH} characters"
    ))
}

fn bad_link() -> Response {
    bad_request("this link is not valid any more")
}

/// The same answer whether or not the address has an account.
fn accepted_mail(text: &str) -> Response {
    (StatusCode::ACCEPTED, Json(json!({ "status": text }))).into_response()
}

async fn sign_up(State(hub): State<Arc<Hub>>, Json(body): Json<RegisterBody>) -> Response {
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

async fn verify_email(State(hub): State<Arc<Hub>>, Json(body): Json<TokenBody>) -> Response {
    match tokio::task::spawn_blocking(move || hub.verify_email(&body.token)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(TokenError::Invalid | TokenError::PasswordTooShort)) => bad_link(),
        Ok(Err(TokenError::Unavailable)) | Err(_) => unavailable(),
    }
}

async fn password_reset(State(hub): State<Arc<Hub>>, Json(body): Json<EmailBody>) -> Response {
    match tokio::task::spawn_blocking(move || hub.request_password_reset(&body.email)).await {
        Ok(Ok(())) => accepted_mail("if that address has an account, a link is on its way"),
        Ok(Err(SignupError::MailNotSetUp)) => no_mail(),
        Ok(Err(_)) | Err(_) => unavailable(),
    }
}

async fn password_reset_confirm(
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

async fn logout(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
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

async fn me(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
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

fn account_json(account: &Account, memberships: &[Membership]) -> serde_json::Value {
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

#[derive(Deserialize)]
struct InviteBody {
    email: String,
    role: String,
}

#[derive(Deserialize)]
struct StaffOrganizationBody {
    slug: String,
    name: String,
    owner_email: String,
}

fn error_text(status: StatusCode, text: &str) -> Response {
    (status, Json(json!({ "error": text }))).into_response()
}

fn member_denied(error: &MemberError) -> Response {
    match error {
        MemberError::Unauthorized => denied(denied_for(&AccessError::Unauthorized)),
        MemberError::Forbidden => denied(denied_for(&AccessError::Forbidden)),
        MemberError::NotFound => denied(denied_for(&AccessError::NotFound)),
        MemberError::LastOwner => error_text(
            StatusCode::CONFLICT,
            "an organization keeps at least one owner",
        ),
        MemberError::Unavailable => denied(denied_for(&AccessError::Unavailable)),
    }
}

/// Runs `work` with the session cookie's token on a thread where blocking is fine. No cookie is a 401.
async fn with_session<T: Send + 'static>(
    hub: Arc<Hub>,
    headers: &HeaderMap,
    work: impl FnOnce(&Hub, &str) -> T + Send + 'static,
) -> Result<T, Response> {
    let Some(token) = session_token(headers) else {
        return Err(denied(denied_for(&AccessError::Unauthorized)));
    };
    tokio::task::spawn_blocking(move || work(&hub, &token))
        .await
        .map_err(|_| denied(denied_for(&AccessError::Unavailable)))
}

async fn invite(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<InviteBody>,
) -> Response {
    let Some(role) = Role::parse(&body.role) else {
        return bad_request("the role must be owner, admin or viewer");
    };
    let result = with_session(hub, &headers, move |hub, token| {
        hub.invite(token, &slug, &body.email, role)
    })
    .await;
    match result {
        Err(response) => response,
        Ok(Ok(())) => accepted_mail("invitation sent"),
        Ok(Err(InviteError::NoMail)) => no_mail(),
        Ok(Err(InviteError::BadEmail)) => bad_request("that is not an email address"),
        Ok(Err(InviteError::Unauthorized)) => denied(denied_for(&AccessError::Unauthorized)),
        Ok(Err(InviteError::Forbidden)) => denied(denied_for(&AccessError::Forbidden)),
        Ok(Err(InviteError::NotFound)) => denied(denied_for(&AccessError::NotFound)),
        Ok(Err(InviteError::Unavailable)) => denied(denied_for(&AccessError::Unavailable)),
    }
}

async fn invitations_of(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.invitations_of(token, &slug)
    })
    .await;
    match result {
        Err(response) => response,
        Ok(Ok(list)) => Json(
            list.iter()
                .map(|i| {
                    json!({
                        "id": i.id,
                        "email": i.email,
                        "role": i.role.as_str(),
                        "expires_at": i.expires_at,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(error)) => member_denied(&error),
    }
}

async fn revoke_invitation(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.revoke_invitation(token, &slug, &id)
    })
    .await;
    match result {
        Err(response) => response,
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => member_denied(&error),
    }
}

async fn members_of(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.members_of(token, &slug)
    })
    .await;
    match result {
        Err(response) => response,
        Ok(Ok(list)) => Json(
            list.iter()
                .map(|m| json!({ "user_id": m.user_id, "email": m.email, "role": m.role.as_str() }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(error)) => member_denied(&error),
    }
}

async fn remove_member(
    State(hub): State<Arc<Hub>>,
    Path((slug, user_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.remove_member(token, &slug, &user_id)
    })
    .await;
    match result {
        Err(response) => response,
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => member_denied(&error),
    }
}

async fn accept_invitation(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<TokenBody>,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.accept_invitation(token, &body.token)
    })
    .await;
    match result {
        Err(response) => response,
        Ok(Ok(membership)) => Json(json!({
            "organization": {
                "slug": membership.organization.slug,
                "name": membership.organization.name,
            },
            "role": membership.role.as_str(),
        }))
        .into_response(),
        Ok(Err(AcceptError::Unauthorized)) => denied(denied_for(&AccessError::Unauthorized)),
        Ok(Err(AcceptError::WrongAccount)) => error_text(
            StatusCode::FORBIDDEN,
            "this invitation is for another account: log in with the address it was sent to",
        ),
        Ok(Err(AcceptError::Invalid)) => bad_link(),
        Ok(Err(AcceptError::Unavailable)) => denied(denied_for(&AccessError::Unavailable)),
    }
}

/// Staff routes answer 404 to everyone else, so nobody learns they exist.
fn staff_denied(error: &StaffError) -> Response {
    match error {
        StaffError::Unauthorized => denied(denied_for(&AccessError::Unauthorized)),
        StaffError::Forbidden => StatusCode::NOT_FOUND.into_response(),
        StaffError::NoMail => no_mail(),
        StaffError::BadEmail => bad_request("that is not an email address"),
        StaffError::BadSlug => bad_request(
            "the slug must be lowercase letters, digits and hyphens, and the name must not be empty",
        ),
        StaffError::SlugTaken => error_text(
            StatusCode::CONFLICT,
            "an organization with that slug exists",
        ),
        StaffError::Unavailable => denied(denied_for(&AccessError::Unavailable)),
    }
}

async fn staff_create_organization(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<StaffOrganizationBody>,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.staff_create_organization(token, &body.slug, &body.name, &body.owner_email)
    })
    .await;
    match result {
        Err(response) => response,
        Ok(Ok(_)) => accepted_mail("the organization was made and its owner invited"),
        Ok(Err(error)) => staff_denied(&error),
    }
}

async fn staff_organizations(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    let result = with_session(hub, &headers, Hub::staff_organizations).await;
    match result {
        Err(response) => response,
        Ok(Ok(list)) => Json(
            list.iter()
                .map(|o| json!({ "slug": o.slug, "name": o.name }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(error)) => staff_denied(&error),
    }
}

/// The cookie that carries a session. `HttpOnly` keeps scripts from reading it, `SameSite=Strict`
/// keeps other sites from using it, and `Secure` keeps it off plain HTTP, so run the Hub behind HTTPS.
fn session_cookie(token: &str, max_age_seconds: i64) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age_seconds}"
    )
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    cookies
        .split(';')
        .filter_map(|cookie| {
            cookie
                .trim()
                .strip_prefix(SESSION_COOKIE)?
                .strip_prefix('=')
        })
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Why a request was not allowed: the status to answer with and a short text.
type Denied = (StatusCode, &'static str);

fn denied(denied: Denied) -> Response {
    (denied.0, Json(json!({ "error": denied.1 }))).into_response()
}

fn denied_for(reason: &AccessError) -> Denied {
    match reason {
        AccessError::Unauthorized => (StatusCode::UNAUTHORIZED, "log in first"),
        AccessError::NotFound => (StatusCode::NOT_FOUND, "no such organization"),
        AccessError::Forbidden => (StatusCode::FORBIDDEN, "this needs a different role"),
        AccessError::Ambiguous => (
            StatusCode::CONFLICT,
            "you belong to several organizations: use /api/v1/orgs/{slug}/...",
        ),
        AccessError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "the Hub cannot read its data right now",
        ),
    }
}

/// The caller's access to the only organization they belong to: 401 when there is no valid
/// session, 404 when there is none, 409 when there are several, 403 when the role is not enough,
/// 503 when the data cannot be read.
async fn require(hub: &Arc<Hub>, headers: &HeaderMap, needed: Role) -> Result<Access, Denied> {
    let Some(token) = session_token(headers) else {
        return Err(denied_for(&AccessError::Unauthorized));
    };
    let hub = Arc::clone(hub);
    match tokio::task::spawn_blocking(move || hub.access_sole(&token, needed)).await {
        Ok(result) => result.map_err(|reason| denied_for(&reason)),
        Err(_) => Err(denied_for(&AccessError::Unavailable)),
    }
}

/// The caller's access to the organization named in the address.
async fn require_in(
    hub: &Arc<Hub>,
    headers: &HeaderMap,
    slug: String,
    needed: Role,
) -> Result<Access, Denied> {
    let Some(token) = session_token(headers) else {
        return Err(denied_for(&AccessError::Unauthorized));
    };
    let hub = Arc::clone(hub);
    match tokio::task::spawn_blocking(move || hub.access(&token, &slug, needed)).await {
        Ok(result) => result.map_err(|reason| denied_for(&reason)),
        Err(_) => Err(denied_for(&AccessError::Unavailable)),
    }
}

/// One stick, or a whole registry file of the admin tool (`{"sticks": [...]}`). It is read by hand
/// instead of by an untagged type, so a mistake is reported with the name of the wrong field.
fn read_sticks(body: &[u8]) -> Result<Vec<NewStick>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("the request is not valid JSON: {e}"))?;
    let invalid = |e: serde_json::Error| format!("the request is not a stick or a registry: {e}");
    match value.get("sticks") {
        Some(list) => serde_json::from_value(list.clone()).map_err(invalid),
        None => serde_json::from_value(value)
            .map(|stick| vec![stick])
            .map_err(invalid),
    }
}

async fn register(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    match require(&hub, &headers, Role::Admin).await {
        Ok(access) => register_into(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

async fn register_in(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => register_into(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

async fn register_into(hub: Arc<Hub>, access: Access, body: Bytes) -> Response {
    let sticks = match read_sticks(&body) {
        Ok(sticks) => sticks,
        Err(why) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response();
        }
    };
    match tokio::task::spawn_blocking(move || hub.register_sticks(&access.organization.id, sticks))
        .await
    {
        Ok(Ok(done)) => {
            let answer = json!({
                "added": done.added,
                "already_known": done.already_known,
                "conflicts": done.conflicts,
            });
            // A different secret for a known stick is refused, but the rest was still registered.
            let status = if done.conflicts.is_empty() {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            (status, Json(answer)).into_response()
        }
        Ok(Err(RegisterError::Invalid(why))) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response()
        }
        Ok(Err(RegisterError::Unavailable)) | Err(_) => unavailable(),
    }
}
