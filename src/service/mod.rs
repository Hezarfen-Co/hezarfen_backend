//! Business rules, workflow locks, multi-call orchestration; calls db::*; no
//! SurrealQL text.

pub mod appointment;
pub mod course;
pub mod enrollment;
pub mod exam_attempt;
pub mod note;
pub mod payment_ledger;
pub mod session;
pub mod settings;
