//! Inviting people to an organization, accepting an invitation, and managing members.

use time::Duration;

use crate::accounts::{AccessError, token_hash, unavailable, valid_email, valid_slug};
use crate::hub::Hub;
use crate::mail::Mail;
use crate::store::{Invitation, Member, Membership, Organization, Role, StoreError};

pub const INVITATION_LIFETIME_DAYS: i64 = 7;

#[derive(Debug, PartialEq, Eq)]
pub enum InviteError {
    /// The Hub was started without a way to send mail.
    NoMail,
    BadEmail,
    Unauthorized,
    /// A member who may not invite this role.
    Forbidden,
    /// No such organization, or the account is not in it. The two look the same.
    NotFound,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AcceptError {
    Unauthorized,
    /// The invitation is for another address. It stays open for the right account.
    WrongAccount,
    /// Unknown, used or ended. The reasons look the same.
    Invalid,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MemberError {
    Unauthorized,
    Forbidden,
    NotFound,
    /// An organization always keeps one owner.
    LastOwner,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StaffError {
    Unauthorized,
    /// The account is not platform staff.
    Forbidden,
    NoMail,
    BadEmail,
    BadSlug,
    SlugTaken,
    Unavailable,
}

fn failed(error: &StoreError) {
    unavailable(error);
}

impl From<AccessError> for InviteError {
    fn from(error: AccessError) -> Self {
        match error {
            AccessError::Unauthorized => Self::Unauthorized,
            AccessError::Forbidden => Self::Forbidden,
            AccessError::NotFound | AccessError::Ambiguous => Self::NotFound,
            AccessError::Unavailable => Self::Unavailable,
        }
    }
}

impl From<AccessError> for MemberError {
    fn from(error: AccessError) -> Self {
        match error {
            AccessError::Unauthorized => Self::Unauthorized,
            AccessError::Forbidden => Self::Forbidden,
            AccessError::NotFound | AccessError::Ambiguous => Self::NotFound,
            AccessError::Unavailable => Self::Unavailable,
        }
    }
}

impl Hub {
    /// Invites an address to the organization with `role` and mails a link. The answer is the same
    /// for an address with an account, one without, and one already in the organization.
    pub fn invite(
        &self,
        session: &str,
        slug: &str,
        email: &str,
        role: Role,
    ) -> Result<(), InviteError> {
        if self.mailer.is_none() {
            return Err(InviteError::NoMail);
        }
        let access = self.access(session, slug, Role::Admin)?;
        if !access.role.may_manage(role) {
            return Err(InviteError::Forbidden);
        }
        if !valid_email(email) {
            return Err(InviteError::BadEmail);
        }
        let now = (self.clock)();
        let store_failed = |error: StoreError| {
            failed(&error);
            InviteError::Unavailable
        };
        let existing = self.store.user_by_email(email).map_err(store_failed)?;
        let is_member = match &existing {
            Some(user) => self
                .store
                .membership(&access.organization.id, &user.id)
                .map_err(store_failed)?
                .is_some(),
            None => false,
        };
        if !self.accounts.mail_allowed(email, now) {
            return Ok(());
        }
        if is_member {
            self.send(Mail {
                to: email.to_owned(),
                subject: format!("You are already in {}", access.organization.name),
                body: format!(
                    "Someone invited this address to {} on Keeper Hub, but you are already in it. \
                     Log in at {}/.",
                    access.organization.name,
                    self.base_url()
                ),
            });
            return Ok(());
        }
        self.store
            .delete_expired_invitations(now)
            .map_err(store_failed)?;
        let token = ck_crypto::generate_stick_token();
        let token = token.expose();
        self.store
            .put_invitation(
                &access.organization.id,
                email,
                role,
                &token_hash(token),
                Some(&access.account.id),
                now + Duration::days(INVITATION_LIFETIME_DAYS),
            )
            .map_err(store_failed)?;
        self.send(Mail {
            to: email.to_owned(),
            subject: format!("Join {} on Keeper Hub", access.organization.name),
            body: format!(
                "{} invited you to {} on Keeper Hub as {}. Open the link while logged in with \
                 this address (make an account first if you have none). It works once, for {} \
                 days.\n\n{}/#invite={token}\n",
                access.account.email,
                access.organization.name,
                role.as_str(),
                INVITATION_LIFETIME_DAYS,
                self.base_url()
            ),
        });
        Ok(())
    }

    /// Uses an invitation for the logged-in account, which must have the invited address. Gives
    /// back the organization and the role the account has in it afterwards.
    pub fn accept_invitation(
        &self,
        session: &str,
        invite_token: &str,
    ) -> Result<Membership, AcceptError> {
        let account = self.account(session).map_err(|error| match error {
            AccessError::Unavailable => AcceptError::Unavailable,
            _ => AcceptError::Unauthorized,
        })?;
        let store_failed = |error: StoreError| {
            failed(&error);
            AcceptError::Unavailable
        };
        let hash = token_hash(invite_token);
        let now = (self.clock)();
        let invitation = self
            .store
            .invitation_for_token(&hash)
            .map_err(store_failed)?
            .filter(|invitation| invitation.expires_at > now)
            .ok_or(AcceptError::Invalid)?;
        // Looked at before it is used up, so the wrong account does not spoil it for the right one.
        if !invitation.email.eq_ignore_ascii_case(&account.email) {
            return Err(AcceptError::WrongAccount);
        }
        let invitation = self
            .store
            .take_invitation(&hash, now)
            .map_err(store_failed)?
            .ok_or(AcceptError::Invalid)?;
        // Already a member is fine: the invitation is used up and the role stays as it is.
        self.store
            .add_member(&invitation.organization_id, &account.id, invitation.role)
            .map_err(store_failed)?;
        self.organizations_of(&account)
            .map_err(|_| AcceptError::Unavailable)?
            .into_iter()
            .find(|m| m.organization.id == invitation.organization_id)
            .ok_or(AcceptError::Unavailable)
    }

    pub fn members_of(&self, session: &str, slug: &str) -> Result<Vec<Member>, MemberError> {
        let access = self.access(session, slug, Role::Admin)?;
        self.store
            .members_of(&access.organization.id)
            .map_err(|error| {
                failed(&error);
                MemberError::Unavailable
            })
    }

    /// Removes a member. Anyone may leave; otherwise only someone of a higher role may remove.
    pub fn remove_member(
        &self,
        session: &str,
        slug: &str,
        user_id: &str,
    ) -> Result<(), MemberError> {
        let access = self.access(session, slug, Role::Viewer)?;
        let store_failed = |error: StoreError| {
            failed(&error);
            MemberError::Unavailable
        };
        let target = self
            .store
            .members_of(&access.organization.id)
            .map_err(store_failed)?
            .into_iter()
            .find(|member| member.user_id == user_id)
            .ok_or(MemberError::NotFound)?;
        let own = target.user_id == access.account.id;
        if !own && !access.role.may_manage(target.role) {
            return Err(MemberError::Forbidden);
        }
        if target.role == Role::Owner
            && self
                .store
                .count_owners(&access.organization.id)
                .map_err(store_failed)?
                <= 1
        {
            return Err(MemberError::LastOwner);
        }
        self.store
            .remove_member(&access.organization.id, user_id)
            .map_err(store_failed)?;
        Ok(())
    }

    pub fn invitations_of(
        &self,
        session: &str,
        slug: &str,
    ) -> Result<Vec<Invitation>, MemberError> {
        let access = self.access(session, slug, Role::Admin)?;
        self.store
            .invitations_of(&access.organization.id)
            .map_err(|error| {
                failed(&error);
                MemberError::Unavailable
            })
    }

    /// Cancels an open invitation, for those who may have made it.
    pub fn revoke_invitation(
        &self,
        session: &str,
        slug: &str,
        invitation_id: &str,
    ) -> Result<(), MemberError> {
        let access = self.access(session, slug, Role::Admin)?;
        let store_failed = |error: StoreError| {
            failed(&error);
            MemberError::Unavailable
        };
        let invitation = self
            .store
            .invitations_of(&access.organization.id)
            .map_err(store_failed)?
            .into_iter()
            .find(|invitation| invitation.id == invitation_id)
            .ok_or(MemberError::NotFound)?;
        if !access.role.may_manage(invitation.role) {
            return Err(MemberError::Forbidden);
        }
        self.store
            .delete_invitation(&access.organization.id, invitation_id)
            .map_err(store_failed)?;
        Ok(())
    }

    /// For platform staff: makes an organization and invites its first owner by mail. The staff
    /// account does not become a member.
    pub fn staff_create_organization(
        &self,
        session: &str,
        slug: &str,
        name: &str,
        owner_email: &str,
    ) -> Result<Organization, StaffError> {
        let account = self.staff_account(session)?;
        if self.mailer.is_none() {
            return Err(StaffError::NoMail);
        }
        if !valid_slug(slug) || name.trim().is_empty() || name.chars().any(char::is_control) {
            return Err(StaffError::BadSlug);
        }
        if !valid_email(owner_email) {
            return Err(StaffError::BadEmail);
        }
        let store_failed = |error: StoreError| {
            failed(&error);
            StaffError::Unavailable
        };
        let organization = self
            .store
            .create_organization(slug, name)
            .map_err(store_failed)?
            .ok_or(StaffError::SlugTaken)?;
        let token = ck_crypto::generate_stick_token();
        let token = token.expose();
        let now = (self.clock)();
        self.store
            .put_invitation(
                &organization.id,
                owner_email,
                Role::Owner,
                &token_hash(token),
                Some(&account.id),
                now + Duration::days(INVITATION_LIFETIME_DAYS),
            )
            .map_err(store_failed)?;
        // Staff are trusted, so this mail is not counted against the per-address limit.
        self.send(Mail {
            to: owner_email.to_owned(),
            subject: format!("Join {} on Keeper Hub", organization.name),
            body: format!(
                "You were invited to be the owner of {} on Keeper Hub. Open the link while \
                 logged in with this address (make an account first if you have none). It works \
                 once, for {} days.\n\n{}/#invite={token}\n",
                organization.name,
                INVITATION_LIFETIME_DAYS,
                self.base_url()
            ),
        });
        Ok(organization)
    }

    /// Every organization, by name only. Nothing an organization holds is shown.
    pub fn staff_organizations(&self, session: &str) -> Result<Vec<Organization>, StaffError> {
        self.staff_account(session)?;
        self.store.organizations().map_err(|error| {
            failed(&error);
            StaffError::Unavailable
        })
    }

    fn staff_account(&self, session: &str) -> Result<crate::accounts::Account, StaffError> {
        let account = self.account(session).map_err(|error| match error {
            AccessError::Unavailable => StaffError::Unavailable,
            _ => StaffError::Unauthorized,
        })?;
        if account.staff {
            Ok(account)
        } else {
            Err(StaffError::Forbidden)
        }
    }
}
