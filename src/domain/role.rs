use sqlx::Type;

use crate::constant::ROLES;
use crate::error::ValidationError;

/// The access levels, in ascending order of privilege.
///
/// The variants are declared low-to-high, so the derived `Ord` matches the
/// hierarchy: `Role::Admin > Role::Teacher`. That ordering is exactly what
/// [`Role::at_least`] relies on — a higher role satisfies any lower requirement.
///
/// `Ai` is not a human role at all: it is the principal an out-of-process AI
/// service carries over the QUIC bridge. It is declared first so it clears no
/// `at_least` bar above itself, it is absent from [`ROLES`] (the assignable
/// set) so [`Role::try_from_str`] rejects `"ai"`, and no HTTP surface can put
/// it on a user row — the only value of it lives in `User::ai_principal`.
///
/// `Parent` sits at the bottom: a read-only observer of the students linked to
/// it (see `domain::parent_link`). It clears no `at_least` bar and fails every
/// exact `== Student` gate, so parents can't sit exams, enroll, or be marked on
/// a roll call — they only read their own students' reports.
///
/// [`Role`] derives [`sqlx::Type`] as a bare TEXT enum: each variant stores as
/// its lowercase name (`"student"`, `"teacher"`, …) and round-trips straight
/// back, which the `role` column's `CHECK` lists and every `role = 'teacher'`
/// filter spells. [`Role::as_str`] must stay in lockstep with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum Role {
    Ai,
    Parent,
    Student,
    Teacher,
    Manager,
    Admin,
}

impl Role {
    /// The wire/storage form. Must stay in lockstep with `rename_all = "lowercase"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Ai => "ai",
            Role::Parent => "parent",
            Role::Student => "student",
            Role::Teacher => "teacher",
            Role::Manager => "manager",
            Role::Admin => "admin",
        }
    }

    /// Parse a wire string into a role — the inverse of [`Role::as_str`] for
    /// the assignable roles only. It searches [`ROLES`], which excludes
    /// [`Role::Ai`], so `"ai"` is rejected like any other unknown word.
    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        ROLES
            .into_iter()
            .find(|role| role.as_str() == value)
            .ok_or(ValidationError::Invalid {
                field: "role",
                reason: "must be one of: parent, student, teacher, manager, admin",
            })
    }

    /// True when `self` is at least as privileged as `min` (the hierarchy check).
    pub fn at_least(self, min: Role) -> bool {
        self >= min
    }

    /// May `self` open a message thread with `recipient`? Messaging is upward
    /// only for the two roles below staff: a `Student` or `Parent` writes to
    /// teachers and above, never sideways or down (issue #23). Staff
    /// (`Teacher`+) write to anyone. [`Role::Ai`] is a service principal with
    /// no mailbox, so it neither sends nor receives.
    pub fn may_message(self, recipient: Role) -> bool {
        if self == Role::Ai || recipient == Role::Ai {
            return false;
        }
        self.at_least(Role::Teacher) || recipient.at_least(Role::Teacher)
    }

    /// The roles `self` is allowed to write to, when that set is narrower than
    /// "everyone" — `None` means unrestricted. Backs the `/users/search`
    /// restriction, which must land in the query so `total` stays right.
    pub fn messageable_roles(self) -> Option<&'static [Role]> {
        match self.at_least(Role::Teacher) {
            true => None,
            false => Some(&[Role::Teacher, Role::Manager, Role::Admin]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hierarchy_orders_low_to_high() {
        assert!(Role::Parent < Role::Student);
        assert!(Role::Student < Role::Teacher);
        assert!(Role::Teacher < Role::Manager);
        assert!(Role::Manager < Role::Admin);
    }

    #[tokio::test]
    async fn parent_clears_no_bar_above_itself() {
        // A parent is an observer: it must never satisfy a staff (or even
        // student) requirement through the hierarchy.
        assert!(Role::Parent.at_least(Role::Parent));
        assert!(!Role::Parent.at_least(Role::Student));
        assert!(!Role::Parent.at_least(Role::Teacher));
    }

    #[tokio::test]
    async fn at_least_is_inclusive_and_upward() {
        // A teacher clears the teacher bar and everything below it.
        assert!(Role::Teacher.at_least(Role::Teacher));
        assert!(Role::Teacher.at_least(Role::Student));
        assert!(!Role::Teacher.at_least(Role::Manager));
        // Admin clears every bar.
        assert!(ROLES.iter().all(|&r| Role::Admin.at_least(r)));
    }

    #[tokio::test]
    async fn ai_is_a_service_principal_no_client_can_reach() {
        // Lowest ordinal: it satisfies no requirement a real role can be asked
        // for, so an AI request clears nothing through the hierarchy.
        assert!(ROLES.iter().all(|&r| !Role::Ai.at_least(r)));
        assert!(ROLES.iter().all(|&r| r.at_least(Role::Ai)));
        // Not assignable: absent from ROLES, so no wire string parses to it.
        assert!(!ROLES.contains(&Role::Ai));
        assert!(Role::try_from_str("ai").is_err());
        assert_eq!(Role::Ai.as_str(), "ai");
    }

    #[tokio::test]
    async fn messaging_is_upward_only_below_staff() {
        // Sideways and downward writes from the two non-staff roles: refused.
        for sender in [Role::Student, Role::Parent] {
            for recipient in [Role::Student, Role::Parent] {
                assert!(!sender.may_message(recipient));
            }
            for staff in [Role::Teacher, Role::Manager, Role::Admin] {
                assert!(sender.may_message(staff));
            }
            assert_eq!(
                sender.messageable_roles(),
                Some(&[Role::Teacher, Role::Manager, Role::Admin][..])
            );
        }
        // Staff write to anyone, in any direction, and are unrestricted in search.
        for sender in [Role::Teacher, Role::Manager, Role::Admin] {
            assert!(ROLES.iter().all(|&r| sender.may_message(r)));
            assert_eq!(sender.messageable_roles(), None);
        }
        // The service principal has no mailbox in either direction.
        assert!(ROLES.iter().all(|&r| !Role::Ai.may_message(r)));
        assert!(ROLES.iter().all(|&r| !r.may_message(Role::Ai)));
    }

    #[tokio::test]
    async fn str_round_trips() {
        for role in ROLES {
            assert_eq!(Role::try_from_str(role.as_str()).unwrap(), role);
        }
        assert!(Role::try_from_str("wizard").is_err());
        assert!(Role::try_from_str("").is_err());
    }

    #[test]
    fn sqlx_encodes_the_storage_form() {
        // The whole point of the TEXT encoding: the stored value is the bare
        // lowercase string, so the `role` column accepts it and every
        // `role = 'teacher'` filter matches. Guard that the sqlx encoding
        // never drifts from `as_str` — the CHECK constraint and the filters
        // spell these same words.
        let mut buf = sqlx::postgres::PgArgumentBuffer::default();
        for role in [Role::Ai, Role::Parent, Role::Student, Role::Teacher, Role::Manager, Role::Admin] {
            buf.clear();
            let _ = sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&role, &mut buf).unwrap();
            assert_eq!(std::str::from_utf8(&buf).unwrap(), role.as_str());
        }
    }
}
