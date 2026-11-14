# Sending Mail Through Azure Communication Services Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The cloud Hub sends its sign-up and password-reset mail through Azure Communication Services Email, from `noreply@mail.dev.contestkeeper.com` (develop) and `noreply@mail.contestkeeper.com` (production), with no manual DNS or Azure steps after the one-time provider registration.

**Architecture:** `ck-hub` gets an `AcsMailer` behind the existing `Mailer` trait. It posts to the Communication Service's REST API, signed with the service's access key (HMAC-SHA256, the same primitive `download.rs` already uses for storage links), and sends from a background thread so a request never waits for Azure. `ck-infra` makes the Email Communication Service, the sender domain, the Communication Service and the sender address, puts the domain's verification records into Cloudflare from the domain's own values, runs the verification, and hands the endpoint, sender address and key to the Container App. This is piece 2b of `docs/superpowers/specs/2026-11-14-accounts-and-organizations-design.md` (the open item "which mail provider").

**Tech Stack:** Rust 2024, `ureq` 3 (new, blocking HTTPS over rustls), `hmac`, `sha2`, `base64` (already used). Terraform `azurerm` 5.14 (`azurerm_email_communication_service`, `_domain`, `azurerm_communication_service`, `_email_domain_association`, `_domain_sender_username`), Cloudflare provider 5.29 (already in use), `az rest`.

## Decisions made here (change them before starting if you disagree)

- **Access-key REST, not SMTP.** Azure's SMTP needs an Entra app registration and a client secret, which the deploy identity has no rights to create. The REST API takes the service's access key, which Terraform can read.
- **A sender subdomain per environment.** The root domain's SPF record belongs to Cloudflare Email Routing (`contact@`), and a domain has one SPF record. Develop sends from `mail.dev.contestkeeper.com`, production from `mail.contestkeeper.com`. Each environment's Terraform state makes its own domain, so the two never collide.
- **The key reaches the Hub only as the environment variable `CK_ACS_KEY`** (a Container App secret). No flag takes it, so it is never in a process list, and nothing logs it. It does live in the Terraform state, which is private (the database URL already does).
- **Mail is sent from a background thread** inside `AcsMailer`, through a bounded queue. A request answers the same whether or not the address has an account, and Azure being slow or down cannot slow or fail a request. A full queue drops the mail and logs it.
- **`http://` endpoints are accepted only for a loopback address** (the tests' fake server). Everything else must be `https://`.
- **Needs once, from you:** `az provider register --namespace Microsoft.Communication --wait`. The deploy identity cannot register providers (`resource_provider_registrations = "none"`).
- **Cost:** per mail, a fraction of a cent, no fixed fee (check Azure's current price list). Production's domain is made but unused until its Hub has a name.

## File Structure

`ck-hub`:
- Create `crates/ck-hub/src/acs_mail.rs`: `AcsMailer`, signing, the background sender.
- Modify `crates/ck-hub/src/mail.rs` (nothing: the trait stays), `lib.rs` (export), `main.rs` (flags, wiring), `Cargo.toml` files (`ureq`), `README.md`.
- Create `crates/ck-hub/tests/acs_mail.rs`: the signed request against a fake local server.

`ck-infra` (`infra/azure`): `variables.tf` (`mail_domain`), `mail.tf` (new: all mail resources and records), `main.tf` (Container App secret and variables), `outputs.tf`, `environments/develop.tfvars`, `docs/azure.md`.

---

## Part 1: ck-hub

### Task 1: Sign a request (pure functions)

**Files:** Create `crates/ck-hub/src/acs_mail.rs`; modify `crates/ck-hub/src/lib.rs`.

- [ ] **Step 1: Write the failing test** inside the new file. The expected values were made with Python's `hmac` and `hashlib`, not with this code:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // key = 32 bytes "0123456789abcdef" twice; body = {"a":1}
    const KEY_B64: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

    #[test]
    fn a_known_signature_matches_the_one_made_with_another_tool() {
        let key = STANDARD.decode(KEY_B64).expect("base64");
        let body = br#"{"a":1}"#;
        assert_eq!(content_hash(body), "AVq9f1zFei3ZS3WQ8ErYCEJzkF7jPsXOvq5iJ2qX+GI=");
        let to_sign = string_to_sign(
            SEND_PATH,
            "Sun, 15 Nov 2026 10:00:00 GMT",
            "acs-test.communication.azure.com",
            &content_hash(body),
        );
        assert_eq!(sign(&key, &to_sign), "HM7XDmW+GUoZJnyZHYMXrYZlqw4KKJg0O2y5H9JYsag=");
    }
}
```

- [ ] **Step 2: Run** `cargo test -p ck-hub --lib acs_mail` → compile error (functions missing).
- [ ] **Step 3: Implement** at the top of the file:

```rust
//! Sending mail through Azure Communication Services Email. The request is signed with the service's
//! access key; see https://learn.microsoft.com/azure/communication-services/tutorials/hmac-header-tutorial

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// The path and query the signature covers. The API version is part of it.
const SEND_PATH: &str = "/emails:send?api-version=2023-03-31";

fn content_hash(body: &[u8]) -> String {
    STANDARD.encode(Sha256::digest(body))
}

fn string_to_sign(path_and_query: &str, date: &str, host: &str, content_hash: &str) -> String {
    format!("POST\n{path_and_query}\n{date};{host};{content_hash}")
}

fn sign(key: &[u8], to_sign: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(to_sign.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}
```

- [ ] **Step 4: Run** → PASS. **Step 5: Commit** `feat: sign azure communication services requests`.

### Task 2: The mailer and its background sender

**Files:** modify `acs_mail.rs`, `crates/ck-hub/Cargo.toml`, workspace `Cargo.toml`; create `crates/ck-hub/tests/acs_mail.rs`.

- [ ] **Step 1: Add the dependency.** Workspace `Cargo.toml`: `ureq = { version = "3", default-features = false, features = ["rustls"] }`; `crates/ck-hub/Cargo.toml`: `ureq.workspace = true` (copy how `hmac` is declared). Then run `cargo deny check` and `cargo tree -i aws-lc-sys`: ureq's `rustls` feature must not pull a second crypto provider (the Hub uses `ring`) and every new licence must be on the allow list in `deny.toml`. If `aws-lc-sys` appears, switch to ureq's `rustls-no-provider` feature and install ring as the process default, as `postgres_store.rs::tls` does for its own config. Do not continue until `cargo deny check` is clean.

- [ ] **Step 2: Write the failing integration test** `tests/acs_mail.rs`. A fake server on `127.0.0.1` reads one request, returns `202`, and hands the request back through a channel:

```rust
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use ck_hub::{AcsMailer, Mail, Mailer};

const KEY_B64: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

/// Answers one request with 202 and returns what it received: the header lines and the body.
fn fake_azure() -> (String, mpsc::Receiver<(Vec<String>, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("port");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("a request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut headers = Vec::new();
        let mut length = 0_usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("header line");
            let line = line.trim_end().to_owned();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().expect("length");
            }
            headers.push(line);
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).expect("body");
        let mut stream = stream;
        stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .expect("answer");
        tx.send((headers, String::from_utf8(body).expect("utf8"))).expect("send");
    });
    (endpoint, rx)
}

#[test]
fn a_mail_is_posted_signed_with_the_key_and_the_right_fields() {
    let (endpoint, received) = fake_azure();
    let mailer = AcsMailer::new(&endpoint, "noreply@mail.dev.example.org", KEY_B64).expect("mailer");
    mailer
        .send(Mail {
            to: "ana@example.org".into(),
            subject: "Confirm".into(),
            body: "Open the link.".into(),
        })
        .expect("queued");

    let (headers, body) = received.recv_timeout(Duration::from_secs(10)).expect("the request");
    let header = |name: &str| {
        headers
            .iter()
            .find_map(|h| h.to_ascii_lowercase().strip_prefix(&format!("{name}:")).map(|v| v.trim().to_owned()))
    };
    assert!(headers[0].starts_with("POST /emails:send?api-version=2023-03-31 "), "{}", headers[0]);
    let authorization = header("authorization").expect("authorization");
    assert!(
        authorization.starts_with("hmac-sha256 signedheaders=x-ms-date;host;x-ms-content-sha256&signature="),
        "{authorization}"
    );
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["senderAddress"], "noreply@mail.dev.example.org");
    assert_eq!(json["recipients"]["to"][0]["address"], "ana@example.org");
    assert_eq!(json["content"]["subject"], "Confirm");
    assert_eq!(json["content"]["plainText"], "Open the link.");
}

#[test]
fn settings_that_are_not_safe_are_refused() {
    let key = KEY_B64;
    assert!(AcsMailer::new("https://x.communication.azure.com", "a@b.example", key).is_ok());
    assert!(AcsMailer::new("http://x.communication.azure.com", "a@b.example", key).is_err(), "plain http to a real host");
    assert!(AcsMailer::new("https://x.communication.azure.com", "not an address", key).is_err());
    assert!(AcsMailer::new("https://x.communication.azure.com", "a@b.example", "not base64!").is_err());
}
```

(The header-name comparison is lower-cased because the mailer's HTTP client may case header names differently. Add `serde_json` to the dev-dependencies only if it is not there already.)

- [ ] **Step 3: Run** `cargo test -p ck-hub --test acs_mail` → compile error (`AcsMailer` missing).
- [ ] **Step 4: Implement** in `acs_mail.rs`, after the signing functions:

```rust
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use chrono::Utc;

use crate::mail::{Mail, MailError, Mailer};

/// Mail waiting to be sent. A full queue drops the next mail (and says so in the log) instead of
/// holding up a request.
const QUEUE: usize = 256;

pub struct AcsMailer {
    queue: SyncSender<Mail>,
}

struct Settings {
    endpoint: String,
    host: String,
    sender: String,
    key: Vec<u8>,
}

impl AcsMailer {
    /// `endpoint` is the Communication Service's address (`https://<name>.communication.azure.com`),
    /// `sender` an address of a verified sender domain, `key_base64` the service's access key.
    /// Plain `http` is accepted only for a loopback address.
    ///
    /// # Errors
    /// When the endpoint, the sender or the key is not usable.
    pub fn new(endpoint: &str, sender: &str, key_base64: &str) -> Result<Self, MailError> {
        let endpoint = endpoint.trim_end_matches('/').to_owned();
        let (scheme, rest) = endpoint
            .split_once("://")
            .ok_or_else(|| MailError("the mail endpoint needs https://".into()))?;
        let host = rest.to_owned();
        let loopback = host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]");
        if !(scheme == "https" || (scheme == "http" && loopback)) || host.is_empty() || host.contains('/') {
            return Err(MailError("the mail endpoint must be https://host".into()));
        }
        if !sender.contains('@') || sender.contains(char::is_whitespace) {
            return Err(MailError("the sender is not an email address".into()));
        }
        let key = STANDARD
            .decode(key_base64.trim())
            .map_err(|_| MailError("the mail key is not valid base64".into()))?;
        let settings = Settings { endpoint, host, sender: sender.to_owned(), key };

        let (queue, inbox) = sync_channel::<Mail>(QUEUE);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        std::thread::Builder::new()
            .name("mail".into())
            .spawn(move || {
                for mail in inbox {
                    if let Err(error) = post(&agent, &settings, &mail) {
                        tracing::error!("cannot send mail: {error}");
                    }
                }
            })
            .map_err(|e| MailError(e.to_string()))?;
        Ok(Self { queue })
    }
}

impl Mailer for AcsMailer {
    fn send(&self, mail: Mail) -> Result<(), MailError> {
        self.queue
            .try_send(mail)
            .map_err(|_| MailError("the mail queue is full, the mail was dropped".into()))
    }
}

fn post(agent: &ureq::Agent, settings: &Settings, mail: &Mail) -> Result<(), String> {
    let body = serde_json::json!({
        "senderAddress": settings.sender,
        "recipients": { "to": [{ "address": mail.to }] },
        "content": { "subject": mail.subject, "plainText": mail.body },
    })
    .to_string();
    let date = Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let hash = content_hash(body.as_bytes());
    let signature = sign(&settings.key, &string_to_sign(SEND_PATH, &date, &settings.host, &hash));
    agent
        .post(format!("{}{SEND_PATH}", settings.endpoint))
        .header("x-ms-date", &date)
        .header("x-ms-content-sha256", &hash)
        .header(
            "Authorization",
            &format!("HMAC-SHA256 SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature={signature}"),
        )
        .header("Content-Type", "application/json")
        .send(body)
        // The answer names no secret, but it can name the address, so only the status is logged.
        .map(|_| ())
        .map_err(|error| error.to_string())
}
```

If `serde_json` is not a normal dependency of `ck-hub` yet it already is (`routes.rs` uses `json!`); `tracing` too. Export `AcsMailer` from `lib.rs` (`mod acs_mail;` and `pub use acs_mail::AcsMailer;`). The error text of `ureq` can contain the URL, never the key or the body.

- [ ] **Step 5: Run** `cargo test -p ck-hub --test acs_mail` and `--lib acs_mail` → PASS. `cargo clippy --all-targets -- -D warnings` (the CI toolchain is newer than a local one: `assert!(x.is_empty())` is flagged there, use `assert_eq!`).
- [ ] **Step 6: Commit** `feat: send mail through azure communication services`.

### Task 3: Flags and wiring

**Files:** modify `crates/ck-hub/src/main.rs`, `README.md`.

- [ ] **Step 1:** extend `MailKind` with `Acs` and add to `ServeArgs`:

```rust
    /// With `--mail acs`: the address of the Communication Service, such as
    /// https://<name>.communication.azure.com
    #[arg(long, env = "CK_MAIL_ENDPOINT")]
    mail_endpoint: Option<String>,
    /// With `--mail acs`: the sender, an address of a verified sender domain
    #[arg(long, env = "CK_MAIL_FROM")]
    mail_from: Option<String>,
```

The key is **not** a flag. In `run`, next to the console branch:

```rust
    if let (Some(MailKind::Acs), Some(url)) = (&args.mail, &args.public_url) {
        let endpoint = args.mail_endpoint.as_deref().ok_or("--mail acs needs --mail-endpoint")?;
        let sender = args.mail_from.as_deref().ok_or("--mail acs needs --mail-from")?;
        let key = std::env::var("CK_ACS_KEY").map_err(|_| "--mail acs needs the key in CK_ACS_KEY")?;
        let mailer = AcsMailer::new(endpoint, sender, &key).map_err(|e| e.to_string())?;
        hub = hub.with_mail(Arc::new(mailer), url);
    }
```

- [ ] **Step 2: Test** that the program starts and refuses to run `--mail acs` without the key: a small test in `tests/startup.rs` that spawns the binary with `--mail acs --public-url https://x.example --mail-endpoint https://x.communication.azure.com --mail-from a@b.example` and no `CK_ACS_KEY`, and expects a non-zero exit within a few seconds and a stderr that names `CK_ACS_KEY` and does not contain any key value.
- [ ] **Step 3: README:** document `--mail acs`, `--mail-endpoint`, `--mail-from` and `CK_ACS_KEY` (environment only), and that mail is sent from a background queue.
- [ ] **Step 4:** `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (memory) and with `CK_TEST_DATABASE_URL` set.
- [ ] **Step 5: Commit** `feat: add the acs mail option to the hub`, push `feature/acs-mail`, open a PR into `develop` titled `feat: send mail through azure communication services`.

---

## Part 2: ck-infra

Start only after Part 1 is merged and its image is built, and after you ran `az provider register --namespace Microsoft.Communication --wait`.

### Task 4: The Azure resources

**Files:** create `infra/azure/mail.tf`; modify `variables.tf`.

- [ ] **Step 1: Variable**

```hcl
variable "mail_domain" {
  description = "The domain the Hub sends mail from, for example mail.dev.contestkeeper.com. Needs custom_domain's Cloudflare zone (cloudflare_zone_id). Leave empty for no mail."
  type        = string
  default     = ""

  validation {
    condition     = var.mail_domain == "" || can(regex("^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\\.)+[a-z]{2,}$", var.mail_domain))
    error_message = "Use a lowercase domain such as mail.dev.contestkeeper.com."
  }
}
```

- [ ] **Step 2: `mail.tf`.** Everything has `count = local.mail ? 1 : 0` where `locals { mail = var.mail_domain != "" && var.cloudflare_zone_id != "" }`:

```hcl
resource "azurerm_email_communication_service" "mail" {
  count               = local.mail ? 1 : 0
  name                = "ecs-${local.base}"
  resource_group_name = data.azurerm_resource_group.this.name
  data_location       = "Europe"
  tags                = var.tags
}

resource "azurerm_email_communication_service_domain" "mail" {
  count             = local.mail ? 1 : 0
  name              = var.mail_domain
  email_service_id  = azurerm_email_communication_service.mail[0].id
  domain_management = "CustomerManaged"
}

resource "azurerm_email_communication_service_domain_sender_username" "noreply" {
  count                  = local.mail ? 1 : 0
  name                   = "noreply"
  email_service_domain_id = azurerm_email_communication_service_domain.mail[0].id
}

resource "azurerm_communication_service" "mail" {
  count               = local.mail ? 1 : 0
  name                = "cs-${local.base}"
  resource_group_name = data.azurerm_resource_group.this.name
  data_location       = "Europe"
  tags                = var.tags
}
```

- [ ] **Step 3: DNS records from the domain's own values.** The domain resource's `verification_records` has one block each for `domain` (TXT), `spf` (TXT), `dkim` and `dkim2` (CNAME), each with `name`, `type`, `value`, `ttl`. Read their exact shape with `terraform providers schema -json` before writing (the attribute is a list of objects), then:

```hcl
locals {
  mail_records = local.mail ? {
    domain = azurerm_email_communication_service_domain.mail[0].verification_records[0].domain[0]
    spf    = azurerm_email_communication_service_domain.mail[0].verification_records[0].spf[0]
    dkim   = azurerm_email_communication_service_domain.mail[0].verification_records[0].dkim[0]
    dkim2  = azurerm_email_communication_service_domain.mail[0].verification_records[0].dkim2[0]
  } : {}
}

resource "cloudflare_dns_record" "mail" {
  for_each = local.mail_records
  zone_id  = var.cloudflare_zone_id
  # Azure gives "@" for the domain itself; the zone wants the full name.
  name    = each.value.name == "@" ? var.mail_domain : "${each.value.name}.${var.mail_domain}"
  type    = each.value.type
  content = each.value.value
  ttl     = 1
  proxied = false
  comment = "Keeper Hub mail (${var.name}), made by Terraform"
}
```

Check on a plan that the generated names are what Azure's portal shows for the same domain (the DKIM names are `selector1-azurecomm-prod-net._domainkey` and `selector2-...` under the domain). A SPF record on `mail.dev.contestkeeper.com` does not clash with the apex SPF of Email Routing.

- [ ] **Step 4: Verify the domain, then link it.** Terraform cannot start verification. After the records exist and a minute has passed, a `terraform_data` starts the four verifications with `az rest` (no CLI extension needed) and waits until they are all `Verified`, at most ten minutes:

```hcl
resource "terraform_data" "mail_verification" {
  count            = local.mail ? 1 : 0
  triggers_replace = [azurerm_email_communication_service_domain.mail[0].id]

  provisioner "local-exec" {
    command = <<-EOT
      set -eu
      base="https://management.azure.com${azurerm_email_communication_service_domain.mail[0].id}"
      api="api-version=2023-04-01"
      for kind in Domain SPF DKIM DKIM2; do
        az rest --method post --url "$base/initiateVerification?$api" --body "{\"verificationType\":\"$kind\"}" --output none
      done
      for attempt in $(seq 1 40); do
        states=$(az rest --method get --url "$base?$api" --query "[properties.verificationStates.Domain.status,properties.verificationStates.SPF.status,properties.verificationStates.DKIM.status,properties.verificationStates.DKIM2.status]" -o tsv | tr '\n' ' ')
        echo "attempt $attempt: $states"
        case "$states" in "Verified Verified Verified Verified ") exit 0 ;; esac
        sleep 15
      done
      echo "the mail domain was not verified in time" >&2; exit 1
    EOT
  }

  depends_on = [cloudflare_dns_record.mail, time_sleep.mail_dns]
}

resource "time_sleep" "mail_dns" {
  count           = local.mail ? 1 : 0
  create_duration = "60s"
  depends_on      = [cloudflare_dns_record.mail]
}

resource "azurerm_communication_service_email_domain_association" "mail" {
  count                    = local.mail ? 1 : 0
  communication_service_id = azurerm_communication_service.mail[0].id
  email_service_domain_id  = azurerm_email_communication_service_domain.mail[0].id
  depends_on               = [terraform_data.mail_verification]
}
```

The API version and the `verificationStates` field names are from memory: before writing this, run the same `az rest` GET by hand against a domain made in a scratch resource group (or read the response of the first deploy) and fix the query to match. The deploy identity needs nothing more than Contributor on its group, which it has.

- [ ] **Step 5: Hand the Hub its settings** in `main.tf`, inside the Container App (copy how `download-key` is done):

```hcl
  dynamic "secret" {
    for_each = local.mail ? [1] : []
    content {
      name  = "mail-key"
      value = azurerm_communication_service.mail[0].primary_key
    }
  }
```

and in the container, `dynamic "env"` blocks (only when `local.mail`) for `CK_MAIL=acs`, `CK_MAIL_ENDPOINT=https://${azurerm_communication_service.mail[0].hostname}`, `CK_MAIL_FROM=noreply@${azurerm_email_communication_service_domain.mail[0].mail_from_sender_domain}` (check whether `mail_from_sender_domain` or `from_sender_domain` is the address domain on a real plan), `CK_PUBLIC_URL=https://${var.custom_domain}`, and `CK_ACS_KEY` from the secret `mail-key`. The Container App must depend on the association so the sender is usable when it starts.

- [ ] **Step 6:** `develop.tfvars`: `mail_domain = "mail.dev.contestkeeper.com"`. Production stays empty until it has its name. `docs/azure.md`: a section "Mail" with the one-time provider registration, the records Terraform makes, the secret handling, and the cost note.
- [ ] **Step 7:** `terraform fmt -check -recursive`, a relaxed `validate` as for the earlier changes (the local Terraform is too old for the write-only database password), commit `feat: send the hub's mail through azure communication services`, push, open a PR. Merge only after the ck-hub image with `--mail acs` is the one the develop deploy runs.

### Task 5: Check it for real

- [ ] After the develop deploy: read `az rest` for the four verification states (all `Verified`); `dig @1.1.1.1` for the four records; then register with your own address:

```bash
curl -sS -X POST https://hub.dev.contestkeeper.com/api/v1/register -H 'content-type: application/json' -d '{"email":"YOUR-ADDRESS","password":"a-long-test-password-1"}'
```

  Expect `202`, then the mail in your inbox (check spam once) from `noreply@mail.dev.contestkeeper.com` with a `https://hub.dev.contestkeeper.com/#verify=...` link. Open it, log in on the page, and use "Forgot password" once. Look at the message headers for `spf=pass` and `dkim=pass`.
- [ ] If nothing arrives: the Container App's console log shows `cannot send mail: ...` with the HTTP status (never the key). `401` is a wrong key or clock; `403`/`400` with an unverified domain means verification is not finished.

## Self-review against the spec

- A real mail provider behind the `Mailer` seam (the spec's open item), with the console mailer kept for local runs: Tasks 1 to 3.
- No secret in a flag, a log or a commit: the key is an environment variable fed from a Container App secret; the log carries statuses only.
- Same answer whether or not an address exists: unchanged, and now also independent of Azure's speed, because sending is off the request.
- Not here: production's mail (its domain waits for its name), bounce and complaint handling, a mail template pass, and Google/Microsoft sign-in (plan 3).
