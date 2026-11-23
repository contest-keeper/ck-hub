//! Invitations, members and the staff routes for making organizations.

use super::{
    AcceptError, AccessError, Arc, Deserialize, HeaderMap, Hub, IntoResponse, InviteError, Json,
    MemberError, Path, Response, Role, StaffError, State, StatusCode, TokenBody, accepted_mail,
    bad_link, bad_request, denied, denied_for, error_text, json, no_mail, rfc3339, with_session,
};

#[derive(Deserialize)]
pub(super) struct InviteBody {
    email: String,
    role: String,
}

#[derive(Deserialize)]
pub(super) struct StaffOrganizationBody {
    slug: String,
    name: String,
    owner_email: String,
}

pub(super) fn member_denied(error: &MemberError) -> Response {
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

pub(super) async fn invite(
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
        Err(reason) => denied(reason),
        Ok(Ok(())) => accepted_mail("invitation sent"),
        Ok(Err(InviteError::NoMail)) => no_mail(),
        Ok(Err(InviteError::BadEmail)) => bad_request("that is not an email address"),
        Ok(Err(InviteError::Unauthorized)) => denied(denied_for(&AccessError::Unauthorized)),
        Ok(Err(InviteError::Forbidden)) => denied(denied_for(&AccessError::Forbidden)),
        Ok(Err(InviteError::NotFound)) => denied(denied_for(&AccessError::NotFound)),
        Ok(Err(InviteError::Unavailable)) => denied(denied_for(&AccessError::Unavailable)),
    }
}

pub(super) async fn invitations_of(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.invitations_of(token, &slug)
    })
    .await;
    match result {
        Err(reason) => denied(reason),
        Ok(Ok(list)) => Json(
            list.iter()
                .map(|i| {
                    json!({
                        "id": i.id,
                        "email": i.email,
                        "role": i.role.as_str(),
                        "expires_at": rfc3339(i.expires_at),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(error)) => member_denied(&error),
    }
}

pub(super) async fn revoke_invitation(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.revoke_invitation(token, &slug, &id)
    })
    .await;
    match result {
        Err(reason) => denied(reason),
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => member_denied(&error),
    }
}

pub(super) async fn members_of(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.members_of(token, &slug)
    })
    .await;
    match result {
        Err(reason) => denied(reason),
        Ok(Ok(list)) => Json(
            list.iter()
                .map(|m| json!({ "user_id": m.user_id, "email": m.email, "role": m.role.as_str() }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(error)) => member_denied(&error),
    }
}

pub(super) async fn remove_member(
    State(hub): State<Arc<Hub>>,
    Path((slug, user_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.remove_member(token, &slug, &user_id)
    })
    .await;
    match result {
        Err(reason) => denied(reason),
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => member_denied(&error),
    }
}

pub(super) async fn accept_invitation(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<TokenBody>,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.accept_invitation(token, &body.token)
    })
    .await;
    match result {
        Err(reason) => denied(reason),
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
pub(super) fn staff_denied(error: &StaffError) -> Response {
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

pub(super) async fn staff_create_organization(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<StaffOrganizationBody>,
) -> Response {
    let result = with_session(hub, &headers, move |hub, token| {
        hub.staff_create_organization(token, &body.slug, &body.name, &body.owner_email)
    })
    .await;
    match result {
        Err(reason) => denied(reason),
        Ok(Ok(_)) => accepted_mail("the organization was made and its owner invited"),
        Ok(Err(error)) => staff_denied(&error),
    }
}

pub(super) async fn staff_organizations(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
) -> Response {
    let result = with_session(hub, &headers, Hub::staff_organizations).await;
    match result {
        Err(reason) => denied(reason),
        Ok(Ok(list)) => Json(
            list.iter()
                .map(|o| json!({ "slug": o.slug, "name": o.name }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(error)) => staff_denied(&error),
    }
}
