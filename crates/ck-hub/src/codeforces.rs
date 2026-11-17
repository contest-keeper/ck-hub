//! Talking to the Codeforces API.

use std::fmt::Write;
use std::time::Duration;

use chrono::DateTime;
use serde::Deserialize;
use sha2::{Digest, Sha512};

use crate::judge::JudgeSubmission;

#[derive(Debug, thiserror::Error)]
pub enum CodeforcesError {
    #[error("the answer is not what Codeforces sends: {0}")]
    Malformed(String),
    #[error("Codeforces refused the request: {0}")]
    Refused(String),
    #[error("the request failed: {0}")]
    Request(String),
}

#[derive(Deserialize)]
struct Answer {
    status: String,
    comment: Option<String>,
    #[serde(default)]
    result: Vec<Row>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    id: u64,
    contest_id: u64,
    creation_time_seconds: i64,
    problem: Problem,
    author: Author,
}

#[derive(Deserialize)]
struct Problem {
    index: String,
}

#[derive(Deserialize)]
struct Author {
    members: Vec<Member>,
}

#[derive(Deserialize)]
struct Member {
    handle: String,
}

/// Reads the answer of `contest.status`. Codeforces lists the newest submission first.
///
/// # Errors
/// [`CodeforcesError::Refused`] when Codeforces says `FAILED`, [`CodeforcesError::Malformed`]
/// when the body is not the expected JSON.
pub fn parse_status(body: &str) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
    let answer: Answer = serde_json::from_str(body)
        .map_err(|error| CodeforcesError::Malformed(error.to_string()))?;
    if answer.status != "OK" {
        return Err(CodeforcesError::Refused(
            answer.comment.unwrap_or(answer.status),
        ));
    }
    let mut submissions = Vec::with_capacity(answer.result.len());
    for row in answer.result {
        let submitted_at =
            DateTime::from_timestamp(row.creation_time_seconds, 0).ok_or_else(|| {
                CodeforcesError::Malformed("a submission time is out of range".to_owned())
            })?;
        submissions.push(JudgeSubmission {
            submission_id: row.id,
            contest_id: row.contest_id,
            handles: row.author.members.into_iter().map(|m| m.handle).collect(),
            problem_index: row.problem.index,
            submitted_at,
        });
    }
    Ok(submissions)
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("writing to a string");
    }
    text
}

/// The query string for a signed call: the parameters, `apiKey`, `time` and `apiSig`, with the
/// parameters in the order the signature covers. `rand` is six characters the caller picks.
#[must_use]
pub fn signed_query(
    method: &str,
    params: &[(&str, String)],
    key: &str,
    secret: &str,
    time: i64,
    rand: &str,
) -> String {
    let mut all: Vec<(String, String)> = params
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect();
    all.push(("apiKey".to_owned(), key.to_owned()));
    all.push(("time".to_owned(), time.to_string()));
    all.sort();
    let joined = all
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let digest = Sha512::digest(format!("{rand}/{method}?{joined}#{secret}").as_bytes());
    format!("{joined}&apiSig={rand}{}", hex(&digest))
}

/// A Codeforces API client. With credentials every call is signed, which also covers gyms and
/// group contests; without them only public contests can be read.
pub struct Client {
    agent: ureq::Agent,
    base: String,
    credentials: Option<(String, String)>,
}

impl Client {
    #[must_use]
    pub fn new(base: &str, credentials: Option<(String, String)>) -> Self {
        // Codeforces explains a refusal in a JSON body with an error status, so the body is read
        // whatever the status is.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            agent,
            base: base.trim_end_matches('/').to_owned(),
            credentials,
        }
    }

    /// One page of `contest.status`, newest first. `from` starts at 1.
    ///
    /// # Errors
    /// [`CodeforcesError::Request`] when the call fails, otherwise what [`parse_status`] says. The
    /// error never holds the address, which carries the API key.
    pub fn status(
        &self,
        contest_id: u64,
        from: u64,
        count: u64,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        self.contest_status(&[
            ("contestId", contest_id.to_string()),
            ("from", from.to_string()),
            ("count", count.to_string()),
        ])
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
        self.contest_status(&[
            ("contestId", contest_id.to_string()),
            ("handle", handle.to_owned()),
            ("from", "1".to_owned()),
            ("count", count.to_string()),
        ])
    }

    fn contest_status(
        &self,
        params: &[(&str, String)],
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        let query = match &self.credentials {
            Some((key, secret)) => {
                let now = chrono::Utc::now();
                let rand = format!("{:06}", now.timestamp_subsec_nanos() % 1_000_000);
                signed_query(
                    "contest.status",
                    params,
                    key,
                    secret,
                    now.timestamp(),
                    &rand,
                )
            }
            None => params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("&"),
        };
        let url = format!("{}/api/contest.status?{query}", self.base);
        let mut response = self
            .agent
            .get(&url)
            .call()
            .map_err(|_| CodeforcesError::Request("cannot reach Codeforces".to_owned()))?;
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|_| CodeforcesError::Request("cannot read the answer".to_owned()))?;
        parse_status(&body)
    }
}

/// Where the submissions of a person in a contest come from. The Codeforces client is the real one;
/// tests use a fake.
pub trait Judge: Send + Sync {
    /// The newest submissions of `handle` in `contest_id`, newest first.
    ///
    /// # Errors
    /// The judge cannot be asked or does not answer as expected.
    fn submissions_of(
        &self,
        contest_id: u64,
        handle: &str,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError>;
}

impl Judge for Client {
    fn submissions_of(
        &self,
        contest_id: u64,
        handle: &str,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        self.status_of(contest_id, handle, 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{"status":"OK","result":[
        {"id":20,"contestId":1900,"creationTimeSeconds":1800000100,
         "problem":{"contestId":1900,"index":"B","name":"x"},
         "author":{"members":[{"handle":"tourist"},{"handle":"petr"}]},"verdict":"OK"},
        {"id":10,"contestId":1900,"creationTimeSeconds":1800000000,
         "problem":{"contestId":1900,"index":"A","name":"y"},
         "author":{"members":[{"handle":"petr"}]}}]}"#;

    #[test]
    fn reads_submissions_newest_first() {
        let submissions = parse_status(BODY).expect("parsed");
        assert_eq!(submissions.len(), 2);
        assert_eq!(submissions[0].submission_id, 20);
        assert_eq!(submissions[0].handles, vec!["tourist", "petr"]);
        assert_eq!(submissions[0].problem_index, "B");
        assert_eq!(submissions[1].submitted_at.timestamp(), 1_800_000_000);
    }

    #[test]
    fn a_failed_status_is_a_refusal_with_the_comment() {
        let error = parse_status(r#"{"status":"FAILED","comment":"contestId: not found"}"#)
            .expect_err("refused");
        assert!(error.to_string().contains("contestId: not found"));
    }

    #[test]
    fn garbage_is_malformed() {
        assert!(matches!(
            parse_status("<html>"),
            Err(CodeforcesError::Malformed(_))
        ));
    }

    #[test]
    fn the_signature_covers_sorted_parameters_and_the_secret() {
        let query = signed_query(
            "contest.status",
            &[("contestId", "1".to_owned()), ("count", "5".to_owned())],
            "k",
            "s",
            100,
            "abcdef",
        );
        let signed = "abcdef/contest.status?apiKey=k&contestId=1&count=5&time=100#s";
        let digest = Sha512::digest(signed.as_bytes());
        let hex = hex(&digest);
        assert_eq!(
            query,
            format!("apiKey=k&contestId=1&count=5&time=100&apiSig=abcdef{hex}")
        );
        assert!(
            !query.contains('s') || !query.contains("#s"),
            "the secret is never sent"
        );
    }
}
