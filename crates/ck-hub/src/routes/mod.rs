//! The HTTP side: turns requests into calls on the [`Hub`] and the Hub's answers into responses.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use ck_protocol::{HEARTBEAT_PATH, SIGNATURE_HEADER, STICK_HEADER};
use serde::Deserialize;
use serde_json::json;

use crate::accounts::{
    Access, AccessError, Account, LoginError, MIN_PASSWORD_LENGTH, SESSION_LIFETIME_HOURS,
};
use crate::commands::CommandError;
use crate::enrol::{EnrolError, EnrolmentSpec};
use crate::forge::{ForgeError, PairingError, PairingState};
use crate::hub::{Hub, Rejection};
use crate::invite::{AcceptError, InviteError, MemberError, StaffError};
use crate::profiles::{
    MAX_SETTINGS_BYTES, MAX_WALLPAPER_BYTES, ProfileError, ProfileInput, content_type,
};
use crate::recordings::{CHUNK_BYTES, RecordingError, UploadStatus};
use crate::register::{NewStick, RegisterError};
use crate::signing::SignError;
use crate::signup::{SignupError, TokenError};
use crate::store::{Membership, Role};

mod accounts;
mod commands;
mod downloads;
mod enrolment;
mod organizations;
mod profiles;
mod recordings;
mod reports;
mod sticks;

use accounts::{
    SESSION_COOKIE, TokenBody, accepted_mail, bad_link, bad_request, forge_connect, forge_token,
    login, logout, me, no_mail, pairing_answer, pairing_poll, pairing_start, password_reset,
    password_reset_confirm, sign_up, verify_email,
};
use commands::{commands_of, one_command, send_command_in};
use downloads::{download_forge, download_image, download_source_offer, list_images};
use enrolment::{
    MAX_SIGN_REQUEST_BYTES, claim_enrolment, config_hash_in, config_signing_key, enrol,
    hardware_write_in, mint_code_in, sign_config_in,
};
use organizations::{
    accept_invitation, invitations_of, invite, members_of, remove_member, revoke_invitation,
    staff_create_organization, staff_organizations,
};
use profiles::{list_profiles, profile_wallpaper, read_profile, remove_profile, save_profile};
use recordings::{
    complete_upload, put_chunk, recordings_of, start_upload, upload_status, view_recording,
};
use reports::{event, heartbeat};
use sticks::{
    MAX_REGISTRY_BYTES, archive_stick, contest_submissions_of, delete_stick, network_of,
    register_in, restore_stick, retire_credential_in, sticks_of, submissions_of,
};

/// The page organizers watch. It is one file with no outside resources, so it works on a network
/// with no internet.
const PAGE: &str = include_str!("../page/index.html");
const NOT_FOUND_PAGE: &str = include_str!("../page/not_found.html");
const THEME_JS: &str = include_str!("../page/theme.js");
const APP_JS: &str = include_str!("../page/app.js");
const APP_CSS: &str = include_str!("../page/app.css");

/// Where sticks send events. The stick's agent uses the same address.
const EVENTS_PATH: &str = "/api/v1/events";

/// A heartbeat is a few hundred bytes. Anything much bigger is not one.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Every address the Hub answers, in one list.
#[allow(clippy::too_many_lines)]
pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/assets/theme.js", get(theme_js))
        .route("/assets/app.js", get(app_js))
        .route("/assets/app.css", get(app_css))
        .route("/forge/connect", get(page))
        .route("/people", get(page))
        .route("/download/forge", get(download_forge))
        .route("/download/image", get(page))
        .route("/download/image/{id}", get(download_image))
        .route(
            "/download/image/{id}/source-offer",
            get(download_source_offer),
        )
        .route("/api/v1/images", get(list_images))
        .route(HEARTBEAT_PATH, post(heartbeat))
        .route(EVENTS_PATH, post(event))
        .route(
            "/api/v1/orgs/{slug}/sticks",
            get(sticks_of)
                .post(register_in)
                .layer(DefaultBodyLimit::max(MAX_REGISTRY_BYTES)),
        )
        .route(
            "/api/v1/orgs/{slug}/sticks/{id}/credential",
            delete(retire_credential_in),
        )
        .route(
            "/api/v1/orgs/{slug}/sticks/{id}/archive",
            post(archive_stick),
        )
        .route(
            "/api/v1/orgs/{slug}/sticks/{id}/restore",
            post(restore_stick),
        )
        .route("/api/v1/orgs/{slug}/sticks/{id}", delete(delete_stick))
        .route(
            "/api/v1/orgs/{slug}/invitations",
            get(invitations_of).post(invite),
        )
        .route(
            "/api/v1/orgs/{slug}/invitations/{id}",
            delete(revoke_invitation),
        )
        .route(
            "/api/v1/orgs/{slug}/sticks/{id}/commands",
            get(commands_of).post(send_command_in),
        )
        .route(
            "/api/v1/orgs/{slug}/commands/{command_id}",
            get(one_command),
        )
        .route("/api/v1/recordings", post(start_upload))
        .route("/api/v1/recordings/{id}", get(upload_status))
        .route(
            "/api/v1/recordings/{id}/chunks/{index}",
            axum::routing::put(put_chunk),
        )
        .route("/api/v1/recordings/{id}/complete", post(complete_upload))
        .route(
            "/api/v1/orgs/{slug}/sticks/{id}/recordings",
            get(recordings_of),
        )
        .route(
            "/api/v1/orgs/{slug}/recordings/{recording_id}/view",
            get(view_recording),
        )
        .route("/api/v1/config-signing-key", get(config_signing_key))
        .route(
            "/api/v1/orgs/{slug}/config-signatures",
            post(sign_config_in).layer(DefaultBodyLimit::max(MAX_SIGN_REQUEST_BYTES)),
        )
        .route("/api/v1/orgs/{slug}/config-hashes", post(config_hash_in))
        .route(
            "/api/v1/orgs/{slug}/hardware-writes",
            post(hardware_write_in),
        )
        .route("/api/v1/orgs/{slug}/enrolment-codes", post(mint_code_in))
        .route("/api/v1/enrol", post(enrol))
        .route("/api/v1/enrol/claim", post(claim_enrolment))
        .route(
            "/api/v1/orgs/{slug}/sticks/{id}/submissions",
            get(submissions_of),
        )
        .route(
            "/api/v1/orgs/{slug}/contests/{contest}/submissions",
            get(contest_submissions_of),
        )
        .route("/api/v1/orgs/{slug}/sticks/{id}/network", get(network_of))
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
        .route("/api/v1/forge/connect", post(forge_connect))
        .route("/api/v1/forge/token", post(forge_token))
        .route("/api/v1/forge/pairings", post(pairing_start))
        .route("/api/v1/forge/pairings/poll", post(pairing_poll))
        .route("/api/v1/forge/pairings/answer", post(pairing_answer))
        .route("/api/v1/profiles", get(list_profiles))
        .route(
            "/api/v1/profiles/{id}",
            get(read_profile).put(save_profile).delete(remove_profile),
        )
        .route("/api/v1/profiles/{id}/wallpaper", get(profile_wallpaper))
        .route("/api/v1/register", post(sign_up))
        .route("/api/v1/verify-email", post(verify_email))
        .route("/api/v1/password-reset", post(password_reset))
        .route(
            "/api/v1/password-reset/confirm",
            post(password_reset_confirm),
        )
        .fallback(unknown_address)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn(crate::csrf::guard))
        .layer(axum::middleware::map_response(security_headers))
        .with_state(hub)
}

/// The page loads its own script and style from this server and talks only to it, so nothing else
/// may load (no inline script or style either), and no other site may put it in a frame.
async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        "content-security-policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; \
         connect-src 'self'; frame-ancestors 'none'"
            .parse()
            .expect("a valid header"),
    );
    headers.insert("x-content-type-options", "nosniff".parse().expect("valid"));
    headers.insert("x-frame-options", "DENY".parse().expect("valid"));
    response
}

async fn page() -> impl IntoResponse {
    ([(CACHE_CONTROL, "no-store")], Html(PAGE))
}

/// An address the Hub does not know. A person who opened a page gets a page; the API and every
/// request that is not a plain page view keep the bare `404`.
async fn unknown_address(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    let api = uri.path() == "/api" || uri.path().starts_with("/api/");
    if api || !(method == axum::http::Method::GET || method == axum::http::Method::HEAD) {
        return StatusCode::NOT_FOUND.into_response();
    }
    not_found_page()
}

/// The page for a person who followed a link to something that is not there.
fn not_found_page() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(CACHE_CONTROL, "no-store")],
        Html(NOT_FOUND_PAGE),
    )
        .into_response()
}

async fn theme_js() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", THEME_JS)
}

async fn app_js() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn app_css() -> impl IntoResponse {
    asset("text/css; charset=utf-8", APP_CSS)
}

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, content_type), (CACHE_CONTROL, "no-store")],
        body,
    )
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

fn error_text(status: StatusCode, text: &str) -> Response {
    (status, Json(json!({ "error": text }))).into_response()
}

/// Runs `work` with the session cookie's token on a thread where blocking is fine. No cookie is a 401.
async fn with_session<T: Send + 'static>(
    hub: Arc<Hub>,
    headers: &HeaderMap,
    work: impl FnOnce(&Hub, &str) -> T + Send + 'static,
) -> Result<T, Denied> {
    let Some(token) = session_token(headers) else {
        return Err(denied_for(&AccessError::Unauthorized));
    };
    tokio::task::spawn_blocking(move || work(&hub, &token))
        .await
        .map_err(|_| denied_for(&AccessError::Unavailable))
}

/// The cookie that carries a session. `HttpOnly` keeps scripts from reading it, `SameSite=Strict`
/// keeps other sites from using it, and `Secure` keeps it off plain HTTP, so run the Hub behind HTTPS.
fn session_cookie(token: &str, max_age_seconds: i64) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age_seconds}"
    )
}

/// The session of the request: the browser's cookie, or the bearer token Keeper Forge sends.
fn session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(bearer) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
    {
        return Some(bearer.to_owned());
    }
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

/// A time as the page and the admin tool read it: UTC, like 2026-11-12T10:00:00Z.
fn rfc3339(at: time::OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .expect("a stored time formats")
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

/// For the profile routes, which have no organization in the address: the caller's access to the only organization they belong to: 401 when there is no valid
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
