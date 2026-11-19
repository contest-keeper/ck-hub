//! Commands the Hub sends to a stick. The data types and
//! the signature are in `ck-protocol` and `ck-crypto`. This is the Hub's side: who may send, the
//! queue, delivery in the heartbeat reply and the ack that closes a command. See
//! `docs/superpowers/specs/2026-11-18-hub-commands-design.md`.

use ck_protocol::command::MAX_LIFETIME_S;
use ck_protocol::{
    AckResult, Action, Command, CommandAck, Event, Refusal, SignedCommand, StickId, Timestamp,
};
use serde::Serialize;
use time::{Duration, OffsetDateTime};

use crate::accounts::Account;
use crate::hub::Hub;
use crate::store::StoreError;

/// What a sender gets when they ask for no lifetime.
pub const DEFAULT_LIFETIME_SECONDS: i64 = 300;

/// A command that lives a shorter time than this is gone before a heartbeat can carry it.
pub const MIN_LIFETIME_SECONDS: i64 = 30;

/// Commands of one stick that are neither finished nor expired.
pub const MAX_OPEN_PER_STICK: usize = 10;

/// Commands one person may send in a minute.
pub const MAX_PER_MINUTE: u32 = 20;

/// How long a finished command stays in the history.
pub const HISTORY_DAYS: i64 = 90;

/// How many commands the history shows.
pub const HISTORY_LIMIT: u32 = 50;

/// Where a command is. `Queued` and `Delivered` become `expired` when `expires_at` passes without
/// an ack; that is worked out when it is read, so nothing has to run at that moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandState {
    Queued,
    Delivered,
    Done,
    Refused,
    Failed,
}

impl CommandState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Delivered => "delivered",
            Self::Done => "done",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "queued" => Self::Queued,
            "delivered" => Self::Delivered,
            "done" => Self::Done,
            "refused" => Self::Refused,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    fn is_open(self) -> bool {
        matches!(self, Self::Queued | Self::Delivered)
    }
}

/// A command as kept: the signed form that is sent, who sent it and how it went.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCommand {
    pub signed: SignedCommand,
    pub sent_by_user_id: String,
    /// As it was when the command was sent.
    pub sent_by_email: String,
    pub state: CommandState,
    pub refusal: Option<Refusal>,
    pub created_at: OffsetDateTime,
    pub delivered_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
}

impl StoredCommand {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.signed.command.id
    }

    #[must_use]
    pub fn expires_at(&self) -> OffsetDateTime {
        self.signed.command.expires_at.as_offset_date_time()
    }

    /// The state to show at `now`: an open command past its end is expired.
    #[must_use]
    pub fn shown_state(&self, now: OffsetDateTime) -> &'static str {
        if self.state.is_open() && self.expires_at() <= now {
            "expired"
        } else {
            self.state.as_str()
        }
    }
}

/// What the API shows of a command.
#[derive(Debug, Serialize, PartialEq)]
pub struct CommandView {
    pub id: String,
    pub stick_id: StickId,
    pub action: Action,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
    pub sent_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub delivered_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

fn view(stored: &StoredCommand, now: OffsetDateTime) -> CommandView {
    CommandView {
        id: stored.id().to_owned(),
        stick_id: stored.signed.command.stick_id.clone(),
        action: stored.signed.command.action.clone(),
        state: stored.shown_state(now),
        refusal: stored.refusal,
        sent_by: stored.sent_by_email.clone(),
        created_at: stored.created_at,
        delivered_at: stored.delivered_at,
        finished_at: stored.finished_at,
        expires_at: stored.expires_at(),
    }
}

/// The name of an action as it is kept next to the command.
#[must_use]
pub fn action_type(action: &Action) -> &'static str {
    match action {
        Action::Message { .. } => "message",
        Action::LockScreen => "lock_screen",
        Action::UnlockScreen => "unlock_screen",
        Action::Shutdown => "shutdown",
        _ => "other",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("no such stick")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("this stick already has {MAX_OPEN_PER_STICK} commands waiting")]
    TooManyOpen,
    #[error("too many commands in a minute")]
    RateLimited,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Hub {
    /// Signs a command for a stick of `organization` and queues it. The stick takes it with its next
    /// heartbeat.
    ///
    /// # Errors
    /// [`CommandError::NotFound`] when the stick is not one of the organization's,
    /// [`CommandError::BadRequest`] for an action or lifetime that is not allowed,
    /// [`CommandError::TooManyOpen`] and [`CommandError::RateLimited`] past the limits.
    pub fn send_command(
        &self,
        organization: &str,
        sender: &Account,
        stick: &str,
        action: Action,
        lifetime_seconds: Option<i64>,
    ) -> Result<CommandView, CommandError> {
        let id = StickId::parse(stick).map_err(|_| CommandError::NotFound)?;
        let Some((owner, token, scope)) = self.stick_secret(&id)? else {
            return Err(CommandError::NotFound);
        };
        let now = (self.clock)();
        if owner != organization
            || scope.is_some_and(|scope| !scope.accepts_at(now, Duration::ZERO))
        {
            return Err(CommandError::NotFound);
        }
        if matches!(action, Action::UnlockScreen)
            && !self
                .store
                .last_heartbeat(organization, &id)?
                .is_some_and(|seen| seen.heartbeat.screen_unlock)
        {
            return Err(CommandError::BadRequest(
                "This stick has not reported support for screen unlock.".to_owned(),
            ));
        }
        let lifetime = lifetime_seconds.unwrap_or(DEFAULT_LIFETIME_SECONDS);
        if !(MIN_LIFETIME_SECONDS..=MAX_LIFETIME_S).contains(&lifetime) {
            return Err(CommandError::BadRequest(format!(
                "the lifetime must be {MIN_LIFETIME_SECONDS} to {MAX_LIFETIME_S} seconds"
            )));
        }
        let issued_at = now.replace_nanosecond(0).unwrap_or(now);
        let command = Command {
            id: new_command_id(),
            stick_id: id.clone(),
            issued_at: Timestamp::from(issued_at),
            expires_at: Timestamp::from(issued_at + Duration::seconds(lifetime)),
            action,
        };
        // The same checks the stick makes, so a command it would refuse is never sent.
        command
            .check(&id, Timestamp::from(now))
            .map_err(|refusal| CommandError::BadRequest(refusal.to_string()))?;
        self.command_rate_limit(&sender.id, now)?;
        let open = self.store.pending_commands(organization, &id, now)?;
        if open.len() >= MAX_OPEN_PER_STICK {
            return Err(CommandError::TooManyOpen);
        }
        let signed = ck_crypto::sign_command(&token, command);
        let stored = StoredCommand {
            signed,
            sent_by_user_id: sender.id.clone(),
            sent_by_email: sender.email.clone(),
            state: CommandState::Queued,
            refusal: None,
            created_at: now,
            delivered_at: None,
            finished_at: None,
        };
        self.store.queue_command(organization, stored.clone())?;
        // Old history goes when a new command comes, so no job has to run for it.
        self.store
            .delete_commands_before(organization, now - Duration::days(HISTORY_DAYS))?;
        Ok(view(&stored, now))
    }

    /// The newest commands of a stick of the organization, or `None` for an id that is not one.
    pub fn commands_of_stick(
        &self,
        organization: &str,
        stick: &str,
    ) -> Result<Option<Vec<CommandView>>, StoreError> {
        let Ok(id) = StickId::parse(stick) else {
            return Ok(None);
        };
        let now = (self.clock)();
        let stored = self.store.commands_of(organization, &id, HISTORY_LIMIT)?;
        Ok(Some(stored.iter().map(|c| view(c, now)).collect()))
    }

    /// One command of the organization.
    pub fn command_of(
        &self,
        organization: &str,
        command: &str,
    ) -> Result<Option<CommandView>, StoreError> {
        let now = (self.clock)();
        Ok(self
            .store
            .command(organization, command)?
            .map(|c| view(&c, now)))
    }

    /// What the reply to a stick's heartbeat carries: its open commands, signed. The ones not sent
    /// before are marked as delivered.
    pub(crate) fn commands_for_reply(
        &self,
        organization: &str,
        stick: &StickId,
        screen_unlock: bool,
    ) -> Result<Vec<SignedCommand>, StoreError> {
        let now = (self.clock)();
        let mut open = self.store.pending_commands(organization, stick, now)?;
        // An older agent would discard the whole reply if it contained an unknown action.
        if !screen_unlock {
            open.retain(|command| !matches!(command.signed.command.action, Action::UnlockScreen));
        }
        let fresh: Vec<String> = open
            .iter()
            .filter(|c| c.state == CommandState::Queued)
            .map(|c| c.id().to_owned())
            .collect();
        if !fresh.is_empty() {
            self.store
                .mark_commands_delivered(organization, &fresh, now)?;
        }
        Ok(open.into_iter().map(|c| c.signed).collect())
    }

    /// Closes the command an ack event names. An ack the Hub cannot place is dropped, never an
    /// error: the event itself is already kept and the stick must not send it again.
    pub(crate) fn apply_ack(&self, organization: &str, stick: &StickId, event: &Event) {
        let Ok(ack) = serde_json::from_value::<CommandAck>(event.payload.clone()) else {
            return;
        };
        let now = (self.clock)();
        // A failure here is not the stick's problem either; the command then simply expires.
        let _ = self
            .store
            .record_command_ack(organization, stick, &ack, now);
    }

    fn command_rate_limit(&self, user: &str, now: OffsetDateTime) -> Result<(), CommandError> {
        let mut rates = self.command_rates.lock().expect("command rates lock");
        let (since, count) = rates.entry(user.to_owned()).or_insert((now, 0));
        if (now - *since).whole_seconds() >= 60 {
            (*since, *count) = (now, 0);
        }
        *count += 1;
        if *count > MAX_PER_MINUTE {
            return Err(CommandError::RateLimited);
        }
        Ok(())
    }
}

/// 32 hex digits from a fresh random secret: only characters a command id may hold.
fn new_command_id() -> String {
    let secret = ck_crypto::generate_stick_token();
    ck_crypto::sha256_hex(secret.expose().as_bytes())[..32].to_owned()
}

/// How an ack settles a command: the state it moves to, and the reason when it was refused.
#[must_use]
pub fn settled(ack: &CommandAck) -> (CommandState, Option<Refusal>) {
    match ack.result {
        AckResult::Ok => (CommandState::Done, None),
        AckResult::Refused => (CommandState::Refused, ack.refusal),
        AckResult::Failed => (CommandState::Failed, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_id_is_one_a_stick_accepts() {
        let id = new_command_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(id, new_command_id());
    }

    #[test]
    fn states_round_trip_through_their_names() {
        for state in [
            CommandState::Queued,
            CommandState::Delivered,
            CommandState::Done,
            CommandState::Refused,
            CommandState::Failed,
        ] {
            assert_eq!(CommandState::parse(state.as_str()), Some(state));
        }
        assert_eq!(CommandState::parse("expired"), None);
    }

    #[test]
    fn an_ack_settles_a_command() {
        let refused = CommandAck {
            command_id: "x".to_owned(),
            result: AckResult::Refused,
            refusal: Some(Refusal::Expired),
        };
        assert_eq!(
            settled(&refused),
            (CommandState::Refused, Some(Refusal::Expired))
        );
        let ok = CommandAck {
            command_id: "x".to_owned(),
            result: AckResult::Ok,
            refusal: None,
        };
        assert_eq!(settled(&ok), (CommandState::Done, None));
    }
}
