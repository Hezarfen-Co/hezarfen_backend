//! Domain layer: every value is a validated newtype, and each entity owns its
//! own persistence. Types derive `surrealdb::types::SurrealValue` so the exact
//! same typed value flows from HTTP input all the way into the database.

pub mod attendance;
pub mod event;
pub mod exam;
pub mod exam_result;
pub mod note;
pub mod profile;
pub mod role;
pub mod session;
pub mod timestamp;
pub mod user;
