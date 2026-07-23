//! Domain layer: every value is a validated newtype, and each entity owns its
//! own persistence. Types derive `surrealdb::types::SurrealValue` so the exact
//! same typed value flows from HTTP input all the way into the database.

pub mod answer_image;
pub mod appointment;
pub mod appointment_slot;
pub mod attendance;
pub mod chat_message;
pub mod conversation;
pub mod course;
pub mod course_session;
pub mod enrollment;
pub mod event;
pub mod exam;
pub mod exam_answer;
pub mod exam_attempt;
pub mod exam_question;
pub mod exam_result;
pub mod homework;
pub mod homework_file;
pub mod homework_result;
pub mod homework_submission;
pub mod message;
pub mod note;
pub mod note_file;
pub mod parent_link;
pub mod pomodoro;
pub mod pool_question;
pub mod preferences;
pub mod profile;
pub mod question_image;
pub mod registration;
pub mod role;
pub mod session;
pub mod session_attendance;
pub mod settings;
pub mod solution;
pub mod subject;
pub mod term;
pub mod timestamp;
pub mod user;
pub mod work_entry;
