use std::fs;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{
    AccessError, CreateUserError, Hub, LoginError, MAX_FAILED_LOGINS, MemoryStore,
    NAME_LOCKOUT_SECONDS, OrganizationError, PasswordCost, Registry, Role, SESSION_LIFETIME_HOURS,
    Store, router,
};
use http_body_util::BodyExt;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

struct Setup {
    _dir: TempDir,
    hub: Arc<Hub>,
    app: Router,
    now: Arc<Mutex<OffsetDateTime>>,
}

fn setup() -> Setup {
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("registry.json"), r#"{"sticks":[]}"#).expect("registry");
    let now = Arc::new(Mutex::new(
        "2026-11-12T10:00:00Z"
            .parse::<ck_protocol::Timestamp>()
            .expect("time")
            .as_offset_date_time(),
    ));
    let clock = {
        let now = Arc::clone(&now);
        Box::new(move || *now.lock().expect("clock"))
    };
    let hub = Hub::new(
        Registry::open(dir.path().join("registry.json")).expect("open"),
        Arc::new(MemoryStore::default()),
        None,
        clock,
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    hub.create_user("ana@example.org", PASSWORD).expect("ana");
    hub.create_user("vera@example.org", PASSWORD).expect("vera");
    hub.create_organization("default", "Default", "ana@example.org")
        .expect("organization");
    hub.add_member("default", "vera@example.org", Role::Viewer)
        .expect("vera is a viewer");
    hub.create_organization("other", "Other", "ana@example.org")
        .expect("another organization");
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
            serde_json::json!({ "email": name, "password": password }).to_string(),
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
        get(&setup.app, "/api/v1/orgs/default/sticks", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&setup.app, "/api/v1/me", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A cookie that is not a session gets the same answer.
    let (status, _) = get(
        &setup.app,
        "/api/v1/orgs/default/sticks",
        Some("ck_session=made-up"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logging_in_gives_a_locked_down_cookie_that_opens_the_data_until_logout() {
    let setup = setup();
    let (status, cookie, body) = post_login(&setup.app, "vera@example.org", PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let set = cookie.expect("a cookie");
    for flag in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(set.contains(flag), "{flag} missing in {set}");
    }
    assert!(!body.contains(PASSWORD), "the password is never echoed");
    let pair = cookie_pair(&set);

    assert_eq!(
        get(&setup.app, "/api/v1/orgs/default/sticks", Some(&pair))
            .await
            .0,
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
        get(&setup.app, "/api/v1/orgs/default/sticks", Some(&pair))
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "the session is gone, not only the cookie"
    );
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_name_get_the_same_answer() {
    let setup = setup();
    let wrong = post_login(&setup.app, "vera@example.org", "not the password at all").await;
    let unknown = post_login(&setup.app, "nobody@example.org", "not the password at all").await;
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        (wrong.0, &wrong.1, &wrong.2),
        (unknown.0, &unknown.1, &unknown.2)
    );
    assert!(wrong.1.is_none(), "no cookie");
}

#[test]
fn access_is_checked_per_organization() {
    let setup = setup();
    let (viewer, _) = setup
        .hub
        .login("vera@example.org", PASSWORD)
        .expect("log in");
    assert!(setup.hub.access(&viewer, "default", Role::Viewer).is_ok());
    assert_eq!(
        setup.hub.access(&viewer, "default", Role::Admin).err(),
        Some(AccessError::Forbidden)
    );
    // Not a member of "other", and no such organization, look the same.
    assert_eq!(
        setup.hub.access(&viewer, "other", Role::Viewer).err(),
        Some(AccessError::NotFound)
    );
    assert_eq!(
        setup.hub.access(&viewer, "nowhere", Role::Viewer).err(),
        Some(AccessError::NotFound)
    );
    assert_eq!(
        setup.hub.access("made-up", "default", Role::Viewer).err(),
        Some(AccessError::Unauthorized)
    );
}

#[test]
fn an_owner_may_do_everything_and_the_only_organization_is_found_without_a_slug() {
    let setup = setup();
    let (owner, _) = setup
        .hub
        .login("ana@example.org", PASSWORD)
        .expect("log in");
    assert!(setup.hub.access(&owner, "default", Role::Owner).is_ok());
    assert!(
        setup.hub.access(&owner, "default", Role::Viewer).is_ok(),
        "an owner sees what a viewer sees"
    );
    let (viewer, _) = setup
        .hub
        .login("vera@example.org", PASSWORD)
        .expect("log in");
    let access = setup.hub.access_sole(&viewer, Role::Viewer).expect("sole");
    assert_eq!(access.organization.slug, "default");
    // Ana belongs to two organizations, so there is no "the" organization.
    assert_eq!(
        setup.hub.access_sole(&owner, Role::Viewer).err(),
        Some(AccessError::Ambiguous)
    );
}

#[test]
fn organizations_have_checked_slugs_and_owners() {
    let setup = setup();
    let make = |slug: &str, owner: &str| setup.hub.create_organization(slug, "Name", owner);
    assert_eq!(
        make("Bad Slug", "ana@example.org").err(),
        Some(OrganizationError::BadSlug)
    );
    assert_eq!(
        make("-bad", "ana@example.org").err(),
        Some(OrganizationError::BadSlug)
    );
    assert_eq!(
        make("default", "ana@example.org").err(),
        Some(OrganizationError::SlugTaken)
    );
    assert_eq!(
        make("fresh", "nobody@example.org").err(),
        Some(OrganizationError::NoSuchUser)
    );
    assert!(make("fresh", "ana@example.org").is_ok());
    assert_eq!(
        setup
            .hub
            .add_member("fresh", "ana@example.org", Role::Viewer)
            .err(),
        Some(OrganizationError::AlreadyMember)
    );
    assert_eq!(
        setup
            .hub
            .add_member("nowhere", "ana@example.org", Role::Viewer)
            .err(),
        Some(OrganizationError::NoSuchOrganization)
    );
}

#[test]
fn a_session_ends_after_its_lifetime() {
    let setup = setup();
    let (token, _) = setup
        .hub
        .login("vera@example.org", PASSWORD)
        .expect("login");
    *setup.now.lock().expect("clock") +=
        Duration::hours(SESSION_LIFETIME_HOURS) - Duration::seconds(1);
    assert!(setup.hub.access(&token, "default", Role::Viewer).is_ok());
    *setup.now.lock().expect("clock") += Duration::seconds(1);
    assert_eq!(
        setup.hub.access(&token, "default", Role::Viewer).err(),
        Some(AccessError::Unauthorized)
    );
}

#[test]
fn too_many_wrong_passwords_lock_the_name_for_a_while_even_for_the_right_password() {
    let setup = setup();
    for _ in 0..MAX_FAILED_LOGINS {
        assert_eq!(
            setup
                .hub
                .login("vera@example.org", "wrong wrong wrong")
                .err(),
            Some(LoginError::BadCredentials)
        );
    }
    assert_eq!(
        setup.hub.login("vera@example.org", PASSWORD).err(),
        Some(LoginError::TooManyAttempts)
    );
    // Another name is not affected.
    assert!(setup.hub.login("ana@example.org", PASSWORD).is_ok());

    *setup.now.lock().expect("clock") += Duration::seconds(NAME_LOCKOUT_SECONDS);
    assert!(
        setup.hub.login("vera@example.org", PASSWORD).is_ok(),
        "the lock ends"
    );
}

#[tokio::test]
async fn the_lock_is_a_429_over_http() {
    let setup = setup();
    for _ in 0..MAX_FAILED_LOGINS {
        post_login(&setup.app, "vera@example.org", "wrong wrong wrong").await;
    }
    assert_eq!(
        post_login(&setup.app, "vera@example.org", PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[test]
fn accounts_are_checked_when_made_and_only_hashes_are_kept() {
    let setup = setup();
    let make = |email: &str, password: &str| setup.hub.create_user(email, password).err();
    assert_eq!(
        make("ana@example.org", PASSWORD),
        Some(CreateUserError::EmailTaken)
    );
    assert_eq!(
        make("ANA@Example.org", PASSWORD),
        Some(CreateUserError::EmailTaken),
        "capitals do not make another account"
    );
    assert_eq!(
        make("bo@example.org", "short"),
        Some(CreateUserError::PasswordTooShort)
    );
    for bad in ["  ", "a\nb", "no-at-sign", "@example.org", "ana@", "a@b@c"] {
        assert_eq!(
            make(bad, PASSWORD),
            Some(CreateUserError::BadEmail),
            "{bad:?}"
        );
    }

    // The email taken stays what it was: the refused second account did not replace the first.
    assert!(setup.hub.login("ana@example.org", PASSWORD).is_ok());
}

#[test]
fn neither_the_password_nor_the_session_token_is_what_the_store_holds() {
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("registry.json"), r#"{"sticks":[]}"#).expect("registry");
    let store = Arc::new(MemoryStore::default());
    let hub = Hub::new(
        Registry::open(dir.path().join("registry.json")).expect("open"),
        Arc::clone(&store) as Arc<dyn Store>,
        None,
        Box::new(OffsetDateTime::now_utc),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    hub.create_user("vera@example.org", PASSWORD).expect("user");
    let (token, _) = hub.login("vera@example.org", PASSWORD).expect("login");

    let user = store
        .user_by_email("vera@example.org")
        .expect("read")
        .expect("a user");
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

#[tokio::test]
async fn an_account_in_several_organizations_reaches_each_by_its_address() {
    let setup = setup();
    let (status, cookie, body) = post_login(&setup.app, "ana@example.org", PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pair = cookie_pair(&cookie.expect("a cookie"));

    // Ana owns two organizations, and each answers only at its own address.
    assert_eq!(
        get(&setup.app, "/api/v1/orgs/default/sticks", Some(&pair))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        get(&setup.app, "/api/v1/orgs/other/sticks", Some(&pair))
            .await
            .0,
        StatusCode::OK
    );

    let (status, me) = get(&setup.app, "/api/v1/me", Some(&pair)).await;
    assert_eq!(status, StatusCode::OK);
    let me: serde_json::Value = serde_json::from_str(&me).expect("json");
    let slugs: Vec<&str> = me["organizations"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|o| o["slug"].as_str().expect("a slug"))
        .collect();
    assert_eq!(slugs, ["default", "other"]);
    assert_eq!(me["organizations"][0]["role"], "owner");
    assert!(
        me.get("role").is_none(),
        "no single role when there are several organizations"
    );
}

#[tokio::test]
async fn one_source_guessing_does_not_lock_login_for_another_source() {
    let setup = setup();
    let from = |source: &'static str, name: String, password: &'static str| {
        let app = setup.app.clone();
        async move {
            let request = Request::post("/api/v1/login")
                .header("content-type", "application/json")
                .header("x-forwarded-for", format!("6.6.6.6, {source}"))
                .body(Body::from(
                    serde_json::json!({ "email": name, "password": password }).to_string(),
                ))
                .expect("request");
            app.oneshot(request).await.expect("response").status()
        }
    };
    for round in 0..10 {
        let status = from("192.0.2.1", format!("nobody{round}@example.org"), "wrong").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    // The guesser is refused even with a right password; somebody else logs in fine.
    assert_eq!(
        from("192.0.2.1", "vera@example.org".to_owned(), PASSWORD).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        from("192.0.2.2", "vera@example.org".to_owned(), PASSWORD).await,
        StatusCode::OK
    );
}
