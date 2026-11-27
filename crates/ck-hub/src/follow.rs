//! Checks sticks' submissions against the judges without anyone naming a contest: whoever logged
//! in on a stick is followed in the contests the stick opened or submitted to. The Hub asks the
//! judge what that person submitted there, and keeps asking for a while, so a submit made from
//! somewhere else shows up too. What the judge says is kept, and the stick list compares it with
//! what the sticks reported.

use std::collections::HashMap;
use std::sync::Mutex;

use ck_protocol::{EventKind, StickId};
use time::{Duration, OffsetDateTime};

use crate::codeforces::CodeforcesError;
use crate::hub::Hub;
use crate::judge::{JudgeKind, SubmitEvent, submit_event};
use crate::store::{StoreError, StoredEvent};
use crate::submissions::{is_handle, pairs_of};

/// A person is followed in a contest for this long after a stick last showed activity for it.
const FOLLOW_FOR: Duration = Duration::hours(6);
/// How soon the same person is asked about again.
const ASK_AGAIN: Duration = Duration::seconds(60);
/// Stops sticks with leaked secrets from making the Hub ask the judges without end.
const MAX_FOLLOWED: usize = 2000;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Followed {
    organization: String,
    judge: JudgeKind,
    contest_id: u64,
    handle: String,
}

struct Times {
    /// As the stick wrote it: a judge may tell capitals apart.
    handle: String,
    active: OffsetDateTime,
    /// `None` right after a new submit: it goes first.
    asked: Option<OffsetDateTime>,
}

#[derive(Default)]
struct State {
    people: HashMap<Followed, Times>,
    /// Sticks that sent something new, with when their events were last read for it.
    sticks: HashMap<(String, StickId), (bool, Option<OffsetDateTime>)>,
}

/// shortcut: kept in memory (the submits are read again at start); store it when more than one Hub
/// runs. One thread asks about one person every two seconds, so past about thirty people each is
/// asked less often than [`ASK_AGAIN`] (500 people: every 17 minutes), and a stick with a leaked
/// secret can fill [`MAX_FOLLOWED`] with invented names. Cap per stick if either bites.
#[derive(Default)]
pub(crate) struct Follows(Mutex<State>);

#[derive(Debug, thiserror::Error)]
pub enum FollowError {
    #[error(transparent)]
    Judge(#[from] CodeforcesError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Hub {
    /// Takes note of an event that may change who has to be followed.
    pub(crate) fn follow_event(&self, organization: &str, stored: &StoredEvent) {
        let kind = stored.event.kind;
        if !matches!(
            kind,
            EventKind::Login | EventKind::Submission | EventKind::Network
        ) {
            return;
        }
        self.follows
            .0
            .lock()
            .expect("follows lock")
            .sticks
            .entry((organization.to_owned(), stored.event.stick_id.clone()))
            .or_default()
            .0 = true;
        // A new submit is asked about at once.
        if let Some(submit) = submit_event(stored)
            && submit.accepted
        {
            let now = (self.clock)();
            self.follow(
                organization,
                submit.judge,
                &submit.handle,
                submit.contest_id,
                now,
                true,
            );
        }
    }

    fn follow(
        &self,
        organization: &str,
        judge: JudgeKind,
        handle: &str,
        contest_id: u64,
        active: OffsetDateTime,
        at_once: bool,
    ) {
        // The handle goes into the judge's address.
        if !is_handle(handle) {
            return;
        }
        let key = Followed {
            organization: organization.to_owned(),
            judge,
            contest_id,
            handle: handle.to_lowercase(),
        };
        let people = &mut self.follows.0.lock().expect("follows lock").people;
        if let Some(times) = people.get_mut(&key) {
            times.active = times.active.max(active);
            if at_once {
                times.asked = None;
            }
        } else if people.len() < MAX_FOLLOWED {
            people.insert(
                key,
                Times {
                    handle: handle.to_owned(),
                    active,
                    asked: None,
                },
            );
        }
    }

    /// Follows again whoever submitted lately, after a restart.
    ///
    /// # Errors
    /// The store failing.
    pub fn follow_recent(&self) -> Result<(), StoreError> {
        let now = (self.clock)();
        for organization in self.store.organizations()? {
            for submit in self.store.submission_events(&organization.id)? {
                if submit.accepted && now - submit.at < FOLLOW_FOR {
                    let SubmitEvent {
                        judge,
                        handle,
                        contest_id,
                        at,
                        ..
                    } = submit;
                    self.follow(&organization.id, judge, &handle, contest_id, at, false);
                }
            }
        }
        Ok(())
    }

    /// Reads one stick that sent something new, at most once in [`ASK_AGAIN`], and follows the
    /// same people and contests its comparison uses.
    fn read_a_changed_stick(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        let next = {
            let sticks = &mut self.follows.0.lock().expect("follows lock").sticks;
            let next = sticks.iter_mut().find(|(_, (changed, read))| {
                *changed && read.is_none_or(|read| now - read >= ASK_AGAIN)
            });
            next.map(|(key, state)| {
                *state = (false, Some(now));
                key.clone()
            })
        };
        let Some((organization, stick)) = next else {
            return Ok(());
        };
        let stored = self.store.events(&organization, &stick)?;
        for judge in JudgeKind::ALL {
            for (handle, contest_id) in pairs_of(judge, &stored) {
                self.follow(&organization, judge, &handle, contest_id, now, false);
            }
        }
        Ok(())
    }

    /// Asks a judge about one followed person who is due, and keeps what it says. `false` when
    /// nobody is due.
    ///
    /// # Errors
    /// The judge or the store failing. That person is due again later like any other.
    pub fn check_next_followed(&self) -> Result<bool, FollowError> {
        let now = (self.clock)();
        self.read_a_changed_stick(now)?;
        let next = {
            let people = &mut self.follows.0.lock().expect("follows lock").people;
            people.retain(|_, times| now - times.active < FOLLOW_FOR);
            let next = people
                .iter()
                .filter(|(_, times)| times.asked.is_none_or(|asked| now - asked >= ASK_AGAIN))
                // `None` sorts first: new ones, then whoever has waited longest.
                .min_by_key(|(_, times)| (times.asked, times.active))
                .map(|(key, _)| key.clone());
            next.and_then(|key| {
                let times = people.get_mut(&key)?;
                times.asked = Some(now);
                let handle = times.handle.clone();
                Some((key, handle))
            })
        };
        let Some((next, handle)) = next else {
            return Ok(false);
        };
        let Some(judge) = self.judges.get(&next.judge) else {
            return Ok(true);
        };
        let found = judge.submissions_of(next.contest_id, &handle)?;
        self.store
            .add_judge_submissions(&next.organization, next.judge.key(), &found, now)?;
        Ok(true)
    }
}
