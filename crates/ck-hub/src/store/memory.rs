//! The in-memory [`Store`]: for the tests and for a Hub started without a database.

use super::{
    AtomicU64, BootSpan, CLAIM_RECOVERY, CommandAck, CommandState, EnrolledStick, Event,
    FirstAndLast, Hardware, HashMap, Heartbeat, Invitation, JudgeSubmission, Member, Membership,
    Mutex, NewEnrolmentCode, NewProfile, NewUser, OffsetDateTime, Ordering, Organization, Profile,
    ProfileSummary, Recording, Redemption, Removal, Role, SeenHeartbeat, Stick, StickHardware,
    StickId, Store, StoreError, StoredCommand, StoredEvent, SubmitEvent, Token, TokenPurpose, User,
    Wallpaper, settled, submit_event,
};

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
    boot_times: Mutex<HashMap<(String, StickId, String), FirstAndLast>>,
    events: Mutex<HashMap<(String, StickId), Vec<StoredEvent>>>,
    /// (organization, stick, boot, seq) of every stored event, to drop a repeat as the database does.
    event_keys: Mutex<std::collections::HashSet<(String, StickId, String, u64)>>,
    judge_submissions: Mutex<Vec<(String, String, JudgeSubmission)>>,
    users: Mutex<Vec<User>>,
    sessions: Mutex<HashMap<String, (String, OffsetDateTime)>>,
    email_tokens: Mutex<Vec<MailedToken>>,
    invitations: Mutex<Vec<(Invitation, String)>>,
    organizations: Mutex<Vec<Organization>>,
    members: Mutex<Vec<(String, String, Role)>>,
    next_id: AtomicU64,
    sticks: Mutex<HashMap<StickId, (String, Stick)>>,
    hardware: Mutex<Vec<(String, StickHardware)>>,
    expected_configs: Mutex<HashMap<(String, StickId), String>>,
    profiles: Mutex<HashMap<(String, String), KeptProfile>>,
    commands: Mutex<Vec<(String, StoredCommand)>>,
    /// Per code hash: the organization, what it was minted for and whether it is used.
    enrolment_codes: Mutex<HashMap<String, KeptCode>>,
    recordings: Mutex<Vec<(String, Recording)>>,
}

struct KeptCode {
    organization: String,
    code: NewEnrolmentCode,
    used_at: Option<OffsetDateTime>,
    claimed_stick_id: Option<StickId>,
}

impl KeptCode {
    fn enrolled(&self) -> EnrolledStick {
        EnrolledStick {
            organization: self.organization.clone(),
            team: self.code.team.clone(),
            site: self.code.site.clone(),
            contest: self.code.contest.clone(),
            machine_number: self.code.machine_number,
        }
    }
}

/// Counts one more write of the disk and returns how many there are now.
fn count_hardware_write(
    rows: &mut Vec<(String, StickHardware)>,
    organization: &str,
    hardware: &Hardware,
    stick: Option<&StickId>,
    now: OffsetDateTime,
) -> u32 {
    let found = rows
        .iter_mut()
        .find(|(owner, row)| owner == organization && row.fingerprint == hardware.fingerprint);
    if let Some((_, row)) = found {
        row.write_count += 1;
        row.last_written_at = now;
        row.serial_hint.clone_from(&hardware.serial_hint);
        if stick.is_some() {
            row.current_stick_id = stick.cloned();
        }
        return row.write_count;
    }
    rows.push((
        organization.to_owned(),
        StickHardware {
            fingerprint: hardware.fingerprint.clone(),
            serial_hint: hardware.serial_hint.clone(),
            write_count: 1,
            first_written_at: now,
            last_written_at: now,
            current_stick_id: stick.cloned(),
        },
    ));
    1
}

impl MemoryStore {
    fn id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn use_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
        recoverable: bool,
    ) -> Redemption {
        let mut codes = self.enrolment_codes.lock().expect("codes lock");
        let Some(kept) = codes.get_mut(code_hash) else {
            return Redemption::Invalid;
        };
        if let Some(used_at) = kept.used_at {
            let recovering = recoverable
                && kept.claimed_stick_id.as_ref() == Some(stick)
                && used_at > now - CLAIM_RECOVERY;
            if recovering
                && self
                    .sticks
                    .lock()
                    .expect("sticks lock")
                    .get(stick)
                    .is_some_and(|(owner, stored)| {
                        owner == &kept.organization
                            && !stored.token.expose().is_empty()
                            && stored.retired_at.is_none()
                            && stored.archived_at.is_none()
                            && stored.deleted_at.is_none()
                            && &stored.token == token
                    })
            {
                return Redemption::Done(kept.enrolled());
            }
            return Redemption::Invalid;
        }
        if kept.code.expires_at <= now {
            return Redemption::Invalid;
        }
        let mut sticks = self.sticks.lock().expect("sticks lock");
        if let Some(expected) = &kept.code.stick_id {
            // A pre-registered stick may only activate its own empty secret via `/enrol/claim`.
            if !recoverable || stick != expected {
                return Redemption::Invalid;
            }
            let Some((owner, stored)) = sticks.get_mut(stick) else {
                return Redemption::Invalid;
            };
            if owner != &kept.organization
                || !stored.token.expose().is_empty()
                || stored.retired_at.is_some()
                || stored.archived_at.is_some()
                || stored.deleted_at.is_some()
            {
                return Redemption::Invalid;
            }
            stored.token = token.clone();
        } else {
            if sticks.contains_key(stick) {
                return Redemption::IdTaken;
            }
            let made = Stick {
                stick_id: stick.clone(),
                token: token.clone(),
                credential_scope: kept.code.credential_scope.clone(),
                retired_at: None,
                archived_at: None,
                delete_after: None,
                deleted_at: None,
                team: kept.code.team.clone(),
                site: kept.code.site.clone(),
                contest: kept.code.contest.clone(),
                machine_number: kept.code.machine_number,
            };
            sticks.insert(stick.clone(), (kept.organization.clone(), made));
        }
        kept.used_at = Some(now);
        kept.claimed_stick_id = recoverable.then(|| stick.clone());
        if let Some(hardware) = &kept.code.hardware {
            // The write counts now, when the stick that was written trades its code.
            let mut rows = self.hardware.lock().expect("hardware lock");
            count_hardware_write(&mut rows, &kept.organization, hardware, Some(stick), now);
        }
        Redemption::Done(kept.enrolled())
    }
}

impl Store for MemoryStore {
    fn record_heartbeat(
        &self,
        organization: &str,
        heartbeat: Heartbeat,
        received_at: OffsetDateTime,
    ) -> Result<Option<u64>, StoreError> {
        let mut boots = self.boot_seqs.lock().expect("boots lock");
        let boot = (
            organization.to_owned(),
            heartbeat.stick_id.clone(),
            heartbeat.boot_id.clone(),
        );
        if let Some(&top) = boots.get(&boot)
            && heartbeat.seq <= top
        {
            return Ok(Some(top));
        }
        boots.insert(boot.clone(), heartbeat.seq);
        self.boot_times
            .lock()
            .expect("boot times lock")
            .entry(boot)
            .and_modify(|(_, last)| *last = received_at)
            .or_insert((received_at, received_at));
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
        Ok(None)
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

    fn boot_spans(
        &self,
        organization: &str,
        stick: &StickId,
        heard_since: OffsetDateTime,
    ) -> Result<Vec<BootSpan>, StoreError> {
        Ok(self
            .boot_times
            .lock()
            .expect("boot times lock")
            .iter()
            .filter(|((org, id, _), (_, last))| {
                org == organization && id == stick && *last >= heard_since
            })
            .map(|((_, _, boot), (first, last))| BootSpan {
                boot_id: boot.clone(),
                first_seen: *first,
                last_seen: *last,
            })
            .collect())
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
    ) -> Result<bool, StoreError> {
        let mut events = self.events.lock().expect("events lock");
        let key = (
            organization.to_owned(),
            event.stick_id.clone(),
            event.boot_id.clone(),
            event.seq,
        );
        if !self.event_keys.lock().expect("event keys lock").insert(key) {
            return Ok(false);
        }
        events
            .entry((organization.to_owned(), event.stick_id.clone()))
            .or_default()
            .push(StoredEvent {
                received_at,
                event,
                integrity,
            });
        Ok(true)
    }

    fn event_count(&self, organization: &str, stick: &StickId) -> Result<u64, StoreError> {
        let events = self.events.lock().expect("events lock");
        Ok(events
            .get(&(organization.to_owned(), stick.clone()))
            .map_or(0, |kept| kept.len() as u64))
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

    fn remove_member(&self, organization_id: &str, user_id: &str) -> Result<Removal, StoreError> {
        let mut members = self.members.lock().expect("members lock");
        let owners = members
            .iter()
            .filter(|(org, _, role)| org == organization_id && *role == Role::Owner)
            .count();
        let is_owner = members.iter().any(|(org, user, role)| {
            org == organization_id && user == user_id && *role == Role::Owner
        });
        if is_owner && owners <= 1 {
            return Ok(Removal::LastOwner);
        }
        let before = members.len();
        members.retain(|(org, user, _)| !(org == organization_id && user == user_id));
        Ok(if members.len() < before {
            Removal::Removed
        } else {
            Removal::NotMember
        })
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

    fn retire_stick(
        &self,
        organization: &str,
        id: &StickId,
        file: Option<&Stick>,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut sticks = self.sticks.lock().expect("sticks lock");
        if let Some(file) = file.filter(|file| &file.stick_id == id) {
            sticks
                .entry(id.clone())
                .or_insert_with(|| (organization.to_owned(), file.clone()));
        }
        let Some((owner, stick)) = sticks.get_mut(id) else {
            return Ok(false);
        };
        if owner != organization || stick.deleted_at.is_some() {
            return Ok(false);
        }
        // Retiring a pre-registered stick that never activated (such as a cancelled Forge write)
        // tombstones it right away so no ghost stick stays on the dashboard.
        if stick.token.expose().is_empty() {
            *stick = Stick {
                stick_id: id.clone(),
                token: Token::new(String::new()),
                credential_scope: None,
                retired_at: Some(now),
                archived_at: None,
                delete_after: None,
                deleted_at: Some(now),
                team: String::new(),
                site: String::new(),
                contest: String::new(),
                machine_number: None,
            };
            drop(sticks);
            self.expected_configs
                .lock()
                .expect("expected configs lock")
                .remove(&(organization.to_owned(), id.clone()));
            return Ok(true);
        }
        stick.retired_at.get_or_insert(now);
        Ok(true)
    }

    fn record_hardware_write(
        &self,
        organization: &str,
        hardware: &Hardware,
        stick: Option<&StickId>,
        now: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let mut kept = self.hardware.lock().expect("hardware lock");
        Ok(count_hardware_write(
            &mut kept,
            organization,
            hardware,
            stick,
            now,
        ))
    }

    fn record_expected_config(
        &self,
        organization: &str,
        stick: &StickId,
        sha256: &str,
        _now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.expected_configs
            .lock()
            .expect("expected configs lock")
            .insert((organization.to_owned(), stick.clone()), sha256.to_owned());
        Ok(())
    }

    fn expected_configs(&self, organization: &str) -> Result<Vec<(StickId, String)>, StoreError> {
        Ok(self
            .expected_configs
            .lock()
            .expect("expected configs lock")
            .iter()
            .filter(|((owner, _), _)| owner == organization)
            .map(|((_, stick), sha256)| (stick.clone(), sha256.clone()))
            .collect())
    }

    fn hardware(&self, organization: &str) -> Result<Vec<StickHardware>, StoreError> {
        Ok(self
            .hardware
            .lock()
            .expect("hardware lock")
            .iter()
            .filter(|(owner, _)| owner == organization)
            .map(|(_, row)| row.clone())
            .collect())
    }

    fn archive_stick(
        &self,
        organization: &str,
        id: &StickId,
        file: Option<&Stick>,
        now: OffsetDateTime,
        delete_after: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut sticks = self.sticks.lock().expect("sticks lock");
        if let Some(file) = file.filter(|file| &file.stick_id == id) {
            sticks
                .entry(id.clone())
                .or_insert_with(|| (organization.to_owned(), file.clone()));
        }
        let Some((owner, stick)) = sticks.get_mut(id) else {
            return Ok(false);
        };
        if owner != organization || stick.deleted_at.is_some() {
            return Ok(false);
        }
        if stick.archived_at.is_none() {
            stick.archived_at = Some(now);
            stick.delete_after = Some(delete_after);
        }
        Ok(true)
    }

    fn restore_stick(&self, organization: &str, id: &StickId) -> Result<bool, StoreError> {
        let mut sticks = self.sticks.lock().expect("sticks lock");
        let Some((owner, stick)) = sticks.get_mut(id) else {
            return Ok(false);
        };
        if owner != organization || stick.archived_at.is_none() || stick.deleted_at.is_some() {
            return Ok(false);
        }
        stick.archived_at = None;
        stick.delete_after = None;
        Ok(true)
    }

    fn sticks_due_for_deletion(
        &self,
        organization: &str,
        now: OffsetDateTime,
    ) -> Result<Vec<StickId>, StoreError> {
        Ok(self
            .sticks
            .lock()
            .expect("sticks lock")
            .iter()
            .filter(|(_, (owner, stick))| {
                owner == organization
                    && stick.deleted_at.is_none()
                    && stick.delete_after.is_some_and(|after| after <= now)
            })
            .map(|(id, _)| id.clone())
            .collect())
    }

    fn delete_stick(
        &self,
        organization: &str,
        id: &StickId,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut sticks = self.sticks.lock().expect("sticks lock");
        let Some((owner, stick)) = sticks.get_mut(id) else {
            return Ok(false);
        };
        if owner != organization || stick.archived_at.is_none() || stick.deleted_at.is_some() {
            return Ok(false);
        }
        *stick = Stick {
            stick_id: id.clone(),
            token: Token::new(String::new()),
            credential_scope: None,
            retired_at: Some(now),
            archived_at: None,
            delete_after: None,
            deleted_at: Some(now),
            team: String::new(),
            site: String::new(),
            contest: String::new(),
            machine_number: None,
        };
        let key = (organization.to_owned(), id.clone());
        self.heartbeats
            .lock()
            .expect("heartbeats lock")
            .remove(&key);
        self.boot_seqs
            .lock()
            .expect("boots lock")
            .retain(|(org, stick, _), _| !(org == organization && stick == id));
        self.boot_times
            .lock()
            .expect("boot times lock")
            .retain(|(org, stick, _), _| !(org == organization && stick == id));
        let mut events = self.events.lock().expect("events lock");
        events.remove(&key);
        self.event_keys
            .lock()
            .expect("event keys lock")
            .retain(|(org, stick, _, _)| !(org == organization && stick == id));
        drop(events);
        self.expected_configs
            .lock()
            .expect("expected configs lock")
            .remove(&key);
        self.commands
            .lock()
            .expect("commands lock")
            .retain(|(org, command)| {
                !(org == organization && command.signed.command.stick_id == *id)
            });
        self.recordings
            .lock()
            .expect("recordings lock")
            .retain(|(org, recording)| !(org == organization && recording.stick_id == id.as_str()));
        for kept in self
            .enrolment_codes
            .lock()
            .expect("codes lock")
            .values_mut()
        {
            if kept.organization == organization {
                if kept.claimed_stick_id.as_ref() == Some(id) {
                    kept.claimed_stick_id = None;
                }
                if kept.code.stick_id.as_ref() == Some(id) {
                    kept.code.stick_id = None;
                }
            }
        }
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
    ) -> Result<bool, StoreError> {
        let mut codes = self.enrolment_codes.lock().expect("codes lock");
        if let Some(stick_id) = &code.stick_id {
            let mut sticks = self.sticks.lock().expect("sticks lock");
            if sticks.contains_key(stick_id) {
                return Ok(false);
            }
            sticks.insert(
                stick_id.clone(),
                (
                    organization.to_owned(),
                    Stick {
                        stick_id: stick_id.clone(),
                        token: Token::new(String::new()),
                        credential_scope: code.credential_scope.clone(),
                        retired_at: None,
                        archived_at: None,
                        delete_after: None,
                        deleted_at: None,
                        team: code.team.clone(),
                        site: code.site.clone(),
                        contest: code.contest.clone(),
                        machine_number: code.machine_number,
                    },
                ),
            );
        }
        codes.insert(
            code.code_hash.clone(),
            KeptCode {
                organization: organization.to_owned(),
                code,
                used_at: None,
                claimed_stick_id: None,
            },
        );
        Ok(true)
    }

    fn redeem_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError> {
        Ok(self.use_enrolment_code(code_hash, now, stick, token, false))
    }

    fn claim_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError> {
        Ok(self.use_enrolment_code(code_hash, now, stick, token, true))
    }

    fn delete_enrolment_codes_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.enrolment_codes
            .lock()
            .expect("codes lock")
            .retain(|_, kept| {
                kept.organization != organization
                    || kept.code.created_at >= before
                    || (kept.claimed_stick_id.is_some()
                        && kept.used_at.is_some_and(|used| used >= before))
            });
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
