//! What the Hub knows about the sticks, and the rules for accepting a heartbeat.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use ck_protocol::{
    Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, SignedCommand, StickId, Token,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use crate::accounts::{Accounts, PasswordCost};
use crate::codeforces::{CodeforcesError, Judge};
use crate::contest::{self, ContestView};
use crate::credentials::CredentialScope;
use crate::download::Download;
use crate::follow::Follows;
use crate::images::ImageSource;
use crate::judge::{JudgeKind, JudgeSubmission, SubmitEvent};
use crate::mail::Mailer;
use crate::network::{self, NetworkView};
use crate::recordings::Blobs;
use crate::registry::Registry;
use crate::store::{BootSpan, Stick, Store, StoreError, StoredEvent};
use crate::submissions::{self, SubmissionsView};

/// A stick that has sent nothing for longer than this is shown as offline.
pub const ONLINE_WINDOW_SECONDS: i64 = 60;

#[derive(Deserialize)]
struct ContestAssertion {
    contest_id: Option<serde_json::Value>,
}

/// A stick sends a heartbeat every few seconds and a few events; this leaves room for a burst and
/// stops a stick with a leaked secret from filling the database.
const MAX_MESSAGES_PER_MINUTE: u32 = 120;

/// Events kept for one boot of one stick: its sequence number may not pass this. The message limit
/// of 120 a minute fills it after about 7 hours; a longer boot is refused with a 429, which a stick
/// keeps queued and tries again later, so nothing is lost on its side.
pub const MAX_EVENTS_PER_BOOT: u64 = 50_000;

/// Events kept for one stick over all its boots, so a stick that boots again and again, or a stolen
/// secret, cannot fill the database. The Hub counts them per stick in memory (read once from the
/// store), so the check is exact for every event and costs no query.
pub const MAX_EVENTS_PER_STICK: u64 = 200_000;

/// How many locks the sticks share to keep their own messages in order.
const WRITE_LOCKS: usize = 64;

/// Why a heartbeat was not accepted. The caller turns this into an HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The message itself is wrong: a missing header, unreadable JSON, or a heartbeat for another
    /// stick than the one that signed it.
    BadRequest(&'static str),
    /// The stick is unknown or the signature does not match. The two look the same from outside, so
    /// nobody can use the answer to find out which stick ids exist.
    Unauthorized,
    /// A message that is not newer than the last one of this boot.
    Conflict(String),
    /// One stick sent more than [`MAX_MESSAGES_PER_MINUTE`] messages in a minute.
    TooManyRequests,
    /// The Hub could not keep or read the data. The stick tries again later.
    Unavailable,
}

impl From<StoreError> for Rejection {
    fn from(error: StoreError) -> Self {
        tracing::error!("{error}");
        Self::Unavailable
    }
}

/// The state of every stick, as the page and the API show it.
#[derive(Debug, Serialize, PartialEq)]
pub struct StickView {
    pub stick_id: String,
    pub team: String,
    pub site: String,
    pub contest: String,
    pub machine_number: Option<u32>,
    /// `false` while the stick was created in Forge and has not yet claimed its secret on first boot.
    pub activated: bool,
    /// `false` until the first heartbeat, after [`ONLINE_WINDOW_SECONDS`] of silence, or after
    /// the credential is retired.
    pub online: bool,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub retired_at: Option<OffsetDateTime>,
    /// Set while the stick is archived: it keeps its data, its credential does not work, and the
    /// Hub deletes it at `delete_after`.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub archived_at: Option<OffsetDateTime>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub delete_after: Option<OffsetDateTime>,
    /// The disk this stick was written to, when Forge reported it.
    pub hardware: Option<HardwareView>,
    pub last: Option<LastHeartbeat>,
    /// Events received from this stick since the Hub started.
    pub events: usize,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_event_at: Option<OffsetDateTime>,
    /// Who logged in last during the stick's latest boot, if anyone did.
    pub current_user: Option<String>,
    /// What the stick's proxy carried, and how much of it was refused as a site not allowed.
    pub network_requests: usize,
    pub blocked_requests: usize,
    /// Submissions the stick reported since the Hub started, and the latest of them.
    pub submissions: usize,
    pub last_submission: Option<SubmissionView>,
    /// How many of this stick's submissions do not agree with the judge: ones the judge does not
    /// show, ones on the judge the stick did not report, a time or a code that differs. Worked out
    /// from what the Hub keeps, never by asking the judge, so the list stays quick.
    pub submission_problems: usize,
    /// Things an organizer should look at: a chain problem, two users on one stick.
    pub warnings: Vec<String>,
}

/// What the Hub knows about the physical disk of a stick.
#[derive(Debug, Serialize, PartialEq)]
pub struct HardwareView {
    /// The start of the disk's fingerprint, to tell disks apart.
    pub id: String,
    /// The last characters of the disk's serial number.
    pub serial_hint: String,
    /// How many times this disk was written. More than one means it was rewritten.
    pub write_count: u32,
    pub rewritten: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub last_written_at: OffsetDateTime,
}

/// What a submission event says, read loosely: the shape of the payload is fixed by the capture
/// rule of each platform, and a field a rule does not send is just left out.
#[derive(Debug, Serialize, PartialEq)]
pub struct SubmissionView {
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
    pub problem: Option<String>,
    pub language: Option<String>,
    pub submission_id: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct LastHeartbeat {
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
    pub seconds_ago: i64,
    pub boot_id: String,
    pub seq: u64,
    pub uptime_s: u64,
    pub hostname: String,
    pub image_version: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub screen_unlock: bool,
    pub recorder: String,
    /// How many heartbeats were accepted from this stick since the Hub started.
    pub heartbeats: u64,
}

/// Per judge, contest and handle: when the judge was asked and what it said.
type JudgedCache = HashMap<(JudgeKind, u64, String), (OffsetDateTime, Vec<JudgeSubmission>)>;

pub struct Hub {
    pub(crate) registry: Mutex<Registry>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) accounts: Accounts,
    /// One message of a stick at a time is read, judged and kept, so two heartbeats of one stick
    /// cannot both pass the sequence check. Sticks share a fixed set of locks by their id, so
    /// different sticks are almost never held up by each other.
    write: Vec<Mutex<()>>,
    /// Per stick: when the current minute started and how many messages it has seen.
    rates: Mutex<HashMap<StickId, (OffsetDateTime, u32)>>,
    /// Per user: when the current minute of commands started and how many they sent in it.
    pub(crate) command_rates: Mutex<HashMap<String, (OffsetDateTime, u32)>>,
    /// Per user: when the current minute of settings signing started and how many they had signed.
    pub(crate) sign_rates: Mutex<HashMap<String, (OffsetDateTime, u32)>>,
    /// The key stick settings are signed with, when the Hub has one.
    pub(crate) config_key: Option<ck_crypto::ConfigSigningKey>,
    /// Per user: enrolment codes made in the current minute.
    pub(crate) enrol_mints: Mutex<HashMap<String, (OffsetDateTime, u32)>>,
    /// Failed enrolments of everyone together: when the current minute started and how many.
    pub(crate) enrol_failures: Mutex<(OffsetDateTime, u32)>,
    /// Per stick: how many events are stored, read from the store at the first event after a start.
    event_totals: Mutex<HashMap<StickId, u64>>,
    log: Option<Mutex<File>>,
    default_org: OnceLock<String>,
    pub(crate) download: Option<Download>,
    /// Where recordings are kept and for how many days, once set up.
    pub(crate) recordings: Option<(Arc<dyn Blobs>, i64)>,
    /// Per stick: when the current minute of upload calls started and how many it has seen.
    pub(crate) chunk_rates: Mutex<HashMap<StickId, (OffsetDateTime, u32)>>,
    /// The images organizers may download, once set up.
    pub(crate) images: Option<Arc<dyn ImageSource>>,
    /// How mail is sent and the address its links start with, once set up.
    pub(crate) mailer: Option<(Arc<dyn Mailer>, String)>,
    pub(crate) clock: Box<dyn Fn() -> OffsetDateTime + Send + Sync>,
    /// Where the submissions of a person in a contest are asked, once set up.
    pub(crate) judges: HashMap<JudgeKind, Arc<dyn Judge>>,
    /// The people the Hub keeps asking the judges about.
    pub(crate) follows: Follows,
    /// What the judge answered lately, so a page that refreshes does not ask again each time.
    judged: Mutex<JudgedCache>,
}

impl Hub {
    /// Lets the Hub hand out files from a private container through `/download/...` links.
    #[must_use]
    pub fn with_download(mut self, download: Download) -> Self {
        self.download = Some(download);
        self
    }

    /// Lets organizers list and download the images the source names. It also needs a download
    /// container, or the links cannot be made.
    #[must_use]
    pub fn with_images(mut self, images: Arc<dyn ImageSource>) -> Self {
        self.images = Some(images);
        self
    }

    #[must_use]
    pub fn new(
        registry: Registry,
        store: Arc<dyn Store>,
        log: Option<File>,
        clock: Box<dyn Fn() -> OffsetDateTime + Send + Sync>,
    ) -> Self {
        Self {
            registry: Mutex::new(registry),
            store,
            accounts: Accounts::new(PasswordCost::default()),
            write: (0..WRITE_LOCKS).map(|_| Mutex::new(())).collect(),
            rates: Mutex::new(HashMap::new()),
            command_rates: Mutex::new(HashMap::new()),
            enrol_mints: Mutex::new(HashMap::new()),
            enrol_failures: Mutex::new((OffsetDateTime::UNIX_EPOCH, 0)),
            event_totals: Mutex::new(HashMap::new()),
            log: log.map(Mutex::new),
            default_org: OnceLock::new(),
            download: None,
            recordings: None,
            chunk_rates: Mutex::new(HashMap::new()),
            images: None,
            mailer: None,
            clock,
            judges: HashMap::new(),
            follows: Follows::default(),
            judged: Mutex::new(HashMap::new()),
            sign_rates: Mutex::new(HashMap::new()),
            config_key: None,
        }
    }

    /// Lets the Hub ask the judge what people submitted, to compare with what sticks reported.
    #[must_use]
    pub fn with_judge(mut self, judge: Arc<dyn Judge>) -> Self {
        self.judges.insert(JudgeKind::Codeforces, judge);
        self
    }

    /// Lets the Hub ask `VJudge` the same, for the sticks used there.
    #[must_use]
    pub fn with_vjudge(mut self, judge: Arc<dyn Judge>) -> Self {
        self.judges.insert(JudgeKind::Vjudge, judge);
        self
    }

    /// Lets the Hub send mail. `public_url` is the address people reach the Hub at: the links in
    /// the mail start with it.
    #[must_use]
    pub fn with_mail(mut self, mailer: Arc<dyn Mailer>, public_url: &str) -> Self {
        self.mailer = Some((mailer, public_url.trim_end_matches('/').to_owned()));
        self
    }

    /// Changes how much work hashing a password takes, for a machine that can afford more or less.
    #[must_use]
    pub fn with_password_cost(mut self, cost: PasswordCost) -> Self {
        self.accounts = Accounts::new(cost);
        self
    }

    /// The organization the registry file's sticks belong to, made on first use.
    pub(crate) fn default_org_id(&self) -> Result<String, StoreError> {
        if let Some(id) = self.default_org.get() {
            return Ok(id.clone());
        }
        let organization = self.store.ensure_organization("default", "Default")?;
        Ok(self.default_org.get_or_init(|| organization.id).clone())
    }

    /// Checks a heartbeat and, when it is fine, remembers it. Returns its sequence number.
    pub fn accept_heartbeat(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        self.accept_heartbeat_reply(stick_header, signature, body)
            .map(|(seq, _)| seq)
    }

    /// Like [`accept_heartbeat`](Self::accept_heartbeat), and also the commands waiting for the stick,
    /// signed, for the reply. They are marked as delivered, and sent again with every heartbeat until
    /// the stick acknowledges them or they expire.
    pub fn accept_heartbeat_reply(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<(u64, Vec<SignedCommand>), Rejection> {
        let result = self.check(stick_header, signature, body);
        let logged = result
            .as_ref()
            .map(|(seq, _, _, _)| *seq)
            .map_err(Clone::clone);
        self.record(&logged, "accepted", "sequence not newer", stick_header);
        let (seq, organization, id, screen_unlock) = result?;
        // A command the Hub cannot read is not worth refusing the heartbeat: the stick would then
        // look offline. It is picked up with the next one.
        let commands = self
            .commands_for_reply(&organization, &id, screen_unlock)
            .unwrap_or_default();
        Ok((seq, commands))
    }

    fn check(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<(u64, String, StickId, bool), Rejection> {
        let (organization, id, _) =
            self.authenticate(stick_header, signature, body, Duration::ZERO)?;
        self.rate_limit(&id)?;
        let heartbeat: Heartbeat = serde_json::from_slice(body)
            .map_err(|_| Rejection::BadRequest("the heartbeat cannot be read"))?;
        if heartbeat.stick_id != id {
            return Err(Rejection::BadRequest("the heartbeat is for another stick"));
        }
        storable(body, heartbeat.seq)?;

        let _one_at_a_time = self.write_lock(&id);
        let seq = heartbeat.seq;
        let screen_unlock = heartbeat.screen_unlock;
        if let Some(top) = self
            .store
            .record_heartbeat(&organization, heartbeat, (self.clock)())?
        {
            return Err(Rejection::Conflict(format!(
                "sequence {seq} is not newer than {top}"
            )));
        }
        Ok((seq, organization, id, screen_unlock))
    }

    /// The lock that keeps messages of one stick in order. Another stick may share it; that only
    /// makes it wait, never fail.
    fn write_lock(&self, id: &StickId) -> std::sync::MutexGuard<'_, ()> {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(id.as_str());
        let at = usize::try_from(hash % self.write.len() as u64).unwrap_or(0);
        self.write[at].lock().expect("write lock")
    }

    /// Counts a message of `id` in the current minute and refuses it past the limit.
    fn rate_limit(&self, id: &StickId) -> Result<(), Rejection> {
        let now = (self.clock)();
        let mut rates = self.rates.lock().expect("rates lock");
        let (since, count) = rates.entry(id.clone()).or_insert((now, 0));
        if (now - *since).whole_seconds() >= 60 {
            (*since, *count) = (now, 0);
        }
        *count += 1;
        if *count > MAX_MESSAGES_PER_MINUTE {
            return Err(Rejection::TooManyRequests);
        }
        Ok(())
    }

    /// Who sent this: the stick named in the header, if the registry knows it and its secret
    /// signed exactly these bytes. Also the organization the stick belongs to.
    pub(crate) fn authenticate(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
        grace: Duration,
    ) -> Result<(String, StickId, Option<CredentialScope>), Rejection> {
        let (Some(stick_header), Some(signature)) = (stick_header, signature) else {
            return Err(Rejection::BadRequest("a header is missing"));
        };
        let id = StickId::parse(stick_header)
            .map_err(|_| Rejection::BadRequest("the stick header is not a stick id"))?;
        let received_at = (self.clock)();
        let Some((organization, token, scope)) = self.stick_secret(&id)? else {
            return Err(Rejection::Unauthorized);
        };
        if !ck_crypto::verify(&token, body, signature) {
            return Err(Rejection::Unauthorized);
        }
        if let Some(scope) = &scope {
            if !scope.accepts_at(received_at, grace) {
                return Err(Rejection::Unauthorized);
            }
            // Recording chunks, status and completion sign fixed text, with no contest claim.
            if body.trim_ascii_start().starts_with(b"{") {
                let claim: ContestAssertion = serde_json::from_slice(body)
                    .map_err(|_| Rejection::BadRequest("the contest identity cannot be read"))?;
                if !scope.agrees_with(claim.contest_id.as_ref()) {
                    return Err(Rejection::Unauthorized);
                }
            }
        }
        Ok((organization, id, scope))
    }

    /// The organization a stick belongs to and its credential. A stored retirement always blocks
    /// an older copy in the trusted file. The file's sticks belong to the default organization.
    pub(crate) fn stick_secret(
        &self,
        id: &StickId,
    ) -> Result<Option<(String, Token, Option<CredentialScope>)>, StoreError> {
        let stored = self.store.stick(id)?;
        // A pre-registered stick has an empty secret until its first boot, so it must never pass
        // HMAC authentication before claiming a real credential.
        if stored.as_ref().is_some_and(|(_, stick)| {
            stick.token.expose().is_empty()
                || stick.retired_at.is_some()
                || stick.archived_at.is_some()
                || stick.deleted_at.is_some()
        }) {
            return Ok(None);
        }
        let from_file = {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            registry
                .find(id)
                .map(|entry| (entry.token.clone(), entry.credential_scope.clone()))
        };
        Ok(match from_file {
            Some((token, scope)) => Some((self.default_org_id()?, token, scope)),
            None => stored
                .map(|(organization, stick)| (organization, stick.token, stick.credential_scope)),
        })
    }

    pub(crate) fn retire_stick(&self, organization: &str, id: &str) -> Result<bool, StoreError> {
        let Ok(id) = StickId::parse(id) else {
            return Ok(false);
        };
        let file = self.registry_entry(organization, &id)?;
        self.store
            .retire_stick(organization, &id, file.as_ref(), (self.clock)())
    }

    /// The registry file's entry for the stick, when the organization is the one the file belongs to.
    fn registry_entry(
        &self,
        organization: &str,
        id: &StickId,
    ) -> Result<Option<Stick>, StoreError> {
        let file = {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            registry.find(id).cloned()
        };
        Ok(match file {
            Some(entry) if organization == self.default_org_id()? => Some(entry),
            _ => None,
        })
    }

    /// Archives a stick for `days`: it stops being accepted and is deleted when the time is up,
    /// unless it is restored first. Returns false for a stick that is unknown or already deleted.
    pub(crate) fn archive_stick(
        &self,
        organization: &str,
        id: &str,
        days: i64,
    ) -> Result<bool, StoreError> {
        let Ok(id) = StickId::parse(id) else {
            return Ok(false);
        };
        let file = self.registry_entry(organization, &id)?;
        let now = (self.clock)();
        self.store.archive_stick(
            organization,
            &id,
            file.as_ref(),
            now,
            now + Duration::days(days),
        )
    }

    /// Brings an archived stick back. Returns false when it is not archived.
    pub(crate) fn restore_stick(&self, organization: &str, id: &str) -> Result<bool, StoreError> {
        let Ok(id) = StickId::parse(id) else {
            return Ok(false);
        };
        self.store.restore_stick(organization, &id)
    }

    /// Deletes an archived stick and everything kept about it, the recording files included. Only
    /// an archived stick can be deleted. Returns false when it is not archived.
    ///
    /// # Errors
    /// [`StoreError`] when a recording file cannot be removed from storage (nothing else is
    /// removed then, and the call can be repeated) or the store fails.
    pub(crate) fn delete_stick(&self, organization: &str, id: &str) -> Result<bool, StoreError> {
        let Ok(id) = StickId::parse(id) else {
            return Ok(false);
        };
        let archived = self.store.stick(&id)?.is_some_and(|(owner, stick)| {
            owner == organization && stick.archived_at.is_some() && stick.deleted_at.is_none()
        });
        if !archived {
            return Ok(false);
        }
        if let Some((blobs, _)) = self.recordings.as_ref() {
            for recording in self.store.recordings_of(organization, &id, u32::MAX)? {
                blobs.delete(&recording.blob(organization)).map_err(|_| {
                    StoreError("a recording could not be removed from storage".to_owned())
                })?;
            }
        }
        self.store.delete_stick(organization, &id, (self.clock)())
    }

    /// Deletes the archived sticks whose time is up. Returns how many it deleted.
    ///
    /// # Errors
    /// [`StoreError`] when the organizations or their sticks cannot be read.
    pub fn purge_archived_sticks(&self) -> Result<usize, StoreError> {
        let now = (self.clock)();
        let mut removed = 0;
        for organization in self.store.organizations()? {
            for id in self.store.sticks_due_for_deletion(&organization.id, now)? {
                match self.delete_stick(&organization.id, id.as_str()) {
                    Ok(true) => removed += 1,
                    Ok(false) => {}
                    // One stick that cannot be removed now does not hold up the others.
                    Err(error) => tracing::warn!("an archived stick could not be deleted: {error}"),
                }
            }
        }
        Ok(removed)
    }

    /// Takes in an event. A problem with the chain never makes the Hub refuse it: the stick would
    /// send it again for ever and block everything behind it. The problem is kept with the event
    /// and shown as a warning, because a gap or a changed event is the thing worth seeing.
    ///
    /// An event the Hub already has is a [`Rejection::Conflict`], which the stick counts as sent.
    pub fn accept_event(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let result = self.check_event(stick_header, signature, body);
        self.record(
            &result,
            "event accepted",
            "event already received",
            stick_header,
        );
        result
    }

    fn check_event(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let (organization, id, scope) =
            self.authenticate(stick_header, signature, body, Duration::ZERO)?;
        self.rate_limit(&id)?;
        let event: Event = serde_json::from_slice(body)
            .map_err(|_| Rejection::BadRequest("the event cannot be read"))?;
        if event.stick_id != id {
            return Err(Rejection::BadRequest("the event is for another stick"));
        }
        if scope.is_some_and(|scope| !scope.agrees_with(event.payload.get("contest_id"))) {
            return Err(Rejection::Unauthorized);
        }
        storable(body, event.seq)?;

        let _one_at_a_time = self.write_lock(&id);
        let last_of_boot = self
            .store
            .last_event_of_boot(&organization, &id, &event.boot_id)?;
        let last_of_boot = last_of_boot.as_ref();
        // A repeat is answered as a conflict whatever the budgets say: the stick counts that as sent,
        // while a 429 would keep it queued.
        if last_of_boot.is_some_and(|last| event.seq <= last.event.seq) {
            return Err(Rejection::Conflict(format!(
                "event {} was already received",
                event.seq
            )));
        }
        if event.seq > MAX_EVENTS_PER_BOOT
            || self.stored_events(&organization, &id)? >= MAX_EVENTS_PER_STICK
        {
            return Err(Rejection::TooManyRequests);
        }
        let integrity = match last_of_boot {
            Some(last) if event.seq > last.event.seq + 1 => {
                Some("an event is missing before this one")
            }
            Some(last)
                if event.prev_hash != ck_crypto::sha256_hex(&last.event.canonical_bytes()) =>
            {
                Some("this event does not follow the one before it")
            }
            // The first event of a boot, or the first the Hub has seen after it restarted.
            None if event.seq == 1 && event.prev_hash == FIRST_PREV_HASH => None,
            None if event.seq == 1 => Some("the first event of a boot names a previous one"),
            None => Some("the Hub has no earlier events of this boot"),
            Some(_) => None,
        };
        let seq = event.seq;
        let ack_event = (event.kind == EventKind::CommandAck).then(|| event.clone());
        let follow = StoredEvent {
            received_at: (self.clock)(),
            event: event.clone(),
            integrity: None,
        };
        let stored = match self.store.append_event(
            &organization,
            event,
            (self.clock)(),
            integrity.map(str::to_owned),
        ) {
            Ok(stored) => stored,
            Err(error) => {
                // The write may have gone through before the error: count again from the store.
                self.event_totals.lock().expect("totals lock").remove(&id);
                return Err(error.into());
            }
        };
        // A copy the database dropped (another process won the race) changes nothing.
        if stored {
            if let Some(total) = self.event_totals.lock().expect("totals lock").get_mut(&id) {
                *total += 1;
            }
            self.follow_event(&organization, &follow);
            if let Some(event) = ack_event {
                self.apply_ack(&organization, &id, &event);
            }
        }
        Ok(seq)
    }

    /// How many events the stick has stored. The caller holds the stick's write lock.
    fn stored_events(&self, organization: &str, id: &StickId) -> Result<u64, StoreError> {
        if let Some(total) = self.event_totals.lock().expect("totals lock").get(id) {
            return Ok(*total);
        }
        let total = self.store.event_count(organization, id)?;
        self.event_totals
            .lock()
            .expect("totals lock")
            .insert(id.clone(), total);
        Ok(total)
    }

    /// The whole network history of one stick of the organization, or `None` for an id that is not
    /// one. A stick of another organization has no events here, so its history is empty.
    pub fn network_of(
        &self,
        organization: &str,
        stick: &str,
    ) -> Result<Option<NetworkView>, StoreError> {
        let Ok(id) = StickId::parse(stick) else {
            return Ok(None);
        };
        let stored = self.store.events(organization, &id)?;
        Ok(Some(network::view(&stored)))
    }

    /// What the stick reported as submitted, and what the judge says the same people submitted.
    /// The fields are Codeforces; `vjudge` holds the same for `VJudge` when the stick was used there.
    pub fn submissions_of(
        &self,
        organization: &str,
        stick: &str,
    ) -> Result<Option<SubmissionsView>, StoreError> {
        let Ok(id) = StickId::parse(stick) else {
            return Ok(None);
        };
        let stored = self.store.events(organization, &id)?;
        let now = (self.clock)();
        let mut view = self.submissions_for(organization, JudgeKind::Codeforces, &stored, now);
        let vjudge = self.submissions_for(organization, JudgeKind::Vjudge, &stored, now);
        view.vjudge = (!vjudge.handles.is_empty() || !vjudge.submissions.is_empty())
            .then(|| Box::new(vjudge));
        Ok(Some(view))
    }

    fn submissions_for(
        &self,
        organization: &str,
        kind: JudgeKind,
        stored: &[StoredEvent],
        now: OffsetDateTime,
    ) -> SubmissionsView {
        let ask = |contest: u64, handle: &str| {
            let judge = self
                .judges
                .get(&kind)
                .expect("asked only when there is a judge");
            let key = (kind, contest, handle.to_lowercase());
            if let Ok(cache) = self.judged.lock()
                && let Some((at, found)) = cache.get(&key)
                && now - *at < Duration::seconds(30)
            {
                return Ok(found.clone());
            }
            let found = match judge.submissions_of(contest, handle) {
                Ok(found) => found,
                // The watcher keeps what the judge said, so a judge that is down for a while does
                // not blank the page. With nothing kept the judge's own error is told.
                Err(error) => {
                    // Only this person's rows count: with none of them the outage is still told.
                    let mut kept = self
                        .store
                        .judge_submissions(organization, kind.key(), contest)
                        .unwrap_or_default();
                    kept.retain(|s| s.handles.iter().any(|h| h.eq_ignore_ascii_case(handle)));
                    if kept.is_empty() {
                        return Err(error);
                    }
                    return Ok(kept);
                }
            };
            if let Ok(mut cache) = self.judged.lock() {
                cache.insert(key, (now, found.clone()));
            }
            Ok(found)
        };
        let ask: Option<&submissions::Ask<'_>> = if self.judges.contains_key(&kind) {
            Some(&ask)
        } else {
            None
        };
        submissions::view_of(kind, stored, ask, now)
    }

    /// Every submission of one contest of a judge the Hub has kept, with the stick that reported
    /// it and whether the time agrees.
    pub fn contest_submissions(
        &self,
        organization: &str,
        kind: JudgeKind,
        contest_id: u64,
    ) -> Result<ContestView, StoreError> {
        let judged = self
            .store
            .judge_submissions(organization, kind.key(), contest_id)?;
        let events: Vec<SubmitEvent> = self
            .store
            .submission_events(organization)?
            .into_iter()
            .filter(|event| event.judge == kind && event.contest_id == contest_id && event.accepted)
            .collect();
        let mut view = contest::view(contest_id, &judged, &events);
        view.site = kind.site();
        Ok(view)
    }

    fn stick_entries(&self, organization: &str) -> Result<Vec<Stick>, StoreError> {
        // The registry file's sticks belong to the default organization.
        let mut entries: Vec<Stick> = if organization == self.default_org_id()? {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            registry.all().cloned().collect()
        } else {
            Vec::new()
        };
        for stick in self.store.sticks(organization)? {
            if stick.deleted_at.is_some() {
                // A deleted stick leaves a tombstone, so the registry file cannot bring it back.
                entries.retain(|entry| entry.stick_id != stick.stick_id);
            } else if let Some(entry) = entries
                .iter_mut()
                .find(|entry| entry.stick_id == stick.stick_id)
            {
                entry.retired_at = stick.retired_at;
                entry.archived_at = stick.archived_at;
                entry.delete_after = stick.delete_after;
            } else {
                entries.push(stick);
            }
        }
        // The order an organizer reads them in: by contest, then machine number.
        entries.sort_by(|a, b| {
            (&a.contest, a.machine_number, a.stick_id.as_str()).cmp(&(
                &b.contest,
                b.machine_number,
                b.stick_id.as_str(),
            ))
        });
        Ok(entries)
    }

    /// Every stick of the organization, with its last heartbeat when it has sent one.
    pub fn sticks(&self, organization: &str) -> Result<Vec<StickView>, StoreError> {
        let now = (self.clock)();
        let entries = self.stick_entries(organization)?;
        let hardware = self.store.hardware(organization)?;
        let expected = self.store.expected_configs(organization)?;
        let (kept, mut views) = (KeptJudge::for_hub(self, organization), Vec::new());
        for entry in &entries {
            let seen = self.store.last_heartbeat(organization, &entry.stick_id)?;
            let stored = self.store.events(organization, &entry.stick_id)?;
            let (network_requests, blocked_requests) = network::counts(&stored);
            let boot = seen.as_ref().map(|s| s.heartbeat.boot_id.clone());
            let reported = seen.as_ref().map(|s| s.heartbeat.config_sha256.clone());
            let trust = seen.as_ref().and_then(|s| s.heartbeat.config_trust);
            let wrote = expected
                .iter()
                .find(|(id, _)| *id == entry.stick_id)
                .map(|(_, sha256)| sha256.as_str());
            let submission_problems = kept.problems(&stored, now);
            let last = seen.map(|s| LastHeartbeat {
                received_at: s.received_at,
                seconds_ago: (now - s.received_at).whole_seconds().max(0),
                boot_id: s.heartbeat.boot_id.clone(),
                seq: s.heartbeat.seq,
                uptime_s: s.heartbeat.uptime_s,
                hostname: s.heartbeat.hostname.clone(),
                image_version: s.heartbeat.image_version.clone(),
                screen_unlock: s.heartbeat.screen_unlock,
                recorder: recorder_text(s.heartbeat.recording.state).to_owned(),
                heartbeats: s.count,
            });
            views.push(StickView {
                stick_id: entry.stick_id.to_string(),
                team: entry.team.clone(),
                site: entry.site.clone(),
                contest: entry.contest.clone(),
                machine_number: entry.machine_number,
                activated: !entry.token.expose().is_empty(),
                online: !entry.token.expose().is_empty()
                    && entry.retired_at.is_none()
                    && entry.archived_at.is_none()
                    && last
                        .as_ref()
                        .is_some_and(|l| l.seconds_ago <= ONLINE_WINDOW_SECONDS),
                retired_at: entry.retired_at,
                archived_at: entry.archived_at,
                delete_after: entry.delete_after,
                hardware: hardware
                    .iter()
                    .find(|row| row.current_stick_id.as_ref() == Some(&entry.stick_id))
                    .map(|row| HardwareView {
                        id: row.fingerprint.chars().take(8).collect(),
                        serial_hint: row.serial_hint.clone(),
                        write_count: row.write_count,
                        rewritten: row.write_count > 1,
                        last_written_at: row.last_written_at,
                    }),
                last,
                events: stored.len(),
                last_event_at: stored.last().map(|s| s.received_at),
                current_user: users_of_boot(&stored, boot.as_deref()).pop(),
                network_requests,
                blocked_requests,
                submissions: stored.iter().filter(|s| taken(s)).count(),
                last_submission: last_submission(&stored),
                submission_problems,
                warnings: {
                    let mut found = warnings(&stored, boot.as_deref());
                    found.extend(problem_warning(submission_problems));
                    found.extend(self.two_places(organization, &entry.stick_id, now)?);
                    found.extend(config_warning(wrote, reported.as_deref()));
                    found.extend(trust_warning(trust));
                    found
                },
            });
        }
        Ok(views)
    }

    /// One line per message in the log file: what happened and to which stick, never the secret,
    /// the signature or the body. `accepted` and `conflict` are the words for those two outcomes.
    fn record(
        &self,
        result: &Result<u64, Rejection>,
        accepted: &'static str,
        conflict: &'static str,
        stick: Option<&str>,
    ) {
        let (outcome, seq) = match result {
            Ok(seq) => (accepted, Some(*seq)),
            Err(Rejection::BadRequest(why)) => (*why, None),
            Err(Rejection::Unauthorized) => ("unknown stick or bad signature", None),
            Err(Rejection::Conflict(_)) => (conflict, None),
            Err(Rejection::TooManyRequests) => ("too many messages from this stick", None),
            Err(Rejection::Unavailable) => ("the store failed", None),
        };
        tracing::info!(outcome, stick, seq, "heartbeat");
        let Some(log) = &self.log else { return };
        let line = json!({
            "at": (self.clock)(),
            "outcome": outcome,
            "stick": stick,
            "seq": seq,
        });
        if let Ok(mut file) = log.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

fn recorder_text(state: RecorderState) -> &'static str {
    match state {
        RecorderState::Off => "off",
        RecorderState::Waiting => "waiting",
        RecorderState::Recording => "recording",
        RecorderState::LowDisk => "low disk",
    }
}

/// Refuses what PostgreSQL could not keep, so the stick is told once instead of retrying for ever:
/// a sequence above `i64::MAX`, or text with a NUL character.
fn storable(body: &[u8], seq: u64) -> Result<(), Rejection> {
    if i64::try_from(seq).is_err() || body.windows(6).any(|w| w == b"\\u0000") {
        return Err(Rejection::BadRequest("a value cannot be stored"));
    }
    Ok(())
}

/// The user names of the logins in `boot` (the boot of the last heartbeat, or of the last event
/// when there is no heartbeat), in order of login and without repeats of the same name in a row.
fn users_of_boot(stored: &[StoredEvent], boot: Option<&str>) -> Vec<String> {
    let Some(boot) = boot.or_else(|| stored.last().map(|s| s.event.boot_id.as_str())) else {
        return Vec::new();
    };
    let mut users: Vec<String> = Vec::new();
    for item in stored.iter().filter(|s| s.event.boot_id == boot) {
        if item.event.kind != EventKind::Login {
            continue;
        }
        if let Some(name) = item.event.payload.get("username").and_then(|v| v.as_str())
            && users.last().map(String::as_str) != Some(name)
        {
            users.push(name.to_owned());
        }
    }
    users
}

/// A submission the judge took. One the stick saw refused, for example while the site asked for a
/// check first, is not one; an event without the field counts as taken.
fn taken(item: &StoredEvent) -> bool {
    item.event.kind == EventKind::Submission
        && item.event.payload.get("accepted") != Some(&serde_json::Value::Bool(false))
}

fn last_submission(stored: &[StoredEvent]) -> Option<SubmissionView> {
    let item = stored.iter().rev().find(|s| taken(s))?;
    // A text, or a number as text; anything else is left out.
    let text = |key: &str| match item.event.payload.get(key)? {
        serde_json::Value::String(value) => Some(value.chars().take(64).collect()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    };
    Some(SubmissionView {
        received_at: item.received_at,
        problem: text("problem"),
        language: text("language"),
        submission_id: text("submission_id"),
    })
}

/// Why an organizer should look at this stick.
/// A warning when the stick says it refused a settings file. It went on with its earlier settings,
/// or with the image's own defaults, so what it does now is not what the file says.
/// What the watcher kept of the judge, read once per contest while a list is built, so the list of
/// sticks never asks the judge and does not read the store again for each stick.
struct KeptJudge<'a> {
    store: &'a dyn Store,
    organization: &'a str,
    /// Without a live judge the details page compares nothing, so the list counts nothing either.
    live: Vec<JudgeKind>,
    rows: RefCell<HashMap<(JudgeKind, u64), Vec<JudgeSubmission>>>,
}

impl<'a> KeptJudge<'a> {
    fn for_hub(hub: &'a Hub, organization: &'a str) -> Self {
        Self::new(
            &*hub.store,
            organization,
            JudgeKind::ALL
                .into_iter()
                .filter(|kind| hub.judges.contains_key(kind))
                .collect(),
        )
    }

    fn new(store: &'a dyn Store, organization: &'a str, live: Vec<JudgeKind>) -> Self {
        Self {
            store,
            organization,
            live,
            rows: RefCell::default(),
        }
    }

    fn rows(&self, kind: JudgeKind, contest: u64) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        let mut known = self.rows.borrow_mut();
        if let Some(rows) = known.get(&(kind, contest)) {
            return Ok(rows.clone());
        }
        let rows = self
            .store
            .judge_submissions(self.organization, kind.key(), contest)
            .map_err(|error| CodeforcesError::Request(error.to_string()))?;
        // No watcher for this contest: nothing is known, so nothing is flagged.
        if rows.is_empty() {
            return Err(CodeforcesError::Request(
                submissions::NOTHING_KEPT.to_owned(),
            ));
        }
        known.insert((kind, contest), rows.clone());
        Ok(rows)
    }

    /// How many submissions of a stick disagree with what the watcher kept of the judges.
    fn problems(&self, stored: &[StoredEvent], now: OffsetDateTime) -> usize {
        self.live
            .iter()
            .map(|kind| {
                let seen = submissions::view_of(
                    *kind,
                    stored,
                    Some(&|contest, _| self.rows(*kind, contest)),
                    now,
                );
                seen.suspect + seen.not_on_judge
            })
            .sum()
    }
}

fn problem_warning(problems: usize) -> Option<String> {
    (problems > 0).then(|| format!("{problems} submission(s) do not agree with the judge."))
}

fn trust_warning(trust: Option<ck_protocol::ConfigTrust>) -> Option<String> {
    (trust == Some(ck_protocol::ConfigTrust::Rejected)).then(|| {
        "Settings refused: this stick refused a settings file the Hub did not sign (edited, unsigned or made for another stick) and kept its earlier settings.".to_owned()
    })
}

/// A warning when the stick reports another settings file than the one Forge wrote. Nothing is said
/// for a stick Forge did not tell the Hub about, or one that has not sent a heartbeat yet.
fn config_warning(wrote: Option<&str>, reported: Option<&str>) -> Option<String> {
    let (wrote, reported) = (wrote?, reported?);
    if reported.eq_ignore_ascii_case(wrote) {
        return None;
    }
    let what = if reported.is_empty() {
        "reports no settings file"
    } else {
        "reports settings other than the ones Forge wrote"
    };
    Some(format!(
        "Settings changed: this stick {what}. The file was edited, or the stick was written by another tool."
    ))
}

/// How long after the last heartbeat of a boot the Hub still counts it as live when it looks for a
/// second machine using the same stick credential.
const TWO_PLACES_WINDOW_SECONDS: i64 = 300;

impl Hub {
    fn two_places(
        &self,
        organization: &str,
        stick: &StickId,
        now: OffsetDateTime,
    ) -> Result<Option<String>, StoreError> {
        let since = now - Duration::seconds(TWO_PLACES_WINDOW_SECONDS);
        Ok(two_places_warning(&self.store.boot_spans(
            organization,
            stick,
            since,
        )?))
    }
}

/// A stick is on one machine at a time: a reboot ends the old boot before the new one starts. Two
/// boots that were both heard from, with their first-to-last times overlapping, mean the credential
/// is in use on two machines (a cloned disk or a copied secret).
fn two_places_warning(spans: &[BootSpan]) -> Option<String> {
    let overlapping = spans.iter().enumerate().any(|(index, a)| {
        spans[index + 1..]
            .iter()
            .any(|b| a.first_seen < b.last_seen && b.first_seen < a.last_seen)
    });
    overlapping.then(|| {
        "two machines are sending heartbeats as this stick at the same time: its credential is in use in two places"
            .to_owned()
    })
}

fn warnings(stored: &[StoredEvent], boot: Option<&str>) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut distinct: Vec<String> = users_of_boot(stored, boot);
    distinct.sort();
    distinct.dedup();
    if distinct.len() > 1 {
        found.push(format!(
            "{} different users logged in during one boot: {}",
            distinct.len(),
            distinct.join(", ")
        ));
    }
    for item in stored {
        if let Some(problem) = &item.integrity {
            let text = format!(
                "event {} of boot {}: {problem}",
                item.event.seq, item.event.boot_id
            );
            if !found.contains(&text) {
                found.push(text);
            }
        }
    }
    found
}
