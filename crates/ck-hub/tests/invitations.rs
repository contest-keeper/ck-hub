use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use ck_hub::{
    AcceptError, Hub, INVITATION_LIFETIME_DAYS, InviteError, MAX_MAILS_PER_ADDRESS, Mailer,
    MemberError, MemoryMailer, MemoryStore, PasswordCost, Registry, Role, token_after,
};

const PASSWORD: &str = "correct horse battery";
const SLUG: &str = "rcpc";

struct Setup {
    hub: Hub,
    mailer: Arc<MemoryMailer>,
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl Setup {
    fn session(&self, email: &str) -> String {
        if self.hub.login(email, PASSWORD).is_err() {
            self.hub.create_user(email, PASSWORD).expect("user");
        }
        self.hub.login(email, PASSWORD).expect("login").0
    }

    fn member(&self, email: &str, role: Role) -> String {
        self.hub.create_user(email, PASSWORD).expect("user");
        self.hub.add_member(SLUG, email, role).expect("member");
        self.session(email)
    }

    fn invite_token(&self, to: &str) -> String {
        let mail = self.mailer.last_to(to).expect("a mail");
        token_after(&mail.body, "#invite=").expect("a token")
    }
}

fn setup() -> Setup {
    let now = Arc::new(Mutex::new(
        "2026-11-15T10:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("time"),
    ));
    let clock = {
        let now = Arc::clone(&now);
        Box::new(move || *now.lock().expect("clock"))
    };
    let mailer = Arc::new(MemoryMailer::default());
    let hub = Hub::new(
        Registry::empty(),
        Arc::new(MemoryStore::default()),
        None,
        clock,
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    })
    .with_mail(
        Arc::clone(&mailer) as Arc<dyn Mailer>,
        "https://hub.example/",
    );
    hub.create_user("owner@example.org", PASSWORD)
        .expect("owner");
    hub.create_organization(SLUG, "RCPC", "owner@example.org")
        .expect("organization");
    Setup { hub, mailer, now }
}

#[test]
fn an_owner_invites_an_admin_and_an_admin_invites_a_viewer_and_nobody_invites_more_than_their_own_level()
 {
    let s = setup();
    let owner = s.session("owner@example.org");
    let admin = s.member("admin@example.org", Role::Admin);
    let viewer = s.member("viewer@example.org", Role::Viewer);
    let invite = |session: &str, role| s.hub.invite(session, SLUG, "new@example.org", role);
    assert_eq!(invite(&owner, Role::Admin), Ok(()));
    assert_eq!(invite(&owner, Role::Viewer), Ok(()));
    assert_eq!(invite(&owner, Role::Owner), Err(InviteError::Forbidden));
    assert_eq!(invite(&admin, Role::Viewer), Ok(()));
    assert_eq!(invite(&admin, Role::Admin), Err(InviteError::Forbidden));
    assert_eq!(invite(&viewer, Role::Viewer), Err(InviteError::Forbidden));
    let stranger = s.session("stranger@example.org");
    assert_eq!(invite(&stranger, Role::Viewer), Err(InviteError::NotFound));
    assert_eq!(
        s.hub
            .invite("no session", SLUG, "x@example.org", Role::Viewer),
        Err(InviteError::Unauthorized)
    );
    assert_eq!(
        s.hub.invite(&owner, SLUG, "not an address", Role::Viewer),
        Err(InviteError::BadEmail)
    );
}

#[test]
fn an_invitation_is_accepted_only_by_the_account_with_that_address_and_only_once() {
    let s = setup();
    let owner = s.session("owner@example.org");
    s.hub
        .invite(&owner, SLUG, "Guest@Example.org", Role::Admin)
        .expect("invited");
    let token = s.invite_token("Guest@Example.org");
    let other = s.session("other@example.org");
    assert_eq!(
        s.hub.accept_invitation(&other, &token),
        Err(AcceptError::WrongAccount)
    );
    assert_eq!(
        s.hub.accept_invitation("no session", &token),
        Err(AcceptError::Unauthorized)
    );
    let guest = s.session("guest@example.org");
    assert_eq!(s.hub.accept_invitation(&guest, &token), Ok(()));
    let members = s.hub.members_of(&owner, SLUG).expect("members");
    assert!(
        members
            .iter()
            .any(|m| m.email == "guest@example.org" && m.role == Role::Admin)
    );
    assert_eq!(
        s.hub.accept_invitation(&guest, &token),
        Err(AcceptError::Invalid)
    );
    assert_eq!(
        s.hub.accept_invitation(&guest, "never existed"),
        Err(AcceptError::Invalid)
    );

    s.hub
        .invite(&owner, SLUG, "late@example.org", Role::Viewer)
        .expect("invited");
    let late = s.invite_token("late@example.org");
    *s.now.lock().expect("clock") +=
        Duration::days(INVITATION_LIFETIME_DAYS) + Duration::minutes(1);
    let late_session = s.session("late@example.org");
    assert_eq!(
        s.hub.accept_invitation(&late_session, &late),
        Err(AcceptError::Invalid)
    );
}

#[test]
fn inviting_again_replaces_the_open_invitation_and_the_old_link_stops_working() {
    let s = setup();
    let owner = s.session("owner@example.org");
    s.hub
        .invite(&owner, SLUG, "guest@example.org", Role::Viewer)
        .expect("first");
    let first = s.invite_token("guest@example.org");
    s.hub
        .invite(&owner, SLUG, "guest@example.org", Role::Admin)
        .expect("second");
    let second = s.invite_token("guest@example.org");
    assert_ne!(first, second);
    let guest = s.session("guest@example.org");
    assert_eq!(
        s.hub.accept_invitation(&guest, &first),
        Err(AcceptError::Invalid)
    );
    assert_eq!(s.hub.accept_invitation(&guest, &second), Ok(()));
    assert_eq!(s.hub.invitations_of(&owner, SLUG).expect("list"), vec![]);
}

#[test]
fn inviting_an_unknown_address_and_a_member_look_the_same_to_the_caller() {
    let s = setup();
    let owner = s.session("owner@example.org");
    s.member("bo@example.org", Role::Viewer);
    assert_eq!(
        s.hub.invite(&owner, SLUG, "bo@example.org", Role::Viewer),
        Ok(())
    );
    let mail = s.mailer.last_to("bo@example.org").expect("a notice");
    assert!(!mail.body.contains("#invite="), "{}", mail.body);
    assert_eq!(
        s.hub
            .invite(&owner, SLUG, "nobody@example.org", Role::Viewer),
        Ok(())
    );
    assert!(
        s.mailer
            .last_to("nobody@example.org")
            .expect("a mail")
            .body
            .contains("https://hub.example/#invite=")
    );
    // Only the unknown address left an invitation.
    assert_eq!(s.hub.invitations_of(&owner, SLUG).expect("list").len(), 1);
}

#[test]
fn invitation_mail_is_limited_like_other_mail() {
    let s = setup();
    let owner = s.session("owner@example.org");
    for _ in 0..MAX_MAILS_PER_ADDRESS + 2 {
        s.hub
            .invite(&owner, SLUG, "guest@example.org", Role::Viewer)
            .expect("same answer every time");
    }
    let sent = s
        .mailer
        .sent()
        .iter()
        .filter(|mail| mail.to == "guest@example.org")
        .count();
    assert_eq!(sent, MAX_MAILS_PER_ADDRESS as usize);
}

#[test]
fn members_are_listed_and_removed_by_the_rules_and_the_last_owner_stays() {
    let s = setup();
    let owner = s.session("owner@example.org");
    let admin = s.member("admin@example.org", Role::Admin);
    let other_admin = s.member("admin2@example.org", Role::Admin);
    let viewer = s.member("viewer@example.org", Role::Viewer);
    let id = |email: &str| {
        s.hub
            .members_of(&owner, SLUG)
            .expect("members")
            .into_iter()
            .find(|m| m.email == email)
            .expect("member")
            .user_id
    };
    assert_eq!(s.hub.members_of(&viewer, SLUG), Err(MemberError::Forbidden));
    assert_eq!(
        s.hub.remove_member(&admin, SLUG, &id("admin2@example.org")),
        Err(MemberError::Forbidden)
    );
    assert_eq!(
        s.hub.remove_member(&viewer, SLUG, &id("admin@example.org")),
        Err(MemberError::Forbidden)
    );
    assert_eq!(
        s.hub.remove_member(&admin, SLUG, &id("viewer@example.org")),
        Ok(())
    );
    assert_eq!(
        s.hub.remove_member(&owner, SLUG, &id("admin2@example.org")),
        Ok(())
    );
    assert_eq!(
        s.hub.remove_member(&owner, SLUG, "no such user"),
        Err(MemberError::NotFound)
    );
    // Someone removed loses access at once; anyone may leave.
    assert_eq!(
        s.hub.members_of(&other_admin, SLUG),
        Err(MemberError::NotFound)
    );
    assert_eq!(
        s.hub.remove_member(&owner, SLUG, &id("owner@example.org")),
        Err(MemberError::LastOwner)
    );
    assert_eq!(
        s.hub.remove_member(&admin, SLUG, &id("admin@example.org")),
        Ok(())
    );
}

#[test]
fn invitations_can_be_listed_and_revoked_by_those_who_may_make_them() {
    let s = setup();
    let owner = s.session("owner@example.org");
    let admin = s.member("admin@example.org", Role::Admin);
    s.hub
        .invite(&owner, SLUG, "a@example.org", Role::Admin)
        .expect("invited");
    s.hub
        .invite(&owner, SLUG, "v@example.org", Role::Viewer)
        .expect("invited");
    let list = s.hub.invitations_of(&admin, SLUG).expect("list");
    assert_eq!(list.len(), 2);
    let find = |email: &str| {
        list.iter()
            .find(|i| i.email == email)
            .expect("one")
            .id
            .clone()
    };
    assert_eq!(
        s.hub
            .revoke_invitation(&admin, SLUG, &find("a@example.org")),
        Err(MemberError::Forbidden)
    );
    assert_eq!(
        s.hub
            .revoke_invitation(&admin, SLUG, &find("v@example.org")),
        Ok(())
    );
    assert_eq!(
        s.hub
            .revoke_invitation(&owner, SLUG, &find("v@example.org")),
        Err(MemberError::NotFound)
    );
    let token = s.invite_token("a@example.org");
    assert_eq!(
        s.hub
            .revoke_invitation(&owner, SLUG, &find("a@example.org")),
        Ok(())
    );
    let guest = s.session("a@example.org");
    assert_eq!(
        s.hub.accept_invitation(&guest, &token),
        Err(AcceptError::Invalid)
    );
}
