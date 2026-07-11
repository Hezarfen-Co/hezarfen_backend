use surrealdb::types::SurrealValue;

/// A unix-millisecond instant. Stored as an `int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, SurrealValue)]
pub struct Timestamp(i64);

impl Timestamp {
    pub fn now() -> Self {
        Self(chrono::Utc::now().timestamp_millis())
    }

    pub fn in_days(days: i64) -> Self {
        Self((chrono::Utc::now() + chrono::Duration::days(days)).timestamp_millis())
    }

    pub fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    pub fn as_millis(&self) -> i64 {
        self.0
    }

    pub fn is_past(&self) -> bool {
        self.0 < Self::now().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn millis_roundtrip() {
        assert_eq!(
            Timestamp::from_millis(1_700_000_000_000).as_millis(),
            1_700_000_000_000
        );
    }

    #[tokio::test]
    async fn ordering() {
        assert!(Timestamp::from_millis(1) < Timestamp::from_millis(2));
    }

    #[tokio::test]
    async fn expiry_direction() {
        assert!(!Timestamp::in_days(1).is_past());
        assert!(Timestamp::in_days(-1).is_past());
    }
}
