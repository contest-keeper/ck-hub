//! `impl Store for PostgresStore`: one query or transaction per method.

use std::collections::HashMap;

use super::{
    BootSpan, COMMAND_COLUMNS, CommandAck, Event, Hardware, Heartbeat, Invitation, JudgeSubmission,
    Member, Membership, NewEnrolmentCode, NewProfile, NewUser, OffsetDateTime, Organization,
    PostgresStore, Profile, ProfileSummary, RECORDING_COLUMNS, Recording, Redemption, Role,
    SeenHeartbeat, Stick, StickHardware, StickId, Store, StoreError, StoredCommand, StoredEvent,
    SubmitEvent, ToSql, Token, TokenPurpose, User, Wallpaper, action_type, command_from, failed,
    invitation_from, mark_retired, organization_from, recording_from_row, refusal_text, role_from,
    settled, stick_from, submit_event, user_from,
};

impl Store for PostgresStore {
    fn record_heartbeat(
        &self,
        organization: &str,
        heartbeat: Heartbeat,
        received_at: OffsetDateTime,
    ) -> Result<Option<u64>, StoreError> {
        let json = serde_json::to_value(&heartbeat).map_err(failed)?;
        let seq = i64::try_from(heartbeat.seq).map_err(failed)?;
        let accepted = self.in_org_transaction(organization, |transaction| {
            let stored = transaction.execute(
                "WITH newer_boot AS (
                     INSERT INTO heartbeat_boots
                         (organization_id, stick_id, boot_id, seq, first_seen, last_seen)
                     VALUES ($1, $2, $3, $4, $5, $5)
                     ON CONFLICT (stick_id, boot_id) DO UPDATE SET
                         seq = EXCLUDED.seq, last_seen = EXCLUDED.last_seen,
                         first_seen = COALESCE(heartbeat_boots.first_seen, EXCLUDED.first_seen)
                     WHERE heartbeat_boots.organization_id = EXCLUDED.organization_id
                       AND heartbeat_boots.seq < EXCLUDED.seq
                     RETURNING organization_id, stick_id, boot_id, seq
                 )
                 INSERT INTO heartbeats
                     (organization_id, stick_id, boot_id, seq, received_at, count, heartbeat)
                 SELECT organization_id, stick_id, boot_id, seq, $5, 1, $6 FROM newer_boot
                 ON CONFLICT (stick_id) DO UPDATE SET
                     boot_id = EXCLUDED.boot_id, seq = EXCLUDED.seq,
                     received_at = EXCLUDED.received_at, heartbeat = EXCLUDED.heartbeat,
                     count = heartbeats.count + 1
                 WHERE heartbeats.organization_id = EXCLUDED.organization_id",
                &[
                    &organization,
                    &heartbeat.stick_id.as_str(),
                    &heartbeat.boot_id,
                    &seq,
                    &received_at,
                    &json,
                ],
            )?;
            // If a different organization owns the latest row, roll back the boot change too.
            Ok((stored == 1, stored == 1))
        })?;
        if accepted {
            return Ok(None);
        }
        // Read the conflict detail after the write attempt. A concurrent writer may have
        // advanced the boot while this transaction waited for its row.
        let top =
            self.heartbeat_seq_of_boot(organization, &heartbeat.stick_id, &heartbeat.boot_id)?;
        top.filter(|&top| top >= heartbeat.seq)
            .map(Some)
            .ok_or_else(|| {
                StoreError("the heartbeat could not be recorded in its organization".to_owned())
            })
    }

    fn last_heartbeat(
        &self,
        organization: &str,
        stick: &StickId,
    ) -> Result<Option<SeenHeartbeat>, StoreError> {
        let row = self.in_org(organization, |transaction| {
            transaction.query_opt(
                "SELECT heartbeat, received_at, count FROM heartbeats
                 WHERE organization_id = $1 AND stick_id = $2",
                &[&organization, &stick.as_str()],
            )
        })?;
        let Some(row) = row else { return Ok(None) };
        let json: serde_json::Value = row.get(0);
        let count: i64 = row.get(2);
        Ok(Some(SeenHeartbeat {
            heartbeat: serde_json::from_value(json).map_err(failed)?,
            received_at: row.get(1),
            count: u64::try_from(count).map_err(failed)?,
        }))
    }

    fn boot_spans(
        &self,
        organization: &str,
        stick: &StickId,
        heard_since: OffsetDateTime,
    ) -> Result<Vec<BootSpan>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT boot_id, first_seen, last_seen FROM heartbeat_boots
                 WHERE organization_id = $1 AND stick_id = $2
                   AND first_seen IS NOT NULL AND last_seen >= $3",
                &[&organization, &stick.as_str(), &heard_since],
            )
        })?;
        Ok(rows
            .iter()
            .map(|row| BootSpan {
                boot_id: row.get(0),
                first_seen: row.get(1),
                last_seen: row.get(2),
            })
            .collect())
    }

    fn heartbeat_seq_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<u64>, StoreError> {
        let row = self.in_org(organization, |transaction| {
            transaction.query_opt(
                "SELECT seq FROM heartbeat_boots
                 WHERE organization_id = $1 AND stick_id = $2 AND boot_id = $3",
                &[&organization, &stick.as_str(), &boot],
            )
        })?;
        row.map(|row| u64::try_from(row.get::<_, i64>(0)).map_err(failed))
            .transpose()
    }

    fn last_event_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<StoredEvent>, StoreError> {
        let row = self.in_org(organization, |transaction| {
            transaction.query_opt(
                "SELECT event, received_at, integrity FROM events
                 WHERE organization_id = $1 AND stick_id = $2 AND boot_id = $3
                 ORDER BY id DESC LIMIT 1",
                &[&organization, &stick.as_str(), &boot],
            )
        })?;
        let Some(row) = row else { return Ok(None) };
        let json: serde_json::Value = row.get(0);
        Ok(Some(StoredEvent {
            event: serde_json::from_value(json).map_err(failed)?,
            received_at: row.get(1),
            integrity: row.get(2),
        }))
    }

    fn append_event(
        &self,
        organization: &str,
        event: Event,
        received_at: OffsetDateTime,
        integrity: Option<String>,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_value(&event).map_err(failed)?;
        let seq = i64::try_from(event.seq).map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO events
                     (organization_id, stick_id, boot_id, seq, received_at, integrity, event)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &organization,
                    &event.stick_id.as_str(),
                    &event.boot_id,
                    &seq,
                    &received_at,
                    &integrity,
                    &json,
                ],
            )
        })?;
        Ok(())
    }

    fn events(&self, organization: &str, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT event, received_at, integrity FROM events
                 WHERE organization_id = $1 AND stick_id = $2 ORDER BY id",
                &[&organization, &stick.as_str()],
            )
        })?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let json: serde_json::Value = row.get(0);
            events.push(StoredEvent {
                event: serde_json::from_value(json).map_err(failed)?,
                received_at: row.get(1),
                integrity: row.get(2),
            });
        }
        Ok(events)
    }

    fn add_judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        submissions: &[JudgeSubmission],
        received_at: OffsetDateTime,
    ) -> Result<usize, StoreError> {
        let mut numbers = Vec::with_capacity(submissions.len());
        for submission in submissions {
            numbers.push((
                i64::try_from(submission.contest_id).map_err(failed)?,
                i64::try_from(submission.submission_id).map_err(failed)?,
            ));
        }
        self.in_org(organization, |transaction| {
            let mut added = 0;
            for (submission, (contest, id)) in submissions.iter().zip(&numbers) {
                added += transaction.execute(
                    "INSERT INTO judge_submissions
                         (organization_id, judge, contest_id, submission_id, handles,
                          problem_index, submitted_at, received_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (organization_id, judge, contest_id, submission_id) DO NOTHING",
                    &[
                        &organization,
                        &judge,
                        contest,
                        id,
                        &submission.handles,
                        &submission.problem_index,
                        &submission.submitted_at,
                        &received_at,
                    ],
                )?;
            }
            Ok(added)
        })
        .map(|added| usize::try_from(added).unwrap_or(0))
    }

    fn judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        contest_id: u64,
    ) -> Result<Vec<JudgeSubmission>, StoreError> {
        let contest = i64::try_from(contest_id).map_err(failed)?;
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT submission_id, handles, problem_index, submitted_at FROM judge_submissions
                 WHERE organization_id = $1 AND judge = $2 AND contest_id = $3
                 ORDER BY submitted_at, id",
                &[&organization, &judge, &contest],
            )
        })?;
        let mut found = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row.get(0);
            found.push(JudgeSubmission {
                submission_id: u64::try_from(id).map_err(failed)?,
                contest_id,
                handles: row.get(1),
                problem_index: row.get(2),
                submitted_at: row.get(3),
            });
        }
        Ok(found)
    }

    fn add_judge_source_hash(
        &self,
        organization: &str,
        judge: &str,
        contest_id: u64,
        submission_id: u64,
        source_sha256: &str,
        fetched_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let contest = i64::try_from(contest_id).map_err(failed)?;
        let id = i64::try_from(submission_id).map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO judge_source_hashes
                     (organization_id, judge, contest_id, submission_id, source_sha256, fetched_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (organization_id, judge, contest_id, submission_id) DO NOTHING",
                &[
                    &organization,
                    &judge,
                    &contest,
                    &id,
                    &source_sha256,
                    &fetched_at,
                ],
            )
        })
        .map(|added| added > 0)
    }

    fn judge_source_hashes(
        &self,
        organization: &str,
        judge: &str,
        contest_id: u64,
    ) -> Result<HashMap<u64, String>, StoreError> {
        let contest = i64::try_from(contest_id).map_err(failed)?;
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT submission_id, source_sha256 FROM judge_source_hashes
                 WHERE organization_id = $1 AND judge = $2 AND contest_id = $3",
                &[&organization, &judge, &contest],
            )
        })?;
        let mut found = HashMap::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row.get(0);
            found.insert(u64::try_from(id).map_err(failed)?, row.get(1));
        }
        Ok(found)
    }

    fn submission_events(&self, organization: &str) -> Result<Vec<SubmitEvent>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT event, received_at, integrity FROM events
                 WHERE organization_id = $1 AND event->>'kind' = 'submission' ORDER BY id",
                &[&organization],
            )
        })?;
        let mut found = Vec::new();
        for row in rows {
            let json: serde_json::Value = row.get(0);
            let stored = StoredEvent {
                event: serde_json::from_value(json).map_err(failed)?,
                received_at: row.get(1),
                integrity: row.get(2),
            };
            found.extend(submit_event(&stored));
        }
        Ok(found)
    }

    fn create_user(&self, user: NewUser) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "INSERT INTO users (email, password_hash, email_verified_at, created_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT DO NOTHING
                 RETURNING id, email, password_hash, email_verified_at, created_at, is_platform_staff",
                &[
                    &user.email,
                    &user.password_hash,
                    &user.email_verified_at,
                    &user.created_at,
                ],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, email, password_hash, email_verified_at, created_at, is_platform_staff
                 FROM users WHERE lower(email) = lower($1)",
                &[&email],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn user(&self, id: &str) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, email, password_hash, email_verified_at, created_at, is_platform_staff
                 FROM users WHERE id = $1",
                &[&id],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn create_organization(
        &self,
        slug: &str,
        name: &str,
    ) -> Result<Option<Organization>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "INSERT INTO organizations (slug, name) VALUES ($1, $2)
                 ON CONFLICT (slug) DO NOTHING RETURNING id, slug, name",
                &[&slug, &name],
            )
        })?;
        Ok(row.as_ref().map(organization_from))
    }

    fn ensure_organization(&self, slug: &str, name: &str) -> Result<Organization, StoreError> {
        let row = self.run(|client| {
            client.execute(
                "INSERT INTO organizations (slug, name) VALUES ($1, $2)
                 ON CONFLICT (slug) DO NOTHING",
                &[&slug, &name],
            )?;
            client.query_one(
                "SELECT id, slug, name FROM organizations WHERE slug = $1",
                &[&slug],
            )
        })?;
        Ok(organization_from(&row))
    }

    fn organization_by_slug(&self, slug: &str) -> Result<Option<Organization>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, slug, name FROM organizations WHERE slug = $1",
                &[&slug],
            )
        })?;
        Ok(row.as_ref().map(organization_from))
    }

    fn add_member(
        &self,
        organization_id: &str,
        user_id: &str,
        role: Role,
    ) -> Result<bool, StoreError> {
        let added = self.run(|client| {
            client.execute(
                "INSERT INTO memberships (organization_id, user_id, role) VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
                &[&organization_id, &user_id, &role.as_str()],
            )
        })?;
        Ok(added == 1)
    }

    fn membership(&self, organization_id: &str, user_id: &str) -> Result<Option<Role>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT role FROM memberships WHERE organization_id = $1 AND user_id = $2",
                &[&organization_id, &user_id],
            )
        })?;
        row.map(|row| role_from(&row.get::<_, String>(0)))
            .transpose()
    }

    fn memberships(&self, user_id: &str) -> Result<Vec<Membership>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT organizations.id, organizations.slug, organizations.name, memberships.role
                 FROM memberships
                 JOIN organizations ON organizations.id = memberships.organization_id
                 WHERE memberships.user_id = $1 ORDER BY organizations.slug",
                &[&user_id],
            )
        })?;
        rows.iter()
            .map(|row| {
                Ok(Membership {
                    organization: organization_from(row),
                    role: role_from(&row.get::<_, String>(3))?,
                })
            })
            .collect()
    }

    fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)",
                &[&token_hash, &user_id, &expires_at],
            )
        })?;
        Ok(())
    }

    fn session(&self, token_hash: &str) -> Result<Option<(String, OffsetDateTime)>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT user_id, expires_at FROM sessions WHERE token_hash = $1",
                &[&token_hash],
            )
        })?;
        Ok(row.map(|row| (row.get(0), row.get(1))))
    }

    fn delete_session(&self, token_hash: &str) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute("DELETE FROM sessions WHERE token_hash = $1", &[&token_hash])
        })?;
        Ok(())
    }

    fn delete_expired_sessions(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.run(|client| client.execute("DELETE FROM sessions WHERE expires_at <= $1", &[&now]))?;
        Ok(())
    }

    fn mark_email_verified(&self, user_id: &str, at: OffsetDateTime) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "UPDATE users SET email_verified_at = $2 WHERE id = $1 AND email_verified_at IS NULL",
                &[&user_id, &at],
            )
        })?;
        Ok(())
    }

    fn set_password_hash(&self, user_id: &str, password_hash: &str) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "UPDATE users SET password_hash = $2 WHERE id = $1",
                &[&user_id, &password_hash],
            )
        })?;
        Ok(())
    }

    fn delete_sessions_of(&self, user_id: &str) -> Result<(), StoreError> {
        self.run(|client| client.execute("DELETE FROM sessions WHERE user_id = $1", &[&user_id]))?;
        Ok(())
    }

    fn put_email_token(
        &self,
        token_hash: &str,
        user_id: &str,
        purpose: TokenPurpose,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute(
                "DELETE FROM email_tokens WHERE user_id = $1 AND purpose = $2",
                &[&user_id, &purpose.as_str()],
            )?;
            transaction.execute(
                "INSERT INTO email_tokens (token_hash, user_id, purpose, expires_at)
                 VALUES ($1, $2, $3, $4)",
                &[&token_hash, &user_id, &purpose.as_str(), &expires_at],
            )?;
            transaction.commit()
        })
    }

    fn take_email_token(
        &self,
        token_hash: &str,
        purpose: TokenPurpose,
        now: OffsetDateTime,
    ) -> Result<Option<String>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "DELETE FROM email_tokens WHERE token_hash = $1 AND purpose = $2
                 RETURNING user_id, expires_at",
                &[&token_hash, &purpose.as_str()],
            )
        })?;
        Ok(row.and_then(|row| {
            let ends: OffsetDateTime = row.get(1);
            (ends > now).then(|| row.get(0))
        }))
    }

    fn delete_expired_email_tokens(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute("DELETE FROM email_tokens WHERE expires_at <= $1", &[&now])
        })?;
        Ok(())
    }

    fn delete_unverified_users_before(&self, before: OffsetDateTime) -> Result<(), StoreError> {
        // Sessions and tokens of these accounts go with them (ON DELETE CASCADE).
        self.run(|client| {
            client.execute(
                "DELETE FROM users WHERE email_verified_at IS NULL AND created_at < $1",
                &[&before],
            )
        })?;
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
        let row = self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute(
                "DELETE FROM invitations WHERE organization_id = $1 AND lower(email) = lower($2)",
                &[&organization_id, &email],
            )?;
            let row = transaction.query_one(
                "INSERT INTO invitations (organization_id, email, role, token_hash, invited_by, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 RETURNING id, organization_id, email, role, invited_by, expires_at",
                &[
                    &organization_id,
                    &email,
                    &role.as_str(),
                    &token_hash,
                    &invited_by,
                    &expires_at,
                ],
            )?;
            transaction.commit()?;
            Ok(row)
        })?;
        invitation_from(&row)
    }

    fn invitation_for_token(&self, token_hash: &str) -> Result<Option<Invitation>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, organization_id, email, role, invited_by, expires_at
                 FROM invitations WHERE token_hash = $1",
                &[&token_hash],
            )
        })?;
        row.as_ref().map(invitation_from).transpose()
    }

    fn take_invitation(
        &self,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Invitation>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "DELETE FROM invitations WHERE token_hash = $1
                 RETURNING id, organization_id, email, role, invited_by, expires_at",
                &[&token_hash],
            )
        })?;
        let Some(invitation) = row.as_ref().map(invitation_from).transpose()? else {
            return Ok(None);
        };
        Ok((invitation.expires_at > now).then_some(invitation))
    }

    fn invitations_of(&self, organization_id: &str) -> Result<Vec<Invitation>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT id, organization_id, email, role, invited_by, expires_at
                 FROM invitations WHERE organization_id = $1 ORDER BY lower(email)",
                &[&organization_id],
            )
        })?;
        rows.iter().map(invitation_from).collect()
    }

    fn delete_invitation(&self, organization_id: &str, id: &str) -> Result<bool, StoreError> {
        let deleted = self.run(|client| {
            client.execute(
                "DELETE FROM invitations WHERE organization_id = $1 AND id = $2",
                &[&organization_id, &id],
            )
        })?;
        Ok(deleted > 0)
    }

    fn delete_expired_invitations(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute("DELETE FROM invitations WHERE expires_at <= $1", &[&now])
        })?;
        Ok(())
    }

    fn members_of(&self, organization_id: &str) -> Result<Vec<Member>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT users.id, users.email, memberships.role
                 FROM memberships JOIN users ON users.id = memberships.user_id
                 WHERE memberships.organization_id = $1 ORDER BY lower(users.email)",
                &[&organization_id],
            )
        })?;
        rows.iter()
            .map(|row| {
                Ok(Member {
                    user_id: row.get(0),
                    email: row.get(1),
                    role: role_from(row.get::<_, &str>(2))?,
                })
            })
            .collect()
    }

    fn remove_member(&self, organization_id: &str, user_id: &str) -> Result<bool, StoreError> {
        let removed = self.run(|client| {
            client.execute(
                "DELETE FROM memberships WHERE organization_id = $1 AND user_id = $2",
                &[&organization_id, &user_id],
            )
        })?;
        Ok(removed > 0)
    }

    fn count_owners(&self, organization_id: &str) -> Result<usize, StoreError> {
        let row = self.run(|client| {
            client.query_one(
                "SELECT count(*) FROM memberships WHERE organization_id = $1 AND role = 'owner'",
                &[&organization_id],
            )
        })?;
        let count: i64 = row.get(0);
        usize::try_from(count).map_err(failed)
    }

    fn set_staff(&self, user_id: &str, staff: bool) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "UPDATE users SET is_platform_staff = $2 WHERE id = $1",
                &[&user_id, &staff],
            )
        })?;
        Ok(())
    }

    fn organizations(&self) -> Result<Vec<Organization>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT id, slug, name FROM organizations ORDER BY slug",
                &[],
            )
        })?;
        Ok(rows.iter().map(organization_from).collect())
    }

    fn add_stick(&self, organization: &str, stick: Stick) -> Result<bool, StoreError> {
        self.save_stick(organization, &stick, None)
    }

    fn retire_stick(
        &self,
        organization: &str,
        id: &StickId,
        file: Option<&Stick>,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        if let Some(file) = file.filter(|file| &file.stick_id == id) {
            return self.save_stick(organization, file, Some(now));
        }
        self.in_org(organization, |transaction| {
            mark_retired(transaction, organization, id, now)
        })
    }

    fn record_hardware_write(
        &self,
        organization: &str,
        hardware: &Hardware,
        stick: Option<&StickId>,
        now: OffsetDateTime,
    ) -> Result<u32, StoreError> {
        let stick = stick.map(StickId::as_str);
        let count: i32 = self.in_org(organization, |transaction| {
            let row = transaction.query_one(
                "INSERT INTO stick_hardware
                     (organization_id, fingerprint, serial_hint, write_count,
                      first_written_at, last_written_at, current_stick_id)
                 VALUES ($1, $2, $3, 1, $4, $4, $5)
                 ON CONFLICT (organization_id, fingerprint) DO UPDATE SET
                     write_count = stick_hardware.write_count + 1,
                     last_written_at = $4,
                     serial_hint = $3,
                     current_stick_id = COALESCE($5, stick_hardware.current_stick_id)
                 RETURNING write_count",
                &[
                    &organization,
                    &hardware.fingerprint,
                    &hardware.serial_hint,
                    &now,
                    &stick,
                ],
            )?;
            Ok(row.get(0))
        })?;
        u32::try_from(count).map_err(failed)
    }

    fn record_expected_config(
        &self,
        organization: &str,
        stick: &StickId,
        sha256: &str,
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO expected_configs (organization_id, stick_id, sha256, recorded_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (organization_id, stick_id) DO UPDATE SET
                     sha256 = EXCLUDED.sha256, recorded_at = EXCLUDED.recorded_at",
                &[&organization, &stick.as_str(), &sha256, &now],
            )
        })
        .map(|_| ())
    }

    fn expected_configs(&self, organization: &str) -> Result<Vec<(StickId, String)>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT stick_id, sha256 FROM expected_configs WHERE organization_id = $1",
                &[&organization],
            )
        })?;
        rows.iter()
            .map(|row| {
                let id: String = row.get(0);
                Ok((StickId::parse(&id).map_err(failed)?, row.get(1)))
            })
            .collect()
    }

    fn hardware(&self, organization: &str) -> Result<Vec<StickHardware>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT fingerprint, serial_hint, write_count, first_written_at,
                        last_written_at, current_stick_id
                 FROM stick_hardware WHERE organization_id = $1",
                &[&organization],
            )
        })?;
        rows.iter()
            .map(|row| {
                let current: Option<String> = row.get(5);
                Ok(StickHardware {
                    fingerprint: row.get(0),
                    serial_hint: row.get(1),
                    write_count: u32::try_from(row.get::<_, i32>(2)).map_err(failed)?,
                    first_written_at: row.get(3),
                    last_written_at: row.get(4),
                    current_stick_id: current
                        .map(|id| StickId::parse(&id).map_err(failed))
                        .transpose()?,
                })
            })
            .collect()
    }

    fn archive_stick(
        &self,
        organization: &str,
        id: &StickId,
        file: Option<&Stick>,
        now: OffsetDateTime,
        delete_after: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        if let Some(file) = file.filter(|file| &file.stick_id == id) {
            self.save_stick(organization, file, None)?;
        }
        self.in_org(organization, |transaction| {
            // The first archiving decides the time; a second one changes nothing.
            let found = transaction.execute(
                "UPDATE sticks SET
                     archived_at = COALESCE(archived_at, $3),
                     delete_after = COALESCE(delete_after, $4)
                 WHERE organization_id = $1 AND stick_id = $2 AND deleted_at IS NULL",
                &[&organization, &id.as_str(), &now, &delete_after],
            )?;
            Ok(found == 1)
        })
    }

    fn restore_stick(&self, organization: &str, id: &StickId) -> Result<bool, StoreError> {
        self.in_org(organization, |transaction| {
            let found = transaction.execute(
                "UPDATE sticks SET archived_at = NULL, delete_after = NULL
                 WHERE organization_id = $1 AND stick_id = $2
                   AND archived_at IS NOT NULL AND deleted_at IS NULL",
                &[&organization, &id.as_str()],
            )?;
            Ok(found == 1)
        })
    }

    fn sticks_due_for_deletion(
        &self,
        organization: &str,
        now: OffsetDateTime,
    ) -> Result<Vec<StickId>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT stick_id FROM sticks
                 WHERE organization_id = $1 AND deleted_at IS NULL AND delete_after <= $2",
                &[&organization, &now],
            )
        })?;
        rows.iter()
            .map(|row| StickId::parse(&row.get::<_, String>(0)).map_err(failed))
            .collect()
    }

    fn delete_stick(
        &self,
        organization: &str,
        id: &StickId,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        self.in_org_transaction(organization, |transaction| {
            let archived = transaction.query_opt(
                "SELECT 1 FROM sticks
                 WHERE organization_id = $1 AND stick_id = $2
                   AND archived_at IS NOT NULL AND deleted_at IS NULL
                 FOR UPDATE",
                &[&organization, &id.as_str()],
            )?;
            if archived.is_none() {
                return Ok((false, false));
            }
            let args: [&(dyn ToSql + Sync); 2] = [&organization, &id.as_str()];
            transaction.execute("SELECT purge_stick_events($1, $2)", &args)?;
            for table in [
                "DELETE FROM heartbeats WHERE organization_id = $1 AND stick_id = $2",
                "DELETE FROM heartbeat_boots WHERE organization_id = $1 AND stick_id = $2",
                "DELETE FROM commands WHERE organization_id = $1 AND stick_id = $2",
                "DELETE FROM recordings WHERE organization_id = $1 AND stick_id = $2",
                "DELETE FROM expected_configs WHERE organization_id = $1 AND stick_id = $2",
                "UPDATE enrolment_codes SET claimed_stick_id = NULL
                 WHERE organization_id = $1 AND claimed_stick_id = $2",
            ] {
                transaction.execute(table, &args)?;
            }
            // The tombstone keeps the id taken: a registry push or the registry file cannot bring
            // the stick back, and its blanked secret never matches again.
            transaction.execute(
                "UPDATE sticks SET token = '', credential_scope = NULL, team = '', site = '',
                     contest = '', machine_number = NULL, retired_at = COALESCE(retired_at, $3),
                     archived_at = NULL, delete_after = NULL, deleted_at = $3
                 WHERE organization_id = $1 AND stick_id = $2",
                &[&organization, &id.as_str(), &now],
            )?;
            Ok((true, true))
        })
    }

    /// The one lookup that runs without an organization: a stick does not say which one it is in.
    /// Row-level security lets it read exactly the stick it names.
    fn stick(&self, id: &StickId) -> Result<Option<(String, Stick)>, StoreError> {
        let row = self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute(
                "SELECT set_config('ck.stick_id', $1, true)",
                &[&id.as_str()],
            )?;
            let row = transaction.query_opt(
                "SELECT stick_id, token, team, site, contest, machine_number, organization_id,
                        credential_scope, retired_at, archived_at, delete_after, deleted_at
                 FROM sticks WHERE stick_id = $1",
                &[&id.as_str()],
            )?;
            transaction.commit()?;
            Ok(row)
        })?;
        row.as_ref()
            .map(|row| Ok((row.get::<_, String>(6), stick_from(row)?)))
            .transpose()
    }

    fn sticks(&self, organization: &str) -> Result<Vec<Stick>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT stick_id, token, team, site, contest, machine_number, credential_scope,
                        retired_at, archived_at, delete_after, deleted_at FROM sticks
                 WHERE organization_id = $1",
                &[&organization],
            )
        })?;
        rows.iter().map(stick_from).collect()
    }

    fn put_profile(
        &self,
        organization: &str,
        profile: NewProfile,
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let (wallpaper_name, wallpaper_data) = match &profile.wallpaper {
            Some(wallpaper) => (Some(&wallpaper.name), Some(&wallpaper.data)),
            None => (None, None),
        };
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO profiles
                     (organization_id, id, name, settings, wallpaper_name, wallpaper_data, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (organization_id, id) DO UPDATE SET
                     name = EXCLUDED.name, settings = EXCLUDED.settings,
                     wallpaper_name = EXCLUDED.wallpaper_name,
                     wallpaper_data = EXCLUDED.wallpaper_data,
                     updated_at = EXCLUDED.updated_at",
                &[
                    &organization,
                    &profile.id,
                    &profile.name,
                    &profile.settings,
                    &wallpaper_name,
                    &wallpaper_data,
                    &now,
                ],
            )
        })
        .map(|_| ())
    }

    fn profiles(&self, organization: &str) -> Result<Vec<ProfileSummary>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT id, name, updated_at FROM profiles WHERE organization_id = $1
                 ORDER BY lower(name), id",
                &[&organization],
            )
        })?;
        Ok(rows
            .iter()
            .map(|row| ProfileSummary {
                id: row.get(0),
                name: row.get(1),
                updated_at: row.get(2),
            })
            .collect())
    }

    fn profile(&self, organization: &str, id: &str) -> Result<Option<Profile>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT id, name, settings, wallpaper_name, updated_at FROM profiles
                 WHERE organization_id = $1 AND id = $2",
                &[&organization, &id],
            )
        })?;
        Ok(rows.first().map(|row| Profile {
            id: row.get(0),
            name: row.get(1),
            settings: row.get(2),
            wallpaper_name: row.get(3),
            updated_at: row.get(4),
        }))
    }

    fn profile_wallpaper(
        &self,
        organization: &str,
        id: &str,
    ) -> Result<Option<Wallpaper>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT wallpaper_name, wallpaper_data FROM profiles
                 WHERE organization_id = $1 AND id = $2 AND wallpaper_data IS NOT NULL",
                &[&organization, &id],
            )
        })?;
        Ok(rows.first().map(|row| Wallpaper {
            name: row.get(0),
            data: row.get(1),
        }))
    }

    fn delete_profile(&self, organization: &str, id: &str) -> Result<bool, StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "DELETE FROM profiles WHERE organization_id = $1 AND id = $2",
                &[&organization, &id],
            )
        })
        .map(|deleted| deleted > 0)
    }

    fn queue_command(&self, organization: &str, command: StoredCommand) -> Result<(), StoreError> {
        let signed = serde_json::to_value(&command.signed).map_err(failed)?;
        let refusal = refusal_text(command.refusal);
        let stick = command.signed.command.stick_id.to_string();
        let expires_at = command.expires_at();
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO commands
                     (organization_id, id, stick_id, sent_by_user_id, sent_by_email, action_type,
                      signed_command, state, refusal, created_at, delivered_at, finished_at,
                      expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                &[
                    &organization,
                    &command.signed.command.id,
                    &stick,
                    &command.sent_by_user_id,
                    &command.sent_by_email,
                    &action_type(&command.signed.command.action),
                    &signed,
                    &command.state.as_str(),
                    &refusal,
                    &command.created_at,
                    &command.delivered_at,
                    &command.finished_at,
                    &expires_at,
                ],
            )
        })
        .map(|_| ())
    }

    fn pending_commands(
        &self,
        organization: &str,
        stick: &StickId,
        now: OffsetDateTime,
    ) -> Result<Vec<StoredCommand>, StoreError> {
        let stick = stick.to_string();
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {COMMAND_COLUMNS} FROM commands
                     WHERE organization_id = $1 AND stick_id = $2
                       AND state IN ('queued', 'delivered') AND expires_at > $3
                     ORDER BY created_at, id"
                ),
                &[&organization, &stick, &now],
            )
        })?;
        rows.iter().map(command_from).collect()
    }

    fn mark_commands_delivered(
        &self,
        organization: &str,
        ids: &[String],
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "UPDATE commands SET state = 'delivered', delivered_at = $3
                 WHERE organization_id = $1 AND id = ANY($2) AND state = 'queued'",
                &[&organization, &ids, &now],
            )
        })
        .map(|_| ())
    }

    fn record_command_ack(
        &self,
        organization: &str,
        stick: &StickId,
        ack: &CommandAck,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let (state, refusal) = settled(ack);
        let refusal = refusal_text(refusal);
        let stick = stick.to_string();
        self.in_org(organization, |transaction| {
            transaction.execute(
                "UPDATE commands SET state = $4, refusal = $5, finished_at = $6
                 WHERE organization_id = $1 AND id = $2 AND stick_id = $3
                   AND state IN ('queued', 'delivered')",
                &[
                    &organization,
                    &ack.command_id,
                    &stick,
                    &state.as_str(),
                    &refusal,
                    &now,
                ],
            )
        })
        .map(|changed| changed > 0)
    }

    fn commands_of(
        &self,
        organization: &str,
        stick: &StickId,
        limit: u32,
    ) -> Result<Vec<StoredCommand>, StoreError> {
        let stick = stick.to_string();
        let limit = i64::from(limit);
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {COMMAND_COLUMNS} FROM commands
                     WHERE organization_id = $1 AND stick_id = $2
                     ORDER BY created_at DESC, id DESC LIMIT $3"
                ),
                &[&organization, &stick, &limit],
            )
        })?;
        rows.iter().map(command_from).collect()
    }

    fn command(&self, organization: &str, id: &str) -> Result<Option<StoredCommand>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {COMMAND_COLUMNS} FROM commands WHERE organization_id = $1 AND id = $2"
                ),
                &[&organization, &id],
            )
        })?;
        rows.first().map(command_from).transpose()
    }

    fn delete_commands_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "DELETE FROM commands WHERE organization_id = $1 AND created_at < $2",
                &[&organization, &before],
            )
        })
        .map(|_| ())
    }

    fn add_enrolment_code(
        &self,
        organization: &str,
        code: NewEnrolmentCode,
    ) -> Result<(), StoreError> {
        let scope = code
            .credential_scope
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(failed)?;
        let machine = code
            .machine_number
            .map(i32::try_from)
            .transpose()
            .map_err(failed)?;
        let fingerprint = code.hardware.as_ref().map(|h| h.fingerprint.clone());
        let serial_hint = code.hardware.as_ref().map(|h| h.serial_hint.clone());
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO enrolment_codes
                     (code_hash, organization_id, created_by_user_id, team, site, contest,
                      machine_number, created_at, expires_at, credential_scope,
                      hardware_fingerprint, hardware_serial_hint)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                &[
                    &code.code_hash,
                    &organization,
                    &code.created_by_user_id,
                    &code.team,
                    &code.site,
                    &code.contest,
                    &machine,
                    &code.created_at,
                    &code.expires_at,
                    &scope,
                    &fingerprint,
                    &serial_hint,
                ],
            )
        })
        .map(|_| ())
    }

    fn redeem_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError> {
        self.use_enrolment_code(code_hash, now, stick, token, false)
    }

    fn claim_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError> {
        self.use_enrolment_code(code_hash, now, stick, token, true)
    }

    fn delete_enrolment_codes_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "DELETE FROM enrolment_codes WHERE organization_id = $1 AND created_at < $2
                   AND (claimed_stick_id IS NULL OR used_at < $2)",
                &[&organization, &before],
            )
        })
        .map(|_| ())
    }
    fn add_recording(&self, organization: &str, recording: &Recording) -> Result<bool, StoreError> {
        let size = i64::try_from(recording.size).map_err(failed)?;
        let duration = recording
            .duration_s
            .map(i64::try_from)
            .transpose()
            .map_err(failed)?;
        let next = i32::try_from(recording.next_chunk).map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO recordings
                     (organization_id, id, stick_id, file, kind, size, sha256, started_at,
                      duration_s, next_chunk, created_at, completed_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                 ON CONFLICT (organization_id, stick_id, file) DO NOTHING",
                &[
                    &organization,
                    &recording.id,
                    &recording.stick_id,
                    &recording.file,
                    &recording.kind.as_str(),
                    &size,
                    &recording.sha256,
                    &recording.started_at,
                    &duration,
                    &next,
                    &recording.created_at,
                    &recording.completed_at,
                    &recording.expires_at,
                ],
            )
        })
        .map(|inserted| inserted == 1)
    }

    fn recording(&self, organization: &str, id: &str) -> Result<Option<Recording>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {RECORDING_COLUMNS} FROM recordings
                     WHERE organization_id = $1 AND id = $2"
                ),
                &[&organization, &id],
            )
        })?;
        rows.first().map(recording_from_row).transpose()
    }

    fn recordings_of(
        &self,
        organization: &str,
        stick: &StickId,
        limit: u32,
    ) -> Result<Vec<Recording>, StoreError> {
        let stick = stick.to_string();
        let limit = i64::from(limit);
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {RECORDING_COLUMNS} FROM recordings
                     WHERE organization_id = $1 AND stick_id = $2
                     ORDER BY created_at DESC, id DESC LIMIT $3"
                ),
                &[&organization, &stick, &limit],
            )
        })?;
        rows.iter().map(recording_from_row).collect()
    }

    fn advance_recording(
        &self,
        organization: &str,
        id: &str,
        index: u32,
    ) -> Result<bool, StoreError> {
        let index = i32::try_from(index).map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "UPDATE recordings SET next_chunk = $3 + 1
                 WHERE organization_id = $1 AND id = $2 AND next_chunk = $3
                   AND completed_at IS NULL",
                &[&organization, &id, &index],
            )
        })
        .map(|changed| changed == 1)
    }

    fn complete_recording(
        &self,
        organization: &str,
        id: &str,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "UPDATE recordings SET completed_at = $3, expires_at = $4
                 WHERE organization_id = $1 AND id = $2 AND completed_at IS NULL",
                &[&organization, &id, &now, &expires_at],
            )
        })
        .map(|changed| changed == 1)
    }

    fn due_recordings(
        &self,
        organization: &str,
        now: OffsetDateTime,
        abandoned_before: OffsetDateTime,
    ) -> Result<Vec<Recording>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {RECORDING_COLUMNS} FROM recordings
                     WHERE organization_id = $1
                       AND ((completed_at IS NOT NULL AND expires_at < $2)
                            OR (completed_at IS NULL AND created_at < $3))
                     ORDER BY created_at LIMIT 200"
                ),
                &[&organization, &now, &abandoned_before],
            )
        })?;
        rows.iter().map(recording_from_row).collect()
    }

    fn delete_recording(&self, organization: &str, id: &str) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "DELETE FROM recordings WHERE organization_id = $1 AND id = $2",
                &[&organization, &id],
            )
        })
        .map(|_| ())
    }
}
