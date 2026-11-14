//! Sending mail. The Hub only needs to hand a message over; a real provider plugs in behind
//! [`Mailer`] later.

use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub body: String,
}

#[derive(Debug)]
pub struct MailError(pub String);

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub trait Mailer: Send + Sync {
    /// # Errors
    /// When the mail could not be handed over.
    fn send(&self, mail: Mail) -> Result<(), MailError>;
}

/// Keeps every mail, for tests.
#[derive(Default)]
pub struct MemoryMailer {
    sent: Mutex<Vec<Mail>>,
}

impl MemoryMailer {
    #[must_use]
    pub fn sent(&self) -> Vec<Mail> {
        self.sent.lock().expect("mail lock").clone()
    }

    /// The newest mail to this address.
    #[must_use]
    pub fn last_to(&self, to: &str) -> Option<Mail> {
        self.sent().into_iter().rev().find(|mail| mail.to == to)
    }
}

impl Mailer for MemoryMailer {
    fn send(&self, mail: Mail) -> Result<(), MailError> {
        self.sent.lock().expect("mail lock").push(mail);
        Ok(())
    }
}

/// Prints every mail, links included, to standard output. For a developer's machine only: the
/// links in it open accounts.
pub struct ConsoleMailer;

impl Mailer for ConsoleMailer {
    fn send(&self, mail: Mail) -> Result<(), MailError> {
        println!(
            "--- mail to {} ---\n{}\n\n{}\n---",
            mail.to, mail.subject, mail.body
        );
        Ok(())
    }
}

/// The token after `marker` in a mail body, for tests (`#verify=`, `#reset=`).
#[must_use]
pub fn token_after(body: &str, marker: &str) -> Option<String> {
    let rest = body.split_once(marker)?.1;
    Some(
        rest.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_mailer_keeps_mail_and_finds_the_newest_for_an_address() {
        let mailer = MemoryMailer::default();
        for subject in ["one", "two"] {
            mailer
                .send(Mail {
                    to: "a@example.org".into(),
                    subject: subject.into(),
                    body: String::new(),
                })
                .expect("send");
        }
        assert_eq!(mailer.sent().len(), 2);
        assert_eq!(
            mailer.last_to("a@example.org").expect("mail").subject,
            "two"
        );
        assert!(mailer.last_to("b@example.org").is_none());
    }

    #[test]
    fn a_token_is_read_up_to_the_end_of_the_link() {
        let body = "Open https://hub.example/#verify=abc-123_x and wait.";
        assert_eq!(token_after(body, "#verify=").as_deref(), Some("abc-123_x"));
        assert_eq!(token_after(body, "#reset="), None);
    }
}
