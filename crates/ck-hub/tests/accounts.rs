use std::fs;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use ck_hub::{
    AccessError, CreateUserError, Hub, LoginError, MAX_FAILED_LOGINS, MemoryStore,
    NAME_LOCKOUT_SECONDS, PasswordCost, Registry, Role, SESSION_LIFETIME_HOURS, Store, router,
};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

struct Setup {
    _dir: TempDir,
    hub: Arc<Hub>,
    app: Router,
    now: Arc<Mutex<DateTime<Utc>>>,
}

fn setup() -> Setup {
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("registry.json"), r#"{"sticks":[]}"#).expect("registry");
    let now = Arc::new(Mutex::new(
        "2026-11-12T10:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("time"),
    ));
    let clock = {
        let now = Arc::clone(&now);
        Box::new(move || *now.lock().expect("clock"))
    };
    let hub = Hub::new(
        Registry::open(dir.path().join("registry.json")).expect("open"),
        Box::new(MemoryStore::default()),
        None,
        clock,
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    hub.create_user("ana", PASSWORD, Role::Admin)
        .expect("admin");
    hub.create_user("vera", PASSWORD, Role::Viewer)
        .expect("viewer");
    let hub = Arc::new(hub);
    Setup {
        _dir: dir,
        app: router(Arc::clone(&hub)),
        hub,
        now,
    }
}

async fn post_login(
    app: &Router,
    name: &str,
    password: &str,
) -> (StatusCode, Option<String>, String) {
    let request = Request::post("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "username": name, "password": password }).to_string(),
        ))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let cookie = response
        .headers()
        .get("set-cookie")
        .map(|v| v.to_str().expect("text").to_owned());
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, cookie, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().expect("cookie").to_owned()
}

#[tokio::test]
async fn the_page_is_public_but_the_data_and_the_session_need_a_login() {
    let setup = setup();
    assert_eq!(get(&setup.app, "/", None).await.0, StatusCode::OK);
    assert_eq!(
        get(&setup.app, "/api/v1/sticks", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&setup.app, "/api/v1/me", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A cookie that is not a session gets the same answer.
    let (status, _) = get(&setup.app, "/api/v1/sticks", Some("ck_session=made-up")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logging_in_gives_a_locked_down_cookie_that_opens_the_data_until_logout() {
    let setup = setup();
    let (status, cookie, body) = post_login(&setup.app, "vera", PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let set = cookie.expect("a cookie");
    for flag in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(set.contains(flag), "{flag} missing in {set}");
    }
    assert!(!body.contains(PASSWORD), "the password is never echoed");
    let pair = cookie_pair(&set);

    assert_eq!(
        get(&setup.app, "/api/v1/sticks", Some(&pair)).await.0,
        StatusCode::OK
    );
    let (_, me) = get(&setup.app, "/api/v1/me", Some(&pair)).await;
    assert!(me.contains("vera") && me.contains("viewer"), "{me}");

    let logout = Request::post("/api/v1/logout")
        .header("cookie", &pair)
        .body(Body::empty())
        .expect("request");
    let response = setup.app.clone().oneshot(logout).await.expect("response");
    assert!(
        response.headers()["set-cookie"]
            .to_str()
            .expect("text")
            .contains("Max-Age=0")
    );
    assert_eq!(
        get(&setup.app, "/api/v1/sticks", Some(&pair)).await.0,
        StatusCode::UNAUTHORIZED,
        "the session is gone, not only the cookie"
    );
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_name_get_the_same_answer() {
    let setup = setup();
    let wrong = post_login(&setup.app, "vera", "not the password at all").await;
    let unknown = post_login(&setup.app, "nobody", "not the password at all").await;
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        (wrong.0, &wrong.1, &wrong.2),
        (unknown.0, &unknown.1, &unknown.2)
    );
    assert!(wrong.1.is_none(), "no cookie");
}

#[test]
fn a_role_that_is_not_enough_is_forbidden() {
    let setup = setup();
    let (viewer, _) = setup.hub.login("vera", PASSWORD).expect("viewer");
    let (admin, _) = setup.hub.login("ana", PASSWORD).expect("admin");
    assert!(setup.hub.access(&viewer, Role::Viewer).is_ok());
    assert_eq!(
        setup.hub.access(&viewer, Role::Admin),
        Err(AccessError::Forbidden)
    );
    assert!(setup.hub.access(&admin, Role::Admin).is_ok());
    assert!(
        setup.hub.access(&admin, Role::Viewer).is_ok(),
        "an admin sees what a viewer sees"
    );
    assert_eq!(
        setup.hub.access("made-up", Role::Viewer),
        Err(AccessError::Unauthorized)
    );
}

#[test]
fn a_session_ends_after_its_lifetime() {
    let setup = setup();
    let (token, _) = setup.hub.login("vera", PASSWORD).expect("login");
    *setup.now.lock().expect("clock") +=
        Duration::hours(SESSION_LIFETIME_HOURS) - Duration::seconds(1);
    assert!(setup.hub.access(&token, Role::Viewer).is_ok());
    *setup.now.lock().expect("clock") += Duration::seconds(1);
    assert_eq!(
        setup.hub.access(&token, Role::Viewer),
        Err(AccessError::Unauthorized)
    );
}

#[test]
fn too_many_wrong_passwords_lock_the_name_for_a_while_even_for_the_right_password() {
    let setup = setup();
    for _ in 0..MAX_FAILED_LOGINS {
        assert_eq!(
            setup.hub.login("vera", "wrong wrong wrong").err(),
            Some(LoginError::BadCredentials)
        );
    }
    assert_eq!(
        setup.hub.login("vera", PASSWORD).err(),
        Some(LoginError::TooManyAttempts)
    );
    // Another name is not affected.
    assert!(setup.hub.login("ana", PASSWORD).is_ok());

    *setup.now.lock().expect("clock") += Duration::seconds(NAME_LOCKOUT_SECONDS);
    assert!(setup.hub.login("vera", PASSWORD).is_ok(), "the lock ends");
}

#[tokio::test]
async fn the_lock_is_a_429_over_http() {
    let setup = setup();
    for _ in 0..MAX_FAILED_LOGINS {
        post_login(&setup.app, "vera", "wrong wrong wrong").await;
    }
    assert_eq!(
        post_login(&setup.app, "vera", PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[test]
fn accounts_are_checked_when_made_and_only_hashes_are_kept() {
    let setup = setup();
    assert_eq!(
        setup.hub.create_user("ana", PASSWORD, Role::Viewer),
        Err(CreateUserError::NameTaken)
    );
    assert_eq!(
        setup.hub.create_user("bo", "short", Role::Viewer),
        Err(CreateUserError::PasswordTooShort)
    );
    assert_eq!(
        setup.hub.create_user("  ", PASSWORD, Role::Viewer),
        Err(CreateUserError::BadName)
    );
    assert_eq!(
        setup.hub.create_user("a\nb", PASSWORD, Role::Viewer),
        Err(CreateUserError::BadName)
    );

    // The name taken stays what it was: the refused second account did not replace the first.
    assert!(setup.hub.login("ana", PASSWORD).is_ok());
}

/// The same store, seen from the Hub and from the test.
struct Shared(Arc<MemoryStore>);
impl Store for Shared {
    fn record_heartbeat(
        &self,
        h: ck_protocol::Heartbeat,
        t: DateTime<Utc>,
    ) -> Result<(), ck_hub::StoreError> {
        self.0.record_heartbeat(h, t)
    }
    fn last_heartbeat(
        &self,
        s: &ck_protocol::StickId,
    ) -> Result<Option<ck_hub::SeenHeartbeat>, ck_hub::StoreError> {
        self.0.last_heartbeat(s)
    }
    fn append_event(
        &self,
        e: ck_protocol::Event,
        t: DateTime<Utc>,
        i: Option<String>,
    ) -> Result<(), ck_hub::StoreError> {
        self.0.append_event(e, t, i)
    }
    fn events(
        &self,
        s: &ck_protocol::StickId,
    ) -> Result<Vec<ck_hub::StoredEvent>, ck_hub::StoreError> {
        self.0.events(s)
    }
    fn create_user(&self, u: ck_hub::User) -> Result<bool, ck_hub::StoreError> {
        self.0.create_user(u)
    }
    fn user(&self, n: &str) -> Result<Option<ck_hub::User>, ck_hub::StoreError> {
        self.0.user(n)
    }
    fn create_session(&self, h: &str, u: &str, e: DateTime<Utc>) -> Result<(), ck_hub::StoreError> {
        self.0.create_session(h, u, e)
    }
    fn session(&self, h: &str) -> Result<Option<(String, DateTime<Utc>)>, ck_hub::StoreError> {
        self.0.session(h)
    }
    fn add_stick(&self, s: ck_hub::Stick) -> Result<bool, ck_hub::StoreError> {
        self.0.add_stick(s)
    }
    fn stick(
        &self,
        id: &ck_protocol::StickId,
    ) -> Result<Option<ck_hub::Stick>, ck_hub::StoreError> {
        self.0.stick(id)
    }
    fn sticks(&self) -> Result<Vec<ck_hub::Stick>, ck_hub::StoreError> {
        self.0.sticks()
    }
    fn delete_session(&self, h: &str) -> Result<(), ck_hub::StoreError> {
        self.0.delete_session(h)
    }
}

#[test]
fn neither_the_password_nor_the_session_token_is_what_the_store_holds() {
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("registry.json"), r#"{"sticks":[]}"#).expect("registry");
    let store = Arc::new(MemoryStore::default());
    let hub = Hub::new(
        Registry::open(dir.path().join("registry.json")).expect("open"),
        Box::new(Shared(Arc::clone(&store))),
        None,
        Box::new(Utc::now),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    hub.create_user("vera", PASSWORD, Role::Viewer)
        .expect("user");
    let (token, _) = hub.login("vera", PASSWORD).expect("login");

    let user = store.user("vera").expect("read").expect("a user");
    assert!(
        user.password_hash.starts_with("$argon2id$"),
        "{}",
        user.password_hash
    );
    assert!(!user.password_hash.contains(PASSWORD));
    // The store knows sessions by a hash of the token, so the token itself finds nothing in it.
    assert!(store.session(&token).expect("read").is_none());
    assert!(
        store
            .session(&ck_crypto::sha256_hex(token.as_bytes()))
            .expect("read")
            .is_some()
    );
}
