use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Duration, OffsetDateTime, UtcOffset};

/// The contest and UTC receipt window assigned to one immutable stick credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialScope {
    pub contest_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub valid_from: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub valid_until: OffsetDateTime,
}

impl CredentialScope {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.contest_id.is_empty()
            || self.contest_id.len() > 200
            || self.contest_id.trim() != self.contest_id
            || self.contest_id.chars().any(char::is_control)
        {
            return Err("the credential scope needs a contest ID of 1 to 200 bytes of plain text");
        }
        if self.valid_until <= self.valid_from
            || self.valid_from.offset() != UtcOffset::UTC
            || self.valid_until.offset() != UtcOffset::UTC
        {
            return Err("the credential scope needs an increasing validity window in UTC");
        }
        Ok(())
    }

    pub(crate) fn accepts_at(&self, now: OffsetDateTime, grace: Duration) -> bool {
        now >= self.valid_from
            && now
                .checked_sub(grace)
                .is_some_and(|receipt| receipt < self.valid_until)
    }

    pub(crate) fn agrees_with(&self, assertion: Option<&Value>) -> bool {
        assertion.is_none_or(|value| match value {
            Value::Null => true,
            Value::String(id) => id == &self.contest_id,
            Value::Number(number) => number
                .as_u64()
                .is_some_and(|id| self.contest_id.parse() == Ok(id)),
            _ => false,
        })
    }
}
