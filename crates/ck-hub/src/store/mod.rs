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
use crate::enrol::{CLAIM_RECOVERY, EnrolledStick, NewEnrolmentCode, Redemption};
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

/// The physical disk a stick was written to, as Forge reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Hardware {
    /// A SHA-256 (64 hex digits) of the disk's serial number and size.
    pub fingerprint: String,
    /// The last characters of the serial number, for a person to match with a stick in hand.
    #[serde(default)]
    pub serial_hint: String,
}

/// What the Hub keeps about one physical disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickHardware {
    pub fingerprint: String,
    pub serial_hint: String,
    /// How many times the disk was written for this organization.
    pub write_count: u32,
    pub first_written_at: OffsetDateTime,
    pub last_written_at: OffsetDateTime,
    /// The stick id of the latest write, once that stick enrolled or was registered.
    pub current_stick_id: Option<StickId>,
}

/// A stick the admin made. The Hub keeps its secret to check what the stick signs. It reads as
/// an entry of the admin tool's registry file, whose other fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Stick {
    pub stick_id: StickId,
    pub token: Token,
    #[serde(default)]
    pub credential_scope: Option<crate::CredentialScope>,
    #[serde(skip)]
    pub retired_at: Option<OffsetDateTime>,
    /// Set while the stick is archived: its credential does not work until it is restored.
    #[serde(skip)]
    pub archived_at: Option<OffsetDateTime>,
    /// When an archived stick is deleted for good.
    #[serde(skip)]
    pub delete_after: Option<OffsetDateTime>,
    /// Set on the tombstone a deleted stick leaves, so its id is not registered or shown again.
    #[serde(skip)]
    pub deleted_at: Option<OffsetDateTime>,
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

/// The first and last time a boot was heard from.
type FirstAndLast = (OffsetDateTime, OffsetDateTime);

/// When one boot of a stick was first and last heard from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootSpan {
    pub boot_id: String,
    pub first_seen: OffsetDateTime,
    pub last_seen: OffsetDateTime,
}

/// An event as the Hub received it, with what was wrong with its place in the chain, if anything.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub received_at: OffsetDateTime,
    pub event: Event,
    pub integrity: Option<String>,
}

/// What removing a member did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    Removed,
    NotMember,
    /// The member is the only owner, so nothing was removed.
    LastOwner,
}

/// Every method fails with [`StoreError`] when the data cannot be read or written.
pub trait Store: Send + Sync {
    /// Widens when a boot was heard from, for a heartbeat that came late through the queue and is
    /// older than one already recorded.
    fn saw_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
        at: OffsetDateTime,
    ) -> Result<(), StoreError>;

    /// Accepts a newer sequence for its boot, keeps it as the stick's latest heartbeat and counts
    /// it atomically. Returns the highest sequence of that boot when the heartbeat is a replay.
    fn record_heartbeat(
        &self,
        organization: &str,
        heartbeat: Heartbeat,
        received_at: OffsetDateTime,
    ) -> Result<Option<u64>, StoreError>;

    fn last_heartbeat(
        &self,
        organization: &str,
        stick: &StickId,
    ) -> Result<Option<SeenHeartbeat>, StoreError>;

    /// The boots of `stick` last heard from at or after `heard_since`, with the times they were
    /// first and last heard from. Boots recorded before the times were kept are left out.
    fn boot_spans(
        &self,
        organization: &str,
        stick: &StickId,
        heard_since: OffsetDateTime,
    ) -> Result<Vec<BootSpan>, StoreError>;

    /// The highest heartbeat sequence ever accepted from `stick` in `boot`, if any.
    fn heartbeat_seq_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<u64>, StoreError>;

    /// Adds an event after the ones already kept for its stick.
    /// Returns false when the store already had this event (same stick, boot and sequence).
    fn append_event(
        &self,
        organization: &str,
        event: Event,
        received_at: OffsetDateTime,
        integrity: Option<String>,
    ) -> Result<bool, StoreError>;

    /// The newest event of `stick` in `boot`, if any.
    fn last_event_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<StoredEvent>, StoreError>;

    /// Every event of the stick, in the order they were received.
    fn events(&self, organization: &str, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError>;

    /// How many events the stick has stored.
    fn event_count(&self, organization: &str, stick: &StickId) -> Result<u64, StoreError>;

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

    /// Removes the member, unless that would leave the organization without an owner. The check
    /// and the removal are one step, so two owners leaving at once cannot both succeed.
    fn remove_member(&self, organization_id: &str, user_id: &str) -> Result<Removal, StoreError>;

    fn set_staff(&self, user_id: &str, staff: bool) -> Result<(), StoreError>;

    /// Every organization, ordered by slug.
    fn organizations(&self) -> Result<Vec<Organization>, StoreError>;

    /// Adds a stick to an organization. Returns false, and changes nothing, when its id is
    /// already known to any organization: a stick's secret is never replaced, and an id belongs
    /// to one organization.
    fn add_stick(&self, organization: &str, stick: Stick) -> Result<bool, StoreError>;

    /// Retires a credential in its own organization, preserving the first retirement time.
    /// A matching entry from the trusted registry allows the store to retain a file-only stick.
    fn retire_stick(
        &self,
        organization: &str,
        id: &StickId,
        file: Option<&Stick>,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError>;

    /// Counts one more write of the disk and returns how many there are now. With `stick`, that
    /// stick id becomes the disk's current one. The rows are kept when a stick is deleted.
    fn record_hardware_write(
        &self,
        organization: &str,
        hardware: &Hardware,
        stick: Option<&StickId>,
        now: OffsetDateTime,
    ) -> Result<u32, StoreError>;

    /// Every disk the organization wrote sticks to.
    fn hardware(&self, organization: &str) -> Result<Vec<StickHardware>, StoreError>;

    /// Keeps the hash of the settings file Forge wrote for a stick, replacing an earlier one.
    fn record_expected_config(
        &self,
        organization: &str,
        stick: &StickId,
        sha256: &str,
        now: OffsetDateTime,
    ) -> Result<(), StoreError>;

    /// The settings hash Forge wrote for each stick of the organization that has one.
    fn expected_configs(&self, organization: &str) -> Result<Vec<(StickId, String)>, StoreError>;

    /// Archives a stick of the organization until `delete_after`. As with retiring, a matching
    /// entry from the trusted registry file lets the store keep a file-only stick. A stick that is
    /// deleted or already archived changes nothing. Returns whether the stick was found.
    fn archive_stick(
        &self,
        organization: &str,
        id: &StickId,
        file: Option<&Stick>,
        now: OffsetDateTime,
        delete_after: OffsetDateTime,
    ) -> Result<bool, StoreError>;

    /// Brings an archived stick back. Returns whether an archived stick was found.
    fn restore_stick(&self, organization: &str, id: &StickId) -> Result<bool, StoreError>;

    /// Archived sticks of the organization whose time is up at `now`.
    fn sticks_due_for_deletion(
        &self,
        organization: &str,
        now: OffsetDateTime,
    ) -> Result<Vec<StickId>, StoreError>;

    /// Deletes an archived stick and everything kept about it (heartbeats, events, commands,
    /// recording rows), leaving only a tombstone with its id. Returns false when the stick is not
    /// archived: only an archived stick can be deleted.
    fn delete_stick(
        &self,
        organization: &str,
        id: &StickId,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError>;

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

    /// Keeps a one-time enrolment code (its hash) for a stick of the organization. When
    /// `code.stick_id` is set, pre-registers that stick with an empty secret in the same step;
    /// returns false, and changes nothing, when that stick id is already taken.
    fn add_enrolment_code(
        &self,
        organization: &str,
        code: NewEnrolmentCode,
    ) -> Result<bool, StoreError>;

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

    /// As in normal redemption, with a bounded retry for the same unretired credential.
    fn claim_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError>;

    /// Removes the organization's old codes, keeping recent claims for recovery.
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

mod memory;

pub use memory::MemoryStore;
