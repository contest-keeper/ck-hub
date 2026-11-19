//! Where the Hub keeps what sticks report.
//!
//! The rules (what to accept, what is a warning) live in the Hub. A store only keeps and returns
//! data, so the same rules run on memory in tests and on PostgreSQL in production.
//!
//! Events are append-only: a store never changes or deletes one.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ck_protocol::{CommandAck, Event, Heartbeat, StickId, Token};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::commands::{CommandState, StoredCommand, settled};
use crate::enrol::{EnrolledStick, NewEnrolmentCode, Redemption};
use crate::judge::{JudgeSubmission, SubmitEvent, submit_event};
use crate::recordings::Recording;

#[derive(Debug, thiserror::Error)]
#[error("the store failed: {0}")]
pub struct StoreError(pub String);

/// What a member of an organization may do. Each role may do everything the ones below it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Everything an admin may do, and invites admins.
    Owner,
    /// Manages sticks, and sees everything a viewer sees.
    Admin,
    /// Only looks at the dashboard.
    Viewer,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Viewer => "viewer",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Owner => 3,
            Self::Admin => 2,
            Self::Viewer => 1,
        }
    }

    /// Whether someone with this role may invite or remove someone with `other`: only a lower role.
    #[must_use]
    pub fn may_manage(self, other: Role) -> bool {
        self.rank() > other.rank()
    }

    /// Whether someone with this role may do what `needed` asks for.
    #[must_use]
    pub fn allows(self, needed: Role) -> bool {
        self.rank() >= needed.rank()
    }
}

/// A stick the admin made. The Hub keeps its secret to check what the stick signs. It reads as
/// an entry of the admin tool's registry file, whose other fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Stick {
    pub stick_id: StickId,
    pub token: Token,
    #[serde(default)]
    pub credential_scope: Option<crate::CredentialScope>,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub contest: String,
    #[serde(default)]
    pub machine_number: Option<u32>,
}

/// An account. The password is only ever kept as an Argon2 hash. `email` is what the person logs
/// in with; accounts made before organizations existed keep their old name there.
#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
    pub password_hash: String,
    /// When the owner of the address confirmed it. `None` until then.
    pub email_verified_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    /// Platform staff make organizations. They do not see inside them.
    pub is_platform_staff: bool,
}

/// What a store needs to make an account. The store picks the id.
#[derive(Debug, Clone)]
pub struct NewUser {
    pub email: String,
    pub password_hash: String,
    pub email_verified_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

/// What a mailed one-time token may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenPurpose {
    Verify,
    Reset,
}

impl TokenPurpose {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Reset => "reset",
        }
    }
}

/// A club or contest team. Everything sticks report belongs to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    pub id: String,
    pub slug: String,
    pub name: String,
}

/// An invitation to join an organization. The hash of its token is never given back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    pub id: String,
    pub organization_id: String,
    pub email: String,
    pub role: Role,
    pub invited_by: Option<String>,
    pub expires_at: OffsetDateTime,
}

/// A person in an organization, with their role there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: String,
    pub email: String,
    pub role: Role,
}

/// An organization a user belongs to, and the role they have in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub organization: Organization,
    pub role: Role,
}

/// The latest heartbeat of a stick, when it arrived and how many there have been.
#[derive(Debug, Clone)]
pub struct SeenHeartbeat {
    pub heartbeat: Heartbeat,
    pub received_at: OffsetDateTime,
    pub count: u64,
}

/// An event as the Hub received it, with what was wrong with its place in the chain, if anything.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub received_at: OffsetDateTime,
    pub event: Event,
    pub integrity: Option<String>,
}

/// Every method fails with [`StoreError`] when the data cannot be read or written.
pub trait Store: Send + Sync {
    /// Keeps `heartbeat` as the latest of its stick in the organization and counts it.
    fn record_heartbeat(
        &self,
        organization: &str,
        heartbeat: Heartbeat,
        received_at: OffsetDateTime,
    ) -> Result<(), StoreError>;

    fn last_heartbeat(
        &self,
        organization: &str,
        stick: &StickId,
    ) -> Result<Option<SeenHeartbeat>, StoreError>;

    /// The highest heartbeat sequence ever accepted from `stick` in `boot`, if any.
    fn heartbeat_seq_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<u64>, StoreError>;

    /// Adds an event after the ones already kept for its stick.
    fn append_event(
        &self,
        organization: &str,
        event: Event,
        received_at: OffsetDateTime,
        integrity: Option<String>,
    ) -> Result<(), StoreError>;

    /// The newest event of `stick` in `boot`, if any.
    fn last_event_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<StoredEvent>, StoreError>;

    /// Every event of the stick, in the order they were received.
    fn events(&self, organization: &str, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError>;

    /// Keeps the submissions the judge reported that are not kept yet. Returns how many were new.
    fn add_judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        submissions: &[JudgeSubmission],
        received_at: OffsetDateTime,
    ) -> Result<usize, StoreError>;

    /// Every kept submission of one contest, oldest first.
    fn judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        contest_id: u64,
    ) -> Result<Vec<JudgeSubmission>, StoreError>;

    /// What every stick of the organization reported as submitted to Codeforces, in the order
    /// the Hub received it. Events that are not well-formed Codeforces submits are left out.
    fn submission_events(&self, organization: &str) -> Result<Vec<SubmitEvent>, StoreError>;

    /// Adds an account. Returns `None`, and changes nothing, when the email is taken. Capitals
    /// do not matter.
    fn create_user(&self, user: NewUser) -> Result<Option<User>, StoreError>;

    /// The account with this email, whatever its capitals.
    fn user_by_email(&self, email: &str) -> Result<Option<User>, StoreError>;

    fn user(&self, id: &str) -> Result<Option<User>, StoreError>;

    /// Adds an organization. Returns `None`, and changes nothing, when the slug is taken.
    fn create_organization(
        &self,
        slug: &str,
        name: &str,
    ) -> Result<Option<Organization>, StoreError>;

    /// The organization with this slug, made if there is none.
    fn ensure_organization(&self, slug: &str, name: &str) -> Result<Organization, StoreError>;

    fn organization_by_slug(&self, slug: &str) -> Result<Option<Organization>, StoreError>;

    /// Makes `user_id` a member. Returns false, and changes nothing, for a member already.
    fn add_member(
        &self,
        organization_id: &str,
        user_id: &str,
        role: Role,
    ) -> Result<bool, StoreError>;

    fn membership(&self, organization_id: &str, user_id: &str) -> Result<Option<Role>, StoreError>;

    /// Every organization the user belongs to, ordered by slug.
    fn memberships(&self, user_id: &str) -> Result<Vec<Membership>, StoreError>;

    /// Starts a session. `token_hash` is a hash of the token the browser holds, never the token.
    fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError>;

    /// The user id a session belongs to and when it ends, if there is such a session.
    fn session(&self, token_hash: &str) -> Result<Option<(String, OffsetDateTime)>, StoreError>;

    fn delete_session(&self, token_hash: &str) -> Result<(), StoreError>;

    /// Removes every session that ended at or before `now`.
    fn delete_expired_sessions(&self, now: OffsetDateTime) -> Result<(), StoreError>;

    fn mark_email_verified(&self, user_id: &str, at: OffsetDateTime) -> Result<(), StoreError>;

    fn set_password_hash(&self, user_id: &str, password_hash: &str) -> Result<(), StoreError>;

    /// Ends every session of the account.
    fn delete_sessions_of(&self, user_id: &str) -> Result<(), StoreError>;

    /// Keeps a hash of a mailed token. It replaces the account's earlier token of the same purpose.
    fn put_email_token(
        &self,
        token_hash: &str,
        user_id: &str,
        purpose: TokenPurpose,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError>;

    /// Uses up a token: the account it was made for, if it exists, has this purpose and has not
    /// ended by `now`. A token of another purpose is left alone; an ended one is removed.
    fn take_email_token(
        &self,
        token_hash: &str,
        purpose: TokenPurpose,
        now: OffsetDateTime,
    ) -> Result<Option<String>, StoreError>;

    /// Removes every token that ended at or before `now`.
    fn delete_expired_email_tokens(&self, now: OffsetDateTime) -> Result<(), StoreError>;

    /// Removes accounts nobody confirmed that were made before `before`, with their tokens and
    /// sessions.
    fn delete_unverified_users_before(&self, before: OffsetDateTime) -> Result<(), StoreError>;

    /// Keeps an invitation. It replaces the open one for the same organization and address (capitals
    /// do not matter).
    fn put_invitation(
        &self,
        organization_id: &str,
        email: &str,
        role: Role,
        token_hash: &str,
        invited_by: Option<&str>,
        expires_at: OffsetDateTime,
    ) -> Result<Invitation, StoreError>;

    /// The invitation a token belongs to, without using it up. Ended ones are returned too.
    fn invitation_for_token(&self, token_hash: &str) -> Result<Option<Invitation>, StoreError>;

    /// Uses up an invitation: it is gone afterwards, and given back only if it has not ended by `now`.
    fn take_invitation(
        &self,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Invitation>, StoreError>;

    /// The open invitations of an organization, ordered by address.
    fn invitations_of(&self, organization_id: &str) -> Result<Vec<Invitation>, StoreError>;

    /// Returns false when the organization has no invitation with this id.
    fn delete_invitation(&self, organization_id: &str, id: &str) -> Result<bool, StoreError>;

    /// Removes every invitation that ended at or before `now`.
    fn delete_expired_invitations(&self, now: OffsetDateTime) -> Result<(), StoreError>;

    /// Everyone in the organization, ordered by email.
    fn members_of(&self, organization_id: &str) -> Result<Vec<Member>, StoreError>;

    /// Returns false when the user was not a member.
    fn remove_member(&self, organization_id: &str, user_id: &str) -> Result<bool, StoreError>;

    fn count_owners(&self, organization_id: &str) -> Result<usize, StoreError>;

    fn set_staff(&self, user_id: &str, staff: bool) -> Result<(), StoreError>;

    /// Every organization, ordered by slug.
    fn organizations(&self) -> Result<Vec<Organization>, StoreError>;

    /// Adds a stick to an organization. Returns false, and changes nothing, when its id is
    /// already known to any organization: a stick's secret is never replaced, and an id belongs
    /// to one organization.
    fn add_stick(&self, organization: &str, stick: Stick) -> Result<bool, StoreError>;

    /// The stick with this id and the organization it belongs to.
    fn stick(&self, id: &StickId) -> Result<Option<(String, Stick)>, StoreError>;

    /// Every stick of the organization.
    fn sticks(&self, organization: &str) -> Result<Vec<Stick>, StoreError>;

    /// Keeps a profile of the organization, replacing the one with the same id. Another
    /// organization's profile with that id is a different profile and is not touched.
    fn put_profile(
        &self,
        organization: &str,
        profile: NewProfile,
        now: OffsetDateTime,
    ) -> Result<(), StoreError>;

    /// The organization's profiles, by name.
    fn profiles(&self, organization: &str) -> Result<Vec<ProfileSummary>, StoreError>;

    fn profile(&self, organization: &str, id: &str) -> Result<Option<Profile>, StoreError>;

    /// The picture of a profile, if it has one.
    fn profile_wallpaper(
        &self,
        organization: &str,
        id: &str,
    ) -> Result<Option<Wallpaper>, StoreError>;

    /// Returns whether there was such a profile.
    fn delete_profile(&self, organization: &str, id: &str) -> Result<bool, StoreError>;

    /// Keeps a signed command of the organization for its stick to pick up.
    fn queue_command(&self, organization: &str, command: StoredCommand) -> Result<(), StoreError>;

    /// The stick's commands that are not finished and not expired at `now`, oldest first.
    fn pending_commands(
        &self,
        organization: &str,
        stick: &StickId,
        now: OffsetDateTime,
    ) -> Result<Vec<StoredCommand>, StoreError>;

    /// Marks queued commands as delivered. A command in another state is left as it is.
    fn mark_commands_delivered(
        &self,
        organization: &str,
        ids: &[String],
        now: OffsetDateTime,
    ) -> Result<(), StoreError>;

    /// Settles the command an ack names, when it is the stick's and still open. Returns whether it
    /// changed anything: the first ack decides, a later one changes nothing.
    fn record_command_ack(
        &self,
        organization: &str,
        stick: &StickId,
        ack: &CommandAck,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError>;

    /// The newest commands of a stick, newest first.
    fn commands_of(
        &self,
        organization: &str,
        stick: &StickId,
        limit: u32,
    ) -> Result<Vec<StoredCommand>, StoreError>;

    fn command(&self, organization: &str, id: &str) -> Result<Option<StoredCommand>, StoreError>;

    /// Removes the organization's commands created before `before`.
    fn delete_commands_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError>;

    /// Keeps a one-time enrolment code (its hash) for a stick of the organization.
    fn add_enrolment_code(
        &self,
        organization: &str,
        code: NewEnrolmentCode,
    ) -> Result<(), StoreError>;

    /// Uses up the code with this hash and registers a new stick, in one step, in the organization
    /// the code was made for. A code that is unknown, used or expired changes nothing. When the stick
    /// id is taken the code is not used up.
    fn redeem_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError>;

    /// Removes the organization's codes made before `before`.
    fn delete_enrolment_codes_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError>;

    /// Keeps a new recording. Returns false, and keeps nothing, when the stick already has a
    /// recording of that file name.
    fn add_recording(&self, organization: &str, recording: &Recording) -> Result<bool, StoreError>;

    fn recording(&self, organization: &str, id: &str) -> Result<Option<Recording>, StoreError>;

    /// The newest recordings of a stick, newest first.
    fn recordings_of(
        &self,
        organization: &str,
        stick: &StickId,
        limit: u32,
    ) -> Result<Vec<Recording>, StoreError>;

    /// Moves an upload past chunk `index`, only if `index` is the next one it expects and it is not
    /// complete. Returns whether it changed anything.
    fn advance_recording(
        &self,
        organization: &str,
        id: &str,
        index: u32,
    ) -> Result<bool, StoreError>;

    /// Marks an upload complete, once.
    fn complete_recording(
        &self,
        organization: &str,
        id: &str,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<bool, StoreError>;

    /// Recordings that are complete and expired at `now`, and uploads created before
    /// `abandoned_before` that never completed. At most a few hundred.
    fn due_recordings(
        &self,
        organization: &str,
        now: OffsetDateTime,
        abandoned_before: OffsetDateTime,
    ) -> Result<Vec<Recording>, StoreError>;

    fn delete_recording(&self, organization: &str, id: &str) -> Result<(), StoreError>;
}

/// A picture kept with a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wallpaper {
    pub name: String,
    pub data: Vec<u8>,
}

/// A profile to keep: what Keeper Forge sends, already checked.
#[derive(Debug, Clone, PartialEq)]
pub struct NewProfile {
    pub id: String,
    pub name: String,
    pub settings: serde_json::Value,
    pub wallpaper: Option<Wallpaper>,
}

/// A kept profile without its picture's bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub settings: serde_json::Value,
    pub wallpaper_name: Option<String>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub updated_at: OffsetDateTime,
}

/// A profile and when it was last saved.
type KeptProfile = (NewProfile, OffsetDateTime);

/// A token's hash, the account it is for, what it is for, and when it ends.
type MailedToken = (String, String, TokenPurpose, OffsetDateTime);

/// Keeps everything in memory. Used by the tests, and by a Hub started without a database; the
/// data is gone when the program stops.
#[derive(Default)]
pub struct MemoryStore {
    heartbeats: Mutex<HashMap<(String, StickId), SeenHeartbeat>>,
    boot_seqs: Mutex<HashMap<(String, StickId, String), u64>>,
    events: Mutex<HashMap<(String, StickId), Vec<StoredEvent>>>,
    judge_submissions: Mutex<Vec<(String, String, JudgeSubmission)>>,
    users: Mutex<Vec<User>>,
    sessions: Mutex<HashMap<String, (String, OffsetDateTime)>>,
    email_tokens: Mutex<Vec<MailedToken>>,
    invitations: Mutex<Vec<(Invitation, String)>>,
    organizations: Mutex<Vec<Organization>>,
    members: Mutex<Vec<(String, String, Role)>>,
    next_id: AtomicU64,
    sticks: Mutex<HashMap<StickId, (String, Stick)>>,
    profiles: Mutex<HashMap<(String, String), KeptProfile>>,
    commands: Mutex<Vec<(String, StoredCommand)>>,
    /// Per code hash: the organization, what it was minted for and whether it is used.
    enrolment_codes: Mutex<HashMap<String, KeptCode>>,
    recordings: Mutex<Vec<(String, Recording)>>,
}

struct KeptCode {
    organization: String,
    code: NewEnrolmentCode,
    used: bool,
}

impl MemoryStore {
    fn id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}

impl Store for MemoryStore {
    fn record_heartbeat(
        &self,
        organization: &str,
        heartbeat: Heartbeat,
        received_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        {
            let mut boots = self.boot_seqs.lock().expect("boots lock");
            let top = boots
                .entry((
                    organization.to_owned(),
                    heartbeat.stick_id.clone(),
                    heartbeat.boot_id.clone(),
                ))
                .or_insert(0);
            *top = (*top).max(heartbeat.seq);
        }
        let key = (organization.to_owned(), heartbeat.stick_id.clone());
        let mut heartbeats = self.heartbeats.lock().expect("heartbeats lock");
        let count = heartbeats.get(&key).map_or(0, |seen| seen.count) + 1;
        heartbeats.insert(
            key,
            SeenHeartbeat {
                heartbeat,
                received_at,
                count,
            },
        );
        Ok(())
    }

    fn last_heartbeat(
        &self,
        organization: &str,
        stick: &StickId,
    ) -> Result<Option<SeenHeartbeat>, StoreError> {
        Ok(self
            .heartbeats
            .lock()
            .expect("heartbeats lock")
            .get(&(organization.to_owned(), stick.clone()))
            .cloned())
    }

    fn heartbeat_seq_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<u64>, StoreError> {
        Ok(self
            .boot_seqs
            .lock()
            .expect("boots lock")
            .get(&(organization.to_owned(), stick.clone(), boot.to_owned()))
            .copied())
    }

    fn last_event_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<StoredEvent>, StoreError> {
        Ok(self
            .events
            .lock()
            .expect("events lock")
            .get(&(organization.to_owned(), stick.clone()))
            .and_then(|all| all.iter().rev().find(|s| s.event.boot_id == boot))
            .cloned())
    }

    fn append_event(
        &self,
        organization: &str,
        event: Event,
        received_at: OffsetDateTime,
        integrity: Option<String>,
    ) -> Result<(), StoreError> {
        self.events
            .lock()
            .expect("events lock")
            .entry((organization.to_owned(), event.stick_id.clone()))
            .or_default()
            .push(StoredEvent {
                received_at,
                event,
                integrity,
            });
        Ok(())
    }

    fn events(&self, organization: &str, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError> {
        Ok(self
            .events
            .lock()
            .expect("events lock")
            .get(&(organization.to_owned(), stick.clone()))
            .cloned()
            .unwrap_or_default())
    }

    fn add_judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        submissions: &[JudgeSubmission],
        _received_at: OffsetDateTime,
    ) -> Result<usize, StoreError> {
        let mut kept = self.judge_submissions.lock().expect("judge lock");
        let mut added = 0;
        for submission in submissions {
            let known = kept.iter().any(|(org, kind, old)| {
                org == organization
                    && kind == judge
                    && old.contest_id == submission.contest_id
                    && old.submission_id == submission.submission_id
            });
            if !known {
                kept.push((
                    organization.to_owned(),
                    judge.to_owned(),
                    submission.clone(),
                ));
                added += 1;
            }
        }
        Ok(added)
    }

    fn judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        contest_id: u64,
    ) -> Result<Vec<JudgeSubmission>, StoreError> {
        let kept = self.judge_submissions.lock().expect("judge lock");
        let mut found: Vec<JudgeSubmission> = kept
            .iter()
            .filter(|(org, kind, s)| {
                org == organization && kind == judge && s.contest_id == contest_id
            })
            .map(|(_, _, s)| s.clone())
            .collect();
        found.sort_by_key(|s| s.submitted_at);
        Ok(found)
    }

    fn submission_events(&self, organization: &str) -> Result<Vec<SubmitEvent>, StoreError> {
        let events = self.events.lock().expect("events lock");
        let mut all: Vec<&StoredEvent> = events
            .iter()
            .filter(|((org, _), _)| org == organization)
            .flat_map(|(_, list)| list.iter())
            .collect();
        all.sort_by_key(|stored| stored.received_at);
        Ok(all.into_iter().filter_map(submit_event).collect())
    }

    fn create_user(&self, user: NewUser) -> Result<Option<User>, StoreError> {
        let mut users = self.users.lock().expect("users lock");
        if users
            .iter()
            .any(|known| known.email.eq_ignore_ascii_case(&user.email))
        {
            return Ok(None);
        }
        let user = User {
            id: self.id("user"),
            email: user.email,
            password_hash: user.password_hash,
            email_verified_at: user.email_verified_at,
            created_at: user.created_at,
            is_platform_staff: false,
        };
        users.push(user.clone());
        Ok(Some(user))
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        let users = self.users.lock().expect("users lock");
        Ok(users
            .iter()
            .find(|user| user.email.eq_ignore_ascii_case(email))
            .cloned())
    }

    fn user(&self, id: &str) -> Result<Option<User>, StoreError> {
        let users = self.users.lock().expect("users lock");
        Ok(users.iter().find(|user| user.id == id).cloned())
    }

    fn create_organization(
        &self,
        slug: &str,
        name: &str,
    ) -> Result<Option<Organization>, StoreError> {
        let mut organizations = self.organizations.lock().expect("organizations lock");
        if organizations.iter().any(|known| known.slug == slug) {
            return Ok(None);
        }
        let organization = Organization {
            id: self.id("org"),
            slug: slug.to_owned(),
            name: name.to_owned(),
        };
        organizations.push(organization.clone());
        Ok(Some(organization))
    }

    fn ensure_organization(&self, slug: &str, name: &str) -> Result<Organization, StoreError> {
        if let Some(found) = self.organization_by_slug(slug)? {
            return Ok(found);
        }
        // Another thread may have made it between the two steps.
        match self.create_organization(slug, name)? {
            Some(made) => Ok(made),
            None => self
                .organization_by_slug(slug)?
                .ok_or_else(|| StoreError("an organization vanished".to_owned())),
        }
    }

    fn organization_by_slug(&self, slug: &str) -> Result<Option<Organization>, StoreError> {
        let organizations = self.organizations.lock().expect("organizations lock");
        Ok(organizations.iter().find(|o| o.slug == slug).cloned())
    }

    fn add_member(
        &self,
        organization_id: &str,
        user_id: &str,
        role: Role,
    ) -> Result<bool, StoreError> {
        let mut members = self.members.lock().expect("members lock");
        if members
            .iter()
            .any(|(org, user, _)| org == organization_id && user == user_id)
        {
            return Ok(false);
        }
        members.push((organization_id.to_owned(), user_id.to_owned(), role));
        Ok(true)
    }

    fn membership(&self, organization_id: &str, user_id: &str) -> Result<Option<Role>, StoreError> {
        let members = self.members.lock().expect("members lock");
        Ok(members
            .iter()
            .find(|(org, user, _)| org == organization_id && user == user_id)
            .map(|(_, _, role)| *role))
    }

    fn memberships(&self, user_id: &str) -> Result<Vec<Membership>, StoreError> {
        let members = self.members.lock().expect("members lock");
        let organizations = self.organizations.lock().expect("organizations lock");
        let mut found: Vec<Membership> = members
            .iter()
            .filter(|(_, user, _)| user == user_id)
            .filter_map(|(org, _, role)| {
                organizations
                    .iter()
                    .find(|o| &o.id == org)
                    .map(|organization| Membership {
                        organization: organization.clone(),
                        role: *role,
                    })
            })
            .collect();
        found.sort_by(|a, b| a.organization.slug.cmp(&b.organization.slug));
        Ok(found)
    }

    fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .insert(token_hash.to_owned(), (user_id.to_owned(), expires_at));
        Ok(())
    }

    fn session(&self, token_hash: &str) -> Result<Option<(String, OffsetDateTime)>, StoreError> {
        Ok(self
            .sessions
            .lock()
            .expect("sessions lock")
            .get(token_hash)
            .cloned())
    }

    fn delete_session(&self, token_hash: &str) -> Result<(), StoreError> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .remove(token_hash);
        Ok(())
    }

    fn delete_expired_sessions(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .retain(|_, (_, ends)| *ends > now);
        Ok(())
    }

    fn mark_email_verified(&self, user_id: &str, at: OffsetDateTime) -> Result<(), StoreError> {
        let mut users = self.users.lock().expect("users lock");
        if let Some(user) = users.iter_mut().find(|user| user.id == user_id) {
            user.email_verified_at.get_or_insert(at);
        }
        Ok(())
    }

    fn set_password_hash(&self, user_id: &str, password_hash: &str) -> Result<(), StoreError> {
        let mut users = self.users.lock().expect("users lock");
        if let Some(user) = users.iter_mut().find(|user| user.id == user_id) {
            password_hash.clone_into(&mut user.password_hash);
        }
        Ok(())
    }

    fn delete_sessions_of(&self, user_id: &str) -> Result<(), StoreError> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .retain(|_, (owner, _)| owner != user_id);
        Ok(())
    }

    fn put_email_token(
        &self,
        token_hash: &str,
        user_id: &str,
        purpose: TokenPurpose,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let mut tokens = self.email_tokens.lock().expect("tokens lock");
        tokens.retain(|(_, owner, kind, _)| !(owner == user_id && *kind == purpose));
        tokens.push((
            token_hash.to_owned(),
            user_id.to_owned(),
            purpose,
            expires_at,
        ));
        Ok(())
    }

    fn take_email_token(
        &self,
        token_hash: &str,
        purpose: TokenPurpose,
        now: OffsetDateTime,
    ) -> Result<Option<String>, StoreError> {
        let mut tokens = self.email_tokens.lock().expect("tokens lock");
        // A token of another purpose is left alone, as in PostgreSQL.
        let Some(at) = tokens
            .iter()
            .position(|(hash, _, kind, _)| hash == token_hash && *kind == purpose)
        else {
            return Ok(None);
        };
        let (_, owner, _, ends) = tokens.remove(at);
        Ok((ends > now).then_some(owner))
    }

    fn delete_expired_email_tokens(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.email_tokens
            .lock()
            .expect("tokens lock")
            .retain(|(_, _, _, ends)| *ends > now);
        Ok(())
    }

    fn delete_unverified_users_before(&self, before: OffsetDateTime) -> Result<(), StoreError> {
        let mut users = self.users.lock().expect("users lock");
        let gone: Vec<String> = users
            .iter()
            .filter(|user| user.email_verified_at.is_none() && user.created_at < before)
            .map(|user| user.id.clone())
            .collect();
        users.retain(|user| !gone.contains(&user.id));
        self.email_tokens
            .lock()
            .expect("tokens lock")
            .retain(|(_, owner, _, _)| !gone.contains(owner));
        self.sessions
            .lock()
            .expect("sessions lock")
            .retain(|_, (owner, _)| !gone.contains(owner));
        Ok(())
    }

    fn put_invitation(
        &self,
        organization_id: &str,
        email: &str,
        role: Role,
        token_hash: &str,
        invited_by: Option<&str>,
        expires_at: OffsetDateTime,
    ) -> Result<Invitation, StoreError> {
        let mut invitations = self.invitations.lock().expect("invitations lock");
        invitations.retain(|(open, _)| {
            !(open.organization_id == organization_id && open.email.eq_ignore_ascii_case(email))
        });
        let invitation = Invitation {
            id: self.id("invite"),
            organization_id: organization_id.to_owned(),
            email: email.to_owned(),
            role,
            invited_by: invited_by.map(str::to_owned),
            expires_at,
        };
        invitations.push((invitation.clone(), token_hash.to_owned()));
        Ok(invitation)
    }

    fn invitation_for_token(&self, token_hash: &str) -> Result<Option<Invitation>, StoreError> {
        let invitations = self.invitations.lock().expect("invitations lock");
        Ok(invitations
            .iter()
            .find(|(_, hash)| hash == token_hash)
            .map(|(invitation, _)| invitation.clone()))
    }

    fn take_invitation(
        &self,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Invitation>, StoreError> {
        let mut invitations = self.invitations.lock().expect("invitations lock");
        let Some(at) = invitations.iter().position(|(_, hash)| hash == token_hash) else {
            return Ok(None);
        };
        let (invitation, _) = invitations.remove(at);
        Ok((invitation.expires_at > now).then_some(invitation))
    }

    fn invitations_of(&self, organization_id: &str) -> Result<Vec<Invitation>, StoreError> {
        let invitations = self.invitations.lock().expect("invitations lock");
        let mut open: Vec<Invitation> = invitations
            .iter()
            .filter(|(invitation, _)| invitation.organization_id == organization_id)
            .map(|(invitation, _)| invitation.clone())
            .collect();
        open.sort_by_key(|a| a.email.to_lowercase());
        Ok(open)
    }

    fn delete_invitation(&self, organization_id: &str, id: &str) -> Result<bool, StoreError> {
        let mut invitations = self.invitations.lock().expect("invitations lock");
        let before = invitations.len();
        invitations.retain(|(invitation, _)| {
            !(invitation.organization_id == organization_id && invitation.id == id)
        });
        Ok(invitations.len() < before)
    }

    fn delete_expired_invitations(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.invitations
            .lock()
            .expect("invitations lock")
            .retain(|(invitation, _)| invitation.expires_at > now);
        Ok(())
    }

    fn members_of(&self, organization_id: &str) -> Result<Vec<Member>, StoreError> {
        let users = self.users.lock().expect("users lock");
        let members = self.members.lock().expect("members lock");
        let mut found: Vec<Member> = members
            .iter()
            .filter(|(org, _, _)| org == organization_id)
            .filter_map(|(_, user_id, role)| {
                users
                    .iter()
                    .find(|user| &user.id == user_id)
                    .map(|user| Member {
                        user_id: user_id.clone(),
                        email: user.email.clone(),
                        role: *role,
                    })
            })
            .collect();
        found.sort_by_key(|a| a.email.to_lowercase());
        Ok(found)
    }

    fn remove_member(&self, organization_id: &str, user_id: &str) -> Result<bool, StoreError> {
        let mut members = self.members.lock().expect("members lock");
        let before = members.len();
        members.retain(|(org, user, _)| !(org == organization_id && user == user_id));
        Ok(members.len() < before)
    }

    fn count_owners(&self, organization_id: &str) -> Result<usize, StoreError> {
        let members = self.members.lock().expect("members lock");
        Ok(members
            .iter()
            .filter(|(org, _, role)| org == organization_id && *role == Role::Owner)
            .count())
    }

    fn set_staff(&self, user_id: &str, staff: bool) -> Result<(), StoreError> {
        let mut users = self.users.lock().expect("users lock");
        if let Some(user) = users.iter_mut().find(|user| user.id == user_id) {
            user.is_platform_staff = staff;
        }
        Ok(())
    }

    fn organizations(&self) -> Result<Vec<Organization>, StoreError> {
        let mut all = self
            .organizations
            .lock()
            .expect("organizations lock")
            .clone();
        all.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(all)
    }

    fn add_stick(&self, organization: &str, stick: Stick) -> Result<bool, StoreError> {
        let mut sticks = self.sticks.lock().expect("sticks lock");
        if sticks.contains_key(&stick.stick_id) {
            return Ok(false);
        }
        sticks.insert(stick.stick_id.clone(), (organization.to_owned(), stick));
        Ok(true)
    }

    fn stick(&self, id: &StickId) -> Result<Option<(String, Stick)>, StoreError> {
        Ok(self.sticks.lock().expect("sticks lock").get(id).cloned())
    }

    fn sticks(&self, organization: &str) -> Result<Vec<Stick>, StoreError> {
        Ok(self
            .sticks
            .lock()
            .expect("sticks lock")
            .values()
            .filter(|(owner, _)| owner == organization)
            .map(|(_, stick)| stick.clone())
            .collect())
    }

    fn put_profile(
        &self,
        organization: &str,
        profile: NewProfile,
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.profiles.lock().expect("profiles lock").insert(
            (organization.to_owned(), profile.id.clone()),
            (profile, now),
        );
        Ok(())
    }

    fn profiles(&self, organization: &str) -> Result<Vec<ProfileSummary>, StoreError> {
        let mut all: Vec<ProfileSummary> = self
            .profiles
            .lock()
            .expect("profiles lock")
            .iter()
            .filter(|((owner, _), _)| owner == organization)
            .map(|(_, (profile, at))| ProfileSummary {
                id: profile.id.clone(),
                name: profile.name.clone(),
                updated_at: *at,
            })
            .collect();
        all.sort_by_key(|p| (p.name.to_lowercase(), p.id.clone()));
        Ok(all)
    }

    fn profile(&self, organization: &str, id: &str) -> Result<Option<Profile>, StoreError> {
        Ok(self
            .profiles
            .lock()
            .expect("profiles lock")
            .get(&(organization.to_owned(), id.to_owned()))
            .map(|(profile, at)| Profile {
                id: profile.id.clone(),
                name: profile.name.clone(),
                settings: profile.settings.clone(),
                wallpaper_name: profile.wallpaper.as_ref().map(|w| w.name.clone()),
                updated_at: *at,
            }))
    }

    fn profile_wallpaper(
        &self,
        organization: &str,
        id: &str,
    ) -> Result<Option<Wallpaper>, StoreError> {
        Ok(self
            .profiles
            .lock()
            .expect("profiles lock")
            .get(&(organization.to_owned(), id.to_owned()))
            .and_then(|(profile, _)| profile.wallpaper.clone()))
    }

    fn delete_profile(&self, organization: &str, id: &str) -> Result<bool, StoreError> {
        Ok(self
            .profiles
            .lock()
            .expect("profiles lock")
            .remove(&(organization.to_owned(), id.to_owned()))
            .is_some())
    }

    fn queue_command(&self, organization: &str, command: StoredCommand) -> Result<(), StoreError> {
        self.commands
            .lock()
            .expect("commands lock")
            .push((organization.to_owned(), command));
        Ok(())
    }

    fn pending_commands(
        &self,
        organization: &str,
        stick: &StickId,
        now: OffsetDateTime,
    ) -> Result<Vec<StoredCommand>, StoreError> {
        let mut open: Vec<StoredCommand> = self
            .commands
            .lock()
            .expect("commands lock")
            .iter()
            .filter(|(owner, c)| {
                owner == organization
                    && &c.signed.command.stick_id == stick
                    && matches!(c.state, CommandState::Queued | CommandState::Delivered)
                    && c.expires_at() > now
            })
            .map(|(_, c)| c.clone())
            .collect();
        open.sort_by_key(|c| c.created_at);
        Ok(open)
    }

    fn mark_commands_delivered(
        &self,
        organization: &str,
        ids: &[String],
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        for (owner, command) in self.commands.lock().expect("commands lock").iter_mut() {
            if owner == organization
                && command.state == CommandState::Queued
                && ids.iter().any(|id| id == command.id())
            {
                command.state = CommandState::Delivered;
                command.delivered_at = Some(now);
            }
        }
        Ok(())
    }

    fn record_command_ack(
        &self,
        organization: &str,
        stick: &StickId,
        ack: &CommandAck,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut commands = self.commands.lock().expect("commands lock");
        let Some((_, command)) = commands.iter_mut().find(|(owner, c)| {
            owner == organization && c.id() == ack.command_id && &c.signed.command.stick_id == stick
        }) else {
            return Ok(false);
        };
        if !matches!(
            command.state,
            CommandState::Queued | CommandState::Delivered
        ) {
            return Ok(false);
        }
        let (state, refusal) = settled(ack);
        command.state = state;
        command.refusal = refusal;
        command.finished_at = Some(now);
        Ok(true)
    }

    fn commands_of(
        &self,
        organization: &str,
        stick: &StickId,
        limit: u32,
    ) -> Result<Vec<StoredCommand>, StoreError> {
        let mut all: Vec<StoredCommand> = self
            .commands
            .lock()
            .expect("commands lock")
            .iter()
            .filter(|(owner, c)| owner == organization && &c.signed.command.stick_id == stick)
            .map(|(_, c)| c.clone())
            .collect();
        all.sort_by_key(|c| std::cmp::Reverse(c.created_at));
        all.truncate(limit as usize);
        Ok(all)
    }

    fn command(&self, organization: &str, id: &str) -> Result<Option<StoredCommand>, StoreError> {
        Ok(self
            .commands
            .lock()
            .expect("commands lock")
            .iter()
            .find(|(owner, c)| owner == organization && c.id() == id)
            .map(|(_, c)| c.clone()))
    }

    fn delete_commands_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.commands
            .lock()
            .expect("commands lock")
            .retain(|(owner, c)| owner != organization || c.created_at >= before);
        Ok(())
    }

    fn add_enrolment_code(
        &self,
        organization: &str,
        code: NewEnrolmentCode,
    ) -> Result<(), StoreError> {
        self.enrolment_codes.lock().expect("codes lock").insert(
            code.code_hash.clone(),
            KeptCode {
                organization: organization.to_owned(),
                code,
                used: false,
            },
        );
        Ok(())
    }

    fn redeem_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError> {
        let mut codes = self.enrolment_codes.lock().expect("codes lock");
        let Some(kept) = codes.get_mut(code_hash) else {
            return Ok(Redemption::Invalid);
        };
        if kept.used || kept.code.expires_at <= now {
            return Ok(Redemption::Invalid);
        }
        let mut sticks = self.sticks.lock().expect("sticks lock");
        if sticks.contains_key(stick) {
            return Ok(Redemption::IdTaken);
        }
        kept.used = true;
        let made = Stick {
            stick_id: stick.clone(),
            token: token.clone(),
            credential_scope: kept.code.credential_scope.clone(),
            team: kept.code.team.clone(),
            site: kept.code.site.clone(),
            contest: kept.code.contest.clone(),
            machine_number: kept.code.machine_number,
        };
        sticks.insert(stick.clone(), (kept.organization.clone(), made));
        Ok(Redemption::Done(EnrolledStick {
            organization: kept.organization.clone(),
            team: kept.code.team.clone(),
            site: kept.code.site.clone(),
            contest: kept.code.contest.clone(),
            machine_number: kept.code.machine_number,
        }))
    }

    fn delete_enrolment_codes_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.enrolment_codes
            .lock()
            .expect("codes lock")
            .retain(|_, kept| kept.organization != organization || kept.code.created_at >= before);
        Ok(())
    }

    fn add_recording(&self, organization: &str, recording: &Recording) -> Result<bool, StoreError> {
        let mut all = self.recordings.lock().expect("recordings lock");
        if all.iter().any(|(org, r)| {
            org == organization && r.stick_id == recording.stick_id && r.file == recording.file
        }) {
            return Ok(false);
        }
        all.push((organization.to_owned(), recording.clone()));
        Ok(true)
    }

    fn recording(&self, organization: &str, id: &str) -> Result<Option<Recording>, StoreError> {
        Ok(self
            .recordings
            .lock()
            .expect("recordings lock")
            .iter()
            .find(|(org, r)| org == organization && r.id == id)
            .map(|(_, r)| r.clone()))
    }

    fn recordings_of(
        &self,
        organization: &str,
        stick: &StickId,
        limit: u32,
    ) -> Result<Vec<Recording>, StoreError> {
        let stick = stick.to_string();
        let mut found: Vec<Recording> = self
            .recordings
            .lock()
            .expect("recordings lock")
            .iter()
            .filter(|(org, r)| org == organization && r.stick_id == stick)
            .map(|(_, r)| r.clone())
            .collect();
        found.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        found.truncate(limit as usize);
        Ok(found)
    }

    fn advance_recording(
        &self,
        organization: &str,
        id: &str,
        index: u32,
    ) -> Result<bool, StoreError> {
        let mut all = self.recordings.lock().expect("recordings lock");
        for (org, r) in all.iter_mut() {
            if org == organization && r.id == id && !r.is_complete() && r.next_chunk == index {
                r.next_chunk = index + 1;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn complete_recording(
        &self,
        organization: &str,
        id: &str,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut all = self.recordings.lock().expect("recordings lock");
        for (org, r) in all.iter_mut() {
            if org == organization && r.id == id && !r.is_complete() {
                r.completed_at = Some(now);
                r.expires_at = Some(expires_at);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn due_recordings(
        &self,
        organization: &str,
        now: OffsetDateTime,
        abandoned_before: OffsetDateTime,
    ) -> Result<Vec<Recording>, StoreError> {
        Ok(self
            .recordings
            .lock()
            .expect("recordings lock")
            .iter()
            .filter(|(org, r)| {
                org == organization
                    && if r.is_complete() {
                        r.expires_at.is_some_and(|end| end < now)
                    } else {
                        r.created_at < abandoned_before
                    }
            })
            .take(200)
            .map(|(_, r)| r.clone())
            .collect())
    }

    fn delete_recording(&self, organization: &str, id: &str) -> Result<(), StoreError> {
        self.recordings
            .lock()
            .expect("recordings lock")
            .retain(|(org, r)| org != organization || r.id != id);
        Ok(())
    }
}
