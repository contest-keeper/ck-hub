//! Talking to the Codeforces API.

use std::fmt::Write;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256, Sha512};
use time::OffsetDateTime;

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
            OffsetDateTime::from_unix_timestamp(row.creation_time_seconds).map_err(|_| {
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

#[derive(Deserialize)]
struct Standings {
    status: String,
    comment: Option<String>,
    result: Option<StandingsResult>,
}

#[derive(Deserialize)]
struct StandingsResult {
    contest: ContestInfo,
}

#[derive(Deserialize)]
struct ContestInfo {
    phase: String,
}

/// Reads the phase of a contest (`FINISHED`, `CODING`, ...) from the answer of `contest.standings`.
///
/// # Errors
/// [`CodeforcesError::Refused`] when Codeforces says `FAILED`, [`CodeforcesError::Malformed`]
/// when the body is not the expected JSON.
pub fn parse_phase(body: &str) -> Result<String, CodeforcesError> {
    let answer: Standings = serde_json::from_str(body)
        .map_err(|error| CodeforcesError::Malformed(error.to_string()))?;
    if answer.status != "OK" {
        return Err(CodeforcesError::Refused(
            answer.comment.unwrap_or(answer.status),
        ));
    }
    answer
        .result
        .map(|result| result.contest.phase)
        .ok_or_else(|| CodeforcesError::Malformed("no contest in the answer".to_owned()))
}

/// The source as Codeforces shows it: one kind of line end, no trailing spaces, no trailing blank
/// lines. The stick applies the same steps to what it sent (`ck-capture`), so the hashes compare.
// shortcut: copied from ck-os, move into ck-crypto at the next ck-protocol release.
#[must_use]
pub fn normalize_source(source: &str) -> String {
    let text = source.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect();
    lines.join("\n").trim_end_matches('\n').to_owned()
}

/// Undoes the HTML escapes of a page's text in one pass, so `&amp;lt;` becomes `&lt;`.
fn unescape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let known = rest.find(';').filter(|end| *end <= 10).and_then(|end| {
            let name = &rest[1..end];
            let decoded = match name {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => name
                    .strip_prefix('#')
                    .and_then(|number| match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => number.parse().ok(),
                    })
                    .and_then(char::from_u32),
            };
            decoded.map(|c| (c, end + 1))
        });
        if let Some((decoded, length)) = known {
            out.push(decoded);
            rest = &rest[length..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The hash of the source on a submission page, or `None` for a page that shows no source (the
/// code is not public yet, or the page is a check or an error).
#[must_use]
pub fn source_hash_of_page(page: &str) -> Option<String> {
    let at = page.find("id=\"program-source-text\"")?;
    let after = &page[at..];
    let start = after.find('>')? + 1;
    let end = after.find("</pre>")?;
    if end < start {
        return None;
    }
    let source = normalize_source(&unescape_html(&after[start..end]));
    Some(hex(&Sha256::digest(source.as_bytes())))
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
        parse_status(&self.call("contest.status", params)?)
    }

    /// The phase of a contest, for example `FINISHED`.
    ///
    /// # Errors
    /// The same as [`Client::status`].
    pub fn phase(&self, contest_id: u64) -> Result<String, CodeforcesError> {
        let body = self.call(
            "contest.standings",
            &[
                ("contestId", contest_id.to_string()),
                ("from", "1".to_owned()),
                ("count", "1".to_owned()),
            ],
        )?;
        parse_phase(&body)
    }

    /// The hash of the source of one submission, read from its public page. `None` when the page
    /// shows no source.
    ///
    /// # Errors
    /// [`CodeforcesError::Request`] when the page cannot be read. The error never holds the address.
    pub fn source_hash(
        &self,
        contest_id: u64,
        submission_id: u64,
    ) -> Result<Option<String>, CodeforcesError> {
        let url = format!(
            "{}/contest/{contest_id}/submission/{submission_id}",
            self.base
        );
        let mut response = self
            .agent
            .get(&url)
            .call()
            .map_err(|_| CodeforcesError::Request("cannot reach Codeforces".to_owned()))?;
        if !response.status().is_success() {
            return Err(CodeforcesError::Request(format!(
                "Codeforces answered {} for a submission page",
                response.status().as_u16()
            )));
        }
        let page = response
            .body_mut()
            .read_to_string()
            .map_err(|_| CodeforcesError::Request("cannot read the page".to_owned()))?;
        Ok(source_hash_of_page(&page))
    }

    /// One API call, signed when there are credentials. Returns the body whatever the status is.
    fn call(&self, method: &str, params: &[(&str, String)]) -> Result<String, CodeforcesError> {
        let query = match &self.credentials {
            Some((key, secret)) => {
                let now = OffsetDateTime::now_utc();
                let rand = format!("{:06}", now.nanosecond() % 1_000_000);
                signed_query(method, params, key, secret, now.unix_timestamp(), &rand)
            }
            None => params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("&"),
        };
        let url = format!("{}/api/{method}?{query}", self.base);
        let mut response = self
            .agent
            .get(&url)
            .call()
            .map_err(|_| CodeforcesError::Request("cannot reach Codeforces".to_owned()))?;
        response
            .body_mut()
            .read_to_string()
            .map_err(|_| CodeforcesError::Request("cannot read the answer".to_owned()))
    }
}

/// What the Hub asks the judge after a contest to check the code. The Codeforces client is the real
/// one; tests use a fake.
pub trait SourceJudge {
    /// The phase of the contest, for example `FINISHED`.
    ///
    /// # Errors
    /// The judge cannot be asked or does not answer as expected.
    fn phase(&self, contest_id: u64) -> Result<String, CodeforcesError>;

    /// The hash of the source of one submission, `None` while the code is not public.
    ///
    /// # Errors
    /// The page cannot be read.
    fn source_hash(
        &self,
        contest_id: u64,
        submission_id: u64,
    ) -> Result<Option<String>, CodeforcesError>;
}

impl SourceJudge for Client {
    fn phase(&self, contest_id: u64) -> Result<String, CodeforcesError> {
        Client::phase(self, contest_id)
    }

    fn source_hash(
        &self,
        contest_id: u64,
        submission_id: u64,
    ) -> Result<Option<String>, CodeforcesError> {
        Client::source_hash(self, contest_id, submission_id)
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
        assert_eq!(submissions[1].submitted_at.unix_timestamp(), 1_800_000_000);
    }

    #[test]
    fn a_failed_status_is_a_refusal_with_the_comment() {
        let error = parse_status(r#"{"status":"FAILED","comment":"contestId: not found"}"#)
            .expect_err("refused");
        assert!(error.to_string().contains("contestId: not found"));
    }

    #[test]
    fn reads_the_phase_of_a_contest() {
        let body = r#"{"status":"OK","result":{"contest":{"id":1,"phase":"FINISHED"},"rows":[]}}"#;
        assert_eq!(parse_phase(body).expect("phase"), "FINISHED");
        assert!(parse_phase(r#"{"status":"FAILED","comment":"no"}"#).is_err());
    }

    #[test]
    fn the_source_of_a_page_is_hashed_like_the_stick_hashes_it() {
        // `ck-capture` hashes the source it sent: LF line ends, no trailing blanks.
        let page = "<div><pre id=\"program-source-text\" class=\"prettyprint\">#include &lt;a&gt;\r\nint x = 1 &amp;&amp; 2;  \r\n</pre></div>";
        let expected = hex(&Sha256::digest(b"#include <a>\nint x = 1 && 2;"));
        assert_eq!(source_hash_of_page(page), Some(expected));
        assert_eq!(source_hash_of_page("<html>log in</html>"), None);
    }

    #[test]
    fn escapes_are_undone_once() {
        assert_eq!(
            unescape_html("&amp;lt; &#65;&#x42; &bogus; &"),
            "&lt; AB &bogus; &"
        );
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
