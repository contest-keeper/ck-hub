use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use ck_hub::{
    Hub, Mailer, MemoryMailer, MemoryStore, PasswordCost, Registry, Role, Store, router,
    token_after,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

struct Setup {
    hub: Arc<Hub>,
    store: Arc<MemoryStore>,
    mailer: Arc<MemoryMailer>,
    app: Router,
}

fn setup(with_mail: bool) -> Setup {
    let now = Arc::new(Mutex::new(
        "2026-11-15T10:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("time"),
    ));
    let clock = Box::new(move || *now.lock().expect("clock"));
    let store = Arc::new(MemoryStore::default());
    let mailer = Arc::new(MemoryMailer::default());
    let mut hub = Hub::new(
        Registry::empty(),
        Arc::clone(&store) as Arc<dyn Store>,
        None,
        clock,
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    if with_mail {
        hub = hub.with_mail(
            Arc::clone(&mailer) as Arc<dyn Mailer>,
            "https://hub.example/",
        );
    }
    hub.create_user("owner@example.org", PASSWORD)
        .expect("owner");
    hub.create_organization("rcpc", "RCPC", "owner@example.org")
        .expect("organization");
    let hub = Arc::new(hub);
    Setup {
        app: router(Arc::clone(&hub)),
        hub,
        store,
        mailer,
    }
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let body = body.map_or_else(Body::empty, |b| {
        request = std::mem::take(&mut request).header("content-type", "application/json");
        Body::from(b.to_string())
    });
    let response = app
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

impl Setup {
    fn cookie(&self, email: &str) -> String {
        if self.hub.login(email, PASSWORD).is_err() {
            self.hub.create_user(email, PASSWORD).expect("user");
        }
        format!(
            "ck_session={}",
            self.hub.login(email, PASSWORD).expect("login").0
        )
    }
}

#[tokio::test]
async fn an_invitation_travels_from_the_owner_through_the_mail_to_a_new_member() {
    let s = setup(true);
    let owner = s.cookie("owner@example.org");
    let (status, _) = call(
        &s.app,
        "POST",
        "/api/v1/orgs/rcpc/invitations",
        Some(&owner),
        Some(json!({"email": "guest@example.org", "role": "viewer"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, list) = call(
        &s.app,
        "GET",
        "/api/v1/orgs/rcpc/invitations",
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(list[0]["email"], "guest@example.org");
    assert_eq!(list[0]["role"], "viewer");
    assert!(list[0].get("token_hash").is_none());

    let mail = s.mailer.last_to("guest@example.org").expect("mail");
    let token = token_after(&mail.body, "#invite=").expect("token");
    let wrong = s.cookie("other@example.org");
    let accept = |cookie: Option<String>, token: String| {
        let app = s.app.clone();
        async move {
            call(
                &app,
                "POST",
                "/api/v1/invitations/accept",
                cookie.as_deref(),
                Some(json!({ "token": token })),
            )
            .await
        }
    };
    assert_eq!(
        accept(None, token.clone()).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        accept(Some(wrong), token.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    let guest = s.cookie("guest@example.org");
    let (status, body) = accept(Some(guest.clone()), token.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["organization"]["slug"], "rcpc");
    assert_eq!(body["role"], "viewer");
    assert_eq!(
        accept(Some(guest.clone()), token).await.0,
        StatusCode::BAD_REQUEST
    );

    // A viewer may look at the sticks but not at the people.
    let (status, _) = call(
        &s.app,
        "GET",
        "/api/v1/orgs/rcpc/members",
        Some(&guest),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, members) = call(
        &s.app,
        "GET",
        "/api/v1/orgs/rcpc/members",
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(members.as_array().expect("list").len(), 2);
}

#[tokio::test]
async fn the_invitation_routes_refuse_what_the_roles_do_not_allow() {
    let s = setup(true);
    let owner = s.cookie("owner@example.org");
    let invite = |cookie: Option<String>, path: &'static str, body: Value| {
        let app = s.app.clone();
        async move {
            call(&app, "POST", path, cookie.as_deref(), Some(body))
                .await
                .0
        }
    };
    let path = "/api/v1/orgs/rcpc/invitations";
    assert_eq!(
        invite(
            None,
            path,
            json!({"email": "a@example.org", "role": "viewer"})
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        invite(
            Some(owner.clone()),
            path,
            json!({"email": "a@example.org", "role": "owner"})
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        invite(
            Some(owner.clone()),
            path,
            json!({"email": "a@example.org", "role": "boss"})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        invite(
            Some(owner.clone()),
            path,
            json!({"email": "nope", "role": "viewer"})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let stranger = s.cookie("stranger@example.org");
    assert_eq!(
        invite(
            Some(stranger),
            path,
            json!({"email": "a@example.org", "role": "viewer"})
        )
        .await,
        StatusCode::NOT_FOUND
    );
    let closed = setup(false);
    let cookie = closed.cookie("owner@example.org");
    let (status, _) = call(
        &closed.app,
        "POST",
        path,
        Some(&cookie),
        Some(json!({"email": "a@example.org", "role": "viewer"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn members_and_invitations_are_removed_over_http() {
    let s = setup(true);
    let owner = s.cookie("owner@example.org");
    s.hub.create_user("bo@example.org", PASSWORD).expect("bo");
    s.hub
        .add_member("rcpc", "bo@example.org", Role::Viewer)
        .expect("member");
    let (_, members) = call(
        &s.app,
        "GET",
        "/api/v1/orgs/rcpc/members",
        Some(&owner),
        None,
    )
    .await;
    let id = |email: &str| {
        members
            .as_array()
            .expect("list")
            .iter()
            .find(|m| m["email"] == email)
            .expect("member")["user_id"]
            .as_str()
            .expect("id")
            .to_owned()
    };
    let delete = |path: String| {
        let app = s.app.clone();
        let owner = owner.clone();
        async move { call(&app, "DELETE", &path, Some(&owner), None).await.0 }
    };
    assert_eq!(
        delete(format!(
            "/api/v1/orgs/rcpc/members/{}",
            id("owner@example.org")
        ))
        .await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        delete(format!(
            "/api/v1/orgs/rcpc/members/{}",
            id("bo@example.org")
        ))
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete("/api/v1/orgs/rcpc/members/nobody".to_owned()).await,
        StatusCode::NOT_FOUND
    );
    call(
        &s.app,
        "POST",
        "/api/v1/orgs/rcpc/invitations",
        Some(&owner),
        Some(json!({"email": "x@example.org", "role": "viewer"})),
    )
    .await;
    let (_, list) = call(
        &s.app,
        "GET",
        "/api/v1/orgs/rcpc/invitations",
        Some(&owner),
        None,
    )
    .await;
    let invitation = list[0]["id"].as_str().expect("id").to_owned();
    assert_eq!(
        delete(format!("/api/v1/orgs/rcpc/invitations/{invitation}")).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete(format!("/api/v1/orgs/rcpc/invitations/{invitation}")).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn staff_routes_are_hidden_from_everyone_else() {
    let s = setup(true);
    let normal = s.cookie("owner@example.org");
    let body = json!({"slug": "club", "name": "Club", "owner_email": "first@example.org"});
    let (status, _) = call(
        &s.app,
        "POST",
        "/api/v1/staff/organizations",
        Some(&normal),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &s.app,
        "GET",
        "/api/v1/staff/organizations",
        Some(&normal),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&s.app, "GET", "/api/v1/staff/organizations", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let staff = s.cookie("staff@example.org");
    let (_, me) = call(&s.app, "GET", "/api/v1/me", Some(&staff), None).await;
    assert!(me.get("staff").is_none());
    let id = s
        .hub
        .account(staff.trim_start_matches("ck_session="))
        .expect("account")
        .id;
    s.store.set_staff(&id, true).expect("staff");
    let (_, me) = call(&s.app, "GET", "/api/v1/me", Some(&staff), None).await;
    assert_eq!(me["staff"], true);

    let (status, _) = call(
        &s.app,
        "POST",
        "/api/v1/staff/organizations",
        Some(&staff),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = call(
        &s.app,
        "POST",
        "/api/v1/staff/organizations",
        Some(&staff),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &s.app,
        "POST",
        "/api/v1/staff/organizations",
        Some(&staff),
        Some(json!({"slug": "Bad Slug", "name": "x", "owner_email": "a@example.org"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, all) = call(
        &s.app,
        "GET",
        "/api/v1/staff/organizations",
        Some(&staff),
        None,
    )
    .await;
    assert_eq!(all.as_array().expect("list").len(), 2);
    assert!(s.mailer.last_to("first@example.org").is_some());
}
