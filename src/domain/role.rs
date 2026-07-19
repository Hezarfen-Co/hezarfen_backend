use surrealdb::types::SurrealValue;

use crate::error::ValidationError;

/// The five access levels, in ascending order of privilege.
///
/// The variants are declared low-to-high, so the derived `Ord` matches the
/// hierarchy: `Role::Admin > Role::Teacher`. That ordering is exactly what
/// [`Role::at_least`] relies on — a higher role satisfies any lower requirement.
///
/// `Parent` sits at the bottom: a read-only observer of the students linked to
/// it (see `domain::parent_link`). It clears no `at_least` bar and fails every
/// exact `== Student` gate, so parents can't sit exams, enroll, or be marked on
/// a roll call — they only read their own students' reports.
///
/// `#[surreal(untagged, rename_all = "lowercase")]` makes each variant serialize
/// to a bare lowercase string (`"student"`, `"teacher"`, …) instead of the
/// default object-wrapped form, so it stores in the `role` column's `TYPE string`
/// and round-trips straight back into the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum Role {
    Parent,
    Student,
    Teacher,
    Manager,
    Admin,
}

impl Role {
    /// Every role, lowest privilege first.
    pub const ALL: [Role; 5] = [
        Role::Parent,
        Role::Student,
        Role::Teacher,
        Role::Manager,
        Role::Admin,
    ];

    /// The wire/storage form. Must stay in lockstep with `rename_all = "lowercase"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Parent => "parent",
            Role::Student => "student",
            Role::Teacher => "teacher",
            Role::Manager => "manager",
            Role::Admin => "admin",
        }
    }

    /// Parse a wire string into a role — the inverse of [`Role::as_str`].
    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        Self::ALL
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::Value;

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
        assert!(Role::ALL.iter().all(|&r| Role::Admin.at_least(r)));
    }

    #[tokio::test]
    async fn str_round_trips() {
        for role in Role::ALL {
            assert_eq!(Role::try_from_str(role.as_str()).unwrap(), role);
        }
        assert!(Role::try_from_str("wizard").is_err());
        assert!(Role::try_from_str("").is_err());
    }

    #[tokio::test]
    async fn surreal_value_is_a_plain_string() {
        // The whole point of `untagged`: the stored value is a bare string, so a
        // `TYPE string` column accepts it. Guard that the encoding never regresses.
        for role in Role::ALL {
            let value = role.into_value();
            assert_eq!(value, Value::String(role.as_str().to_string()));
            assert_eq!(Role::from_value(value).unwrap(), role);
        }
    }
}
