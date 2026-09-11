//! Business rules, workflow locks, multi-call orchestration; calls db::*; no
//! SurrealQL text.

pub mod appointment;
pub mod appointment_slot;
pub mod bank_question;
pub mod bank_question_image;
pub mod class_blueprint;
pub mod course;
pub mod course_session;
pub mod enrollment;
pub mod exam;
pub mod exam_attempt;
pub mod exam_question;
pub mod meal_booking;
pub mod note;
pub mod payment_ledger;
pub mod pool_question;
pub mod session;
pub mod settings;
pub mod solution;
pub mod user;
