use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use ck_hub::{
    Hub, LoginError, MAIL_WINDOW_SECONDS, MAX_FAILED_LOGINS, MAX_MAILS_PER_ADDRESS, Mail, Mailer,
    MemoryMailer, MemoryStore, PasswordCost, Registry, SignupError, TokenError,
    VERIFY_LIFETIME_HOURS, token_after,
};

const PASSWORD: &str = "correct horse battery";
const NEW: &str = "newcomer@example.org";

struct Setup {
    hub: Arc<Hub>,
    mailer: Arc<MemoryMailer>,
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl Setup {
    fn advance(&self, by: Duration) {
        *self.now.lock().expect("clock") += by;
    }

    fn link_token(&self, to: &str, marker: &str) -> String {
        let mail = self.mailer.last_to(to).expect("a mail");
        token_after(&mail.body, marker).expect("a token in the mail")
    }
}

fn hub_with(now: &Arc<Mutex<DateTime<Utc>>>) -> Hub {
    let clock = {
        let now = Arc::clone(now);
        Box::new(move || *now.lock().expect("clock"))
    };
    Hub::new(
        Registry::empty(),
        Arc::new(MemoryStore::default()),
        None,
        clock,
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    })
}

fn setup() -> Setup {
    let now = Arc::new(Mutex::new(
        "2026-11-12T10:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("time"),
    ));
    let mailer = Arc::new(MemoryMailer::default());
    let hub = hub_with(&now).with_mail(
        Arc::clone(&mailer) as Arc<dyn Mailer>,
        "https://hub.example/",
    );
    Setup {
        hub: Arc::new(hub),
        mailer,
        now,
    }
}

#[test]
fn registering_sends_a_link_and_the_account_cannot_log_in_until_it_is_used() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    assert_eq!(
        s.hub.login(NEW, PASSWORD).err(),
        Some(LoginError::NotVerified)
    );
    let mail = s.mailer.last_to(NEW).expect("a mail");
    assert!(mail.body.contains("https://hub.example/#verify="));
    s.hub
        .verify_email(&s.link_token(NEW, "#verify="))
        .expect("verified");
    assert!(s.hub.login(NEW, PASSWORD).is_ok());
}

#[test]
fn a_wrong_password_is_still_a_wrong_password_for_an_unverified_account() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    assert_eq!(
        s.hub.login(NEW, "not the password!!").err(),
        Some(LoginError::BadCredentials)
    );
}

#[test]
fn a_verification_link_works_once_and_not_after_a_day() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    let token = s.link_token(NEW, "#verify=");
    s.hub.verify_email(&token).expect("first use");
    assert_eq!(s.hub.verify_email(&token), Err(TokenError::Invalid));

    s.hub
        .register_account("late@example.org", PASSWORD)
        .expect("registered");
    let late = s.link_token("late@example.org", "#verify=");
    s.advance(Duration::hours(VERIFY_LIFETIME_HOURS) + Duration::seconds(1));
    assert_eq!(s.hub.verify_email(&late), Err(TokenError::Invalid));
}

#[test]
fn registering_validates_input_and_needs_a_mailer() {
    let s = setup();
    assert_eq!(
        s.hub.register_account("nope", PASSWORD),
        Err(SignupError::BadEmail)
    );
    assert_eq!(
        s.hub.register_account(NEW, "short"),
        Err(SignupError::PasswordTooShort)
    );
    assert_eq!(s.mailer.sent(), Vec::<Mail>::new());

    let bare = hub_with(&s.now);
    assert_eq!(
        bare.register_account(NEW, PASSWORD),
        Err(SignupError::MailNotSetUp)
    );
    assert_eq!(
        bare.request_password_reset(NEW),
        Err(SignupError::MailNotSetUp)
    );
}

#[test]
fn registering_a_known_address_looks_the_same_and_never_changes_the_account() {
    let s = setup();
    s.hub.create_user("ana@example.org", PASSWORD).expect("ana");
    s.hub
        .register_account("ana@example.org", "another long password")
        .expect("same answer");
    let mail = s.mailer.last_to("ana@example.org").expect("a mail");
    assert!(
        !mail.body.contains("#verify="),
        "a confirmed account gets a notice, not a link"
    );
    assert!(s.hub.login("ana@example.org", PASSWORD).is_ok());
    assert_eq!(
        s.hub
            .login("ana@example.org", "another long password")
            .err(),
        Some(LoginError::BadCredentials)
    );
}

#[test]
fn registering_again_before_confirming_sends_a_fresh_link_and_keeps_the_first_password() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("first");
    let first = s.link_token(NEW, "#verify=");
    s.hub
        .register_account(NEW, "a different password")
        .expect("second");
    let second = s.link_token(NEW, "#verify=");
    assert_ne!(first, second);
    assert_eq!(
        s.hub.verify_email(&first),
        Err(TokenError::Invalid),
        "the older link is replaced"
    );
    s.hub.verify_email(&second).expect("verified");
    assert!(s.hub.login(NEW, PASSWORD).is_ok());
}

#[test]
fn mail_is_limited_per_address_and_the_answer_does_not_change() {
    let s = setup();
    for _ in 0..MAX_MAILS_PER_ADDRESS + 2 {
        s.hub
            .register_account(NEW, PASSWORD)
            .expect("always the same answer");
    }
    assert_eq!(s.mailer.sent().len(), MAX_MAILS_PER_ADDRESS as usize);
    s.advance(Duration::seconds(MAIL_WINDOW_SECONDS + 1));
    s.hub.register_account(NEW, PASSWORD).expect("answer");
    assert_eq!(s.mailer.sent().len(), MAX_MAILS_PER_ADDRESS as usize + 1);
}

#[test]
fn a_reset_link_sets_a_new_password_ends_sessions_and_verifies() {
    let s = setup();
    s.hub.create_user("ana@example.org", PASSWORD).expect("ana");
    let (session, _) = s.hub.login("ana@example.org", PASSWORD).expect("login");
    s.hub
        .request_password_reset("ana@example.org")
        .expect("asked");
    let token = s.link_token("ana@example.org", "#reset=");
    assert_eq!(
        s.hub.reset_password(&token, "short"),
        Err(TokenError::PasswordTooShort)
    );
    s.hub
        .reset_password(&token, "a brand new password")
        .expect("reset");
    assert_eq!(
        s.hub.reset_password(&token, "a brand new password"),
        Err(TokenError::Invalid)
    );
    assert!(s.hub.account(&session).is_err(), "old sessions ended");
    assert!(s.hub.login("ana@example.org", PASSWORD).is_err());
    assert!(
        s.hub
            .login("ana@example.org", "a brand new password")
            .is_ok()
    );
}

#[test]
fn a_reset_for_an_unknown_address_sends_nothing_and_says_nothing() {
    let s = setup();
    s.hub
        .request_password_reset("ghost@example.org")
        .expect("same answer");
    assert_eq!(s.mailer.sent(), Vec::<Mail>::new());
}

#[test]
fn a_reset_clears_the_login_lockout_and_confirms_an_unverified_account() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    s.hub.request_password_reset(NEW).expect("asked");
    let token = s.link_token(NEW, "#reset=");
    for _ in 0..MAX_FAILED_LOGINS {
        let _ = s.hub.login(NEW, "wrong wrong wrong");
    }
    assert_eq!(
        s.hub.login(NEW, PASSWORD).err(),
        Some(LoginError::TooManyAttempts)
    );
    s.hub
        .reset_password(&token, "a brand new password")
        .expect("reset");
    assert!(s.hub.login(NEW, "a brand new password").is_ok());
}

#[test]
fn unconfirmed_accounts_are_removed_after_two_days() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    s.advance(Duration::hours(49));
    s.hub
        .register_account("someone@example.org", PASSWORD)
        .expect("purges as it goes");
    s.hub
        .register_account(NEW, "a different password")
        .expect("the address is free again");
    assert_eq!(
        s.hub.login(NEW, "a different password").err(),
        Some(LoginError::NotVerified),
        "a new account, with the new password"
    );
}

mod http {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    async fn post(app: &Router, path: &str, body: serde_json::Value) -> (StatusCode, bool, String) {
        let request = Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let cookie = response.headers().contains_key("set-cookie");
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, cookie, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn the_whole_sign_up_through_http() {
        let s = setup();
        let app = ck_hub::router(Arc::clone(&s.hub));
        let json = |email: &str, password: &str| serde_json::json!({ "email": email, "password": password });

        let (status, _, _) = post(&app, "/api/v1/register", json("nope", PASSWORD)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, text) = post(&app, "/api/v1/register", json(NEW, "short")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(text.contains("12 characters"), "{text}");
        let (status, _, _) = post(&app, "/api/v1/register", json(NEW, PASSWORD)).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        let (status, cookie, text) = post(&app, "/api/v1/login", json(NEW, PASSWORD)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!cookie, "no session before the email is confirmed");
        assert!(text.contains("confirm your email"), "{text}");

        let token = s.link_token(NEW, "#verify=");
        let (status, _, _) = post(
            &app,
            "/api/v1/verify-email",
            serde_json::json!({ "token": token }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = post(
            &app,
            "/api/v1/verify-email",
            serde_json::json!({ "token": token }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, cookie, _) = post(&app, "/api/v1/login", json(NEW, PASSWORD)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(cookie);
    }

    #[tokio::test]
    async fn reset_answers_the_same_for_every_address_and_a_short_password_keeps_the_link() {
        let s = setup();
        s.hub.create_user("ana@example.org", PASSWORD).expect("ana");
        let app = ck_hub::router(Arc::clone(&s.hub));
        let ask = |email: &str| serde_json::json!({ "email": email });
        let known = post(&app, "/api/v1/password-reset", ask("ana@example.org")).await;
        let unknown = post(&app, "/api/v1/password-reset", ask("ghost@example.org")).await;
        assert_eq!(known, unknown);
        assert_eq!(known.0, StatusCode::ACCEPTED);

        let token = s.link_token("ana@example.org", "#reset=");
        let (status, _, _) = post(
            &app,
            "/api/v1/password-reset/confirm",
            serde_json::json!({ "token": token, "password": "short" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = post(
            &app,
            "/api/v1/password-reset/confirm",
            serde_json::json!({ "token": token, "password": "a brand new password" }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn without_mail_sign_up_and_reset_are_closed() {
        let now = Arc::new(Mutex::new(Utc::now()));
        let app = ck_hub::router(Arc::new(hub_with(&now)));
        let (status, _, _) = post(
            &app,
            "/api/v1/register",
            serde_json::json!({ "email": NEW, "password": PASSWORD }),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let (status, _, _) = post(
            &app,
            "/api/v1/password-reset",
            serde_json::json!({ "email": NEW }),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
