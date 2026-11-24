//! Enrolment codes, hardware writes and the signing of stick settings.

use super::{
    Access, Arc, Bytes, CACHE_CONTROL, Deserialize, EnrolError, EnrolmentSpec, HeaderMap, Hub,
    IntoResponse, Json, Path, Response, Role, SignError, State, StatusCode, denied, json,
    require_in, unavailable,
};

/// What an admin decides about the stick a code is made for.
#[derive(Deserialize, Default)]
pub(super) struct MintBody {
    #[serde(default)]
    team: String,
    #[serde(default)]
    site: String,
    #[serde(default)]
    contest: String,
    #[serde(default)]
    machine_number: Option<u32>,
    #[serde(default)]
    lifetime_minutes: Option<i64>,
    #[serde(default)]
    credential_scope: Option<crate::CredentialScope>,
    #[serde(default)]
    hardware: Option<crate::Hardware>,
}

pub(super) async fn mint_code_in(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => mint(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

pub(super) fn enrol_refusal(error: EnrolError) -> Response {
    let refuse =
        |status: StatusCode, why: String| (status, Json(json!({ "error": why }))).into_response();
    match error {
        EnrolError::BadRequest(why) => refuse(StatusCode::BAD_REQUEST, why),
        EnrolError::RateLimited => refuse(StatusCode::TOO_MANY_REQUESTS, error.to_string()),
        EnrolError::Invalid => refuse(StatusCode::UNAUTHORIZED, error.to_string()),
        EnrolError::Store(error) => {
            tracing::error!("{error}");
            unavailable()
        }
    }
}

/// What Forge says when it writes a disk for a stick whose id it already knows.
#[derive(Deserialize)]
pub(super) struct HardwareWriteBody {
    stick_id: String,
    #[serde(flatten)]
    hardware: crate::Hardware,
}

pub(super) async fn hardware_write_in(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => record_write(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

/// A settings file of the largest size the Hub signs, as base64 in a small JSON request. A bigger
/// request is refused before it is read; a file just over the limit gets a plain answer.
pub(super) const MAX_SIGN_REQUEST_BYTES: usize = 2 * 1024 * 1024;

/// The public key images trust to sign settings. It is public, so it needs no sign-in: the image
/// build fetches it.
pub(super) async fn config_signing_key(State(hub): State<Arc<Hub>>) -> Response {
    match hub.config_signing_public_key() {
        Some(key) => {
            Json(json!({ "key_id": key.key_id(), "public_key": key.to_base64() })).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            "this Hub has no settings signing key",
        )
            .into_response(),
    }
}

/// The settings file of a stick, as base64 so its bytes arrive exactly as they will be written.
#[derive(Deserialize)]
pub(super) struct SignBody {
    config: String,
}

pub(super) async fn sign_config_in(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => sign(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn sign(hub: Arc<Hub>, access: Access, body: Bytes) -> Response {
    use base64::Engine;
    let refuse =
        |status: StatusCode, why: &str| (status, Json(json!({ "error": why }))).into_response();
    let Ok(request) = serde_json::from_slice::<SignBody>(&body) else {
        return refuse(StatusCode::BAD_REQUEST, "the request cannot be read");
    };
    let Ok(config) = base64::engine::general_purpose::STANDARD.decode(request.config.trim()) else {
        return refuse(StatusCode::BAD_REQUEST, "the settings must be base64");
    };
    let result = tokio::task::spawn_blocking(move || {
        hub.sign_stick_settings(&access.organization.id, &access.account, &config)
    })
    .await;
    match result {
        Ok(Ok(signature)) => Json(json!({
            "signature": signature.to_file_text(),
            "key_id": signature.key_id(),
        }))
        .into_response(),
        Ok(Err(error)) => match error {
            SignError::NoKey => refuse(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
            SignError::BadRequest(why) => refuse(StatusCode::BAD_REQUEST, &why),
            SignError::RateLimited => refuse(StatusCode::TOO_MANY_REQUESTS, &error.to_string()),
            SignError::NotYours => refuse(StatusCode::CONFLICT, &error.to_string()),
            SignError::Store(error) => {
                tracing::error!("{error}");
                unavailable()
            }
        },
        Err(_) => unavailable(),
    }
}

/// What Forge says about the settings file it wrote for a stick.
#[derive(Deserialize)]
pub(super) struct ConfigHashBody {
    stick_id: String,
    sha256: String,
}

pub(super) async fn config_hash_in(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => record_config(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn record_config(hub: Arc<Hub>, access: Access, body: Bytes) -> Response {
    let Ok(note) = serde_json::from_slice::<ConfigHashBody>(&body) else {
        return (StatusCode::BAD_REQUEST, "the request cannot be read").into_response();
    };
    let Ok(stick) = ck_protocol::StickId::parse(&note.stick_id) else {
        return (StatusCode::BAD_REQUEST, "the stick id is not valid").into_response();
    };
    let result = tokio::task::spawn_blocking(move || {
        hub.record_config_hash(&access.organization.id, &stick, &note.sha256)
    })
    .await;
    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => enrol_refusal(error),
        Err(_) => unavailable(),
    }
}

pub(super) async fn record_write(hub: Arc<Hub>, access: Access, body: Bytes) -> Response {
    let Ok(write) = serde_json::from_slice::<HardwareWriteBody>(&body) else {
        return (StatusCode::BAD_REQUEST, "the request cannot be read").into_response();
    };
    let Ok(stick) = ck_protocol::StickId::parse(&write.stick_id) else {
        return (StatusCode::BAD_REQUEST, "the stick id is not valid").into_response();
    };
    let result = tokio::task::spawn_blocking(move || {
        hub.record_stick_write(&access.organization.id, &write.hardware, &stick)
    })
    .await;
    match result {
        Ok(Ok(count)) => Json(json!({ "write_count": count })).into_response(),
        Ok(Err(error)) => enrol_refusal(error),
        Err(_) => unavailable(),
    }
}

pub(super) async fn mint(hub: Arc<Hub>, access: Access, body: Bytes) -> Response {
    // An empty body is a code with nothing preset.
    let spec: MintBody = if body.is_empty() {
        MintBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(spec) => spec,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("the request cannot be read: {error}") })),
                )
                    .into_response();
            }
        }
    };
    let result = tokio::task::spawn_blocking(move || {
        hub.mint_enrolment_code(
            &access.organization.id,
            &access.account,
            EnrolmentSpec {
                team: spec.team,
                site: spec.site,
                contest: spec.contest,
                machine_number: spec.machine_number,
                lifetime_minutes: spec.lifetime_minutes,
                credential_scope: spec.credential_scope,
                hardware: spec.hardware,
            },
        )
    })
    .await;
    match result {
        Ok(Ok(minted)) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, "no-store")],
            Json(minted),
        )
            .into_response(),
        Ok(Err(error)) => enrol_refusal(error),
        Err(_) => unavailable(),
    }
}

#[derive(Deserialize)]
pub(super) struct EnrolBody {
    code: String,
}

/// A stick trades its one-time code for its id and secret. There is no login: the code is the proof.
/// The answer carries the secret, so it is never cached.
pub(super) async fn enrol(State(hub): State<Arc<Hub>>, body: Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<EnrolBody>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the request must be {\"code\": \"...\"}" })),
        )
            .into_response();
    };
    match tokio::task::spawn_blocking(move || hub.enrol(&body.code)).await {
        Ok(Ok(enrolment)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, "no-store")],
            Json(enrolment),
        )
            .into_response(),
        Ok(Err(error)) => enrol_refusal(error),
        Err(_) => unavailable(),
    }
}

#[derive(Deserialize)]
pub(super) struct ClaimEnrolmentBody {
    code: String,
    stick_id: String,
    token: String,
}

pub(super) async fn claim_enrolment(State(hub): State<Arc<Hub>>, body: Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<ClaimEnrolmentBody>(&body) else {
        return enrol_refusal(EnrolError::BadRequest(
            "the request must contain code, stick_id and token".to_owned(),
        ));
    };
    match tokio::task::spawn_blocking(move || {
        hub.claim_enrolment(&body.code, &body.stick_id, body.token)
    })
    .await
    {
        Ok(Ok(id)) => (
            [(CACHE_CONTROL, "no-store")],
            Json(json!({ "stick_id": id })),
        )
            .into_response(),
        Ok(Err(error)) => enrol_refusal(error),
        Err(_) => unavailable(),
    }
}
