//! Talking to `VJudge`'s public status list (`/status/data`), which answers JSON to a client with no
//! login. The error type is the Codeforces one: it is the Hub's judge error, not Codeforces-only.

use std::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::codeforces::{CodeforcesError, Judge, STATUS_PAGE};
use crate::judge::JudgeSubmission;

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    data: Vec<Row>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    run_id: u64,
    /// Milliseconds since 1970, by `VJudge`'s clock.
    time: i64,
    user_name: String,
    contest_id: Option<u64>,
    /// The problem's letter in the contest.
    contest_num: Option<String>,
}

/// Reads the answer of `/status/data`, newest first. A row that is not in a contest, or has no
/// usable time, is left out.
///
/// # Errors
/// [`CodeforcesError::Malformed`] when the body is not the expected JSON.
pub fn parse_status(body: &str) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
    let answer: Answer = serde_json::from_str(body)
        .map_err(|error| CodeforcesError::Malformed(error.to_string()))?;
    Ok(answer
        .data
        .into_iter()
        .filter_map(|row| {
            Some(JudgeSubmission {
                submission_id: row.run_id,
                contest_id: row.contest_id?,
                handles: vec![row.user_name],
                problem_index: row.contest_num?,
                submitted_at: OffsetDateTime::from_unix_timestamp(row.time / 1000).ok()?,
            })
        })
        .collect())
}

/// Handles are put in a query as they are only when they are plain; anything else is encoded.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A `VJudge` client. It needs no credentials: the status list of a contest is public.
pub struct Client {
    agent: ureq::Agent,
    base: String,
}

impl Client {
    #[must_use]
    pub fn new(base: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .user_agent("keeper-hub")
            .build()
            .into();
        Self {
            agent,
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    /// One page of the contest's submissions, newest first. `from` starts at 1.
    ///
    /// # Errors
    /// [`CodeforcesError::Request`] when the call fails, otherwise what [`parse_status`] says.
    pub fn status(
        &self,
        contest_id: u64,
        from: u64,
        count: u64,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        self.list(contest_id, "", from, count)
    }

    /// The newest `count` submissions of one person in a contest, newest first.
    ///
    /// # Errors
    /// The same as [`Client::status`].
    pub fn status_of(
        &self,
        contest_id: u64,
        handle: &str,
        count: u64,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        self.list(contest_id, handle, 1, count)
    }

    fn list(
        &self,
        contest_id: u64,
        handle: &str,
        from: u64,
        count: u64,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        let url = format!(
            "{}/status/data?draw=1&start={}&length={count}&un={}&OJId=All&probNum=&res=0\
             &language=&onlyFollowee=false&orderBy=run_id&contestId={contest_id}",
            self.base,
            from.saturating_sub(1),
            encode(handle)
        );
        let mut response = self
            .agent
            .get(&url)
            .call()
            .map_err(|_| CodeforcesError::Request("cannot reach VJudge".to_owned()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|_| CodeforcesError::Request("cannot read the answer".to_owned()))?;
        if status != 200 {
            return Err(CodeforcesError::Refused(format!(
                "VJudge answered {status}"
            )));
        }
        parse_status(&body)
    }
}

impl Judge for Client {
    fn submissions_of(
        &self,
        contest_id: u64,
        handle: &str,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        self.status_of(contest_id, handle, STATUS_PAGE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{"data":[
        {"runId":74477117,"time":1795789024000,"userName":"robbertp","status":"Accepted",
         "contestId":871412,"contestNum":"C","oj":null,"probNum":null},
        {"runId":74479366,"time":1795797584000,"userName":"caluete","status":"Accepted",
         "contestId":null,"contestNum":null,"oj":"CSES","probNum":"1094"},
        {"runId":74476990,"time":9223372036854775807,"userName":"x","contestId":871412,"contestNum":"A"}
    ],"recordsTotal":9999999}"#;

    #[test]
    fn reads_contest_rows_with_the_judges_time_and_skips_the_rest() {
        let rows = parse_status(BODY).expect("parsed");
        assert_eq!(
            rows.len(),
            1,
            "a row outside a contest or with no time is left out"
        );
        assert_eq!(rows[0].submission_id, 74_477_117);
        assert_eq!(rows[0].contest_id, 871_412);
        assert_eq!(rows[0].problem_index, "C");
        assert_eq!(rows[0].handles, vec!["robbertp"]);
        assert_eq!(rows[0].submitted_at.unix_timestamp(), 1_795_789_024);
    }

    #[test]
    fn a_body_that_is_not_the_list_is_malformed() {
        assert!(matches!(
            parse_status("<html>Just a moment...</html>"),
            Err(CodeforcesError::Malformed(_))
        ));
        assert!(parse_status(r#"{"data":[{"runId":"x"}]}"#).is_err());
        assert_eq!(parse_status("{}").expect("empty").len(), 0);
    }

    #[test]
    fn a_handle_is_encoded_in_the_query() {
        assert_eq!(encode("robbertp_1.x-y"), "robbertp_1.x-y");
        assert_eq!(encode("a b&c=d"), "a%20b%26c%3Dd");
    }
}
