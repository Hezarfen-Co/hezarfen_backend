//! Domain layer: every value is a validated newtype, and each entity owns its
//! own persistence. Row structs derive `sqlx::FromRow` so the exact same typed
//! value flows from HTTP input all the way into PostgreSQL; transparent
//! newtypes derive `sqlx::Type` so a column reads straight into the validated
//! wrapper.

pub mod answer_image;
pub mod appointment;
pub mod appointment_slot;
pub mod attendance;
pub mod badge;
pub mod bank_question;
pub mod bank_question_image;
pub mod board;
pub mod board_stroke;
pub mod builder;
pub mod chatbot_message;
pub mod chatbot_thread;
pub mod class_blueprint;
pub mod class_course;
pub mod class_group;
pub mod class_member;
pub mod course;
pub mod course_note;
pub mod course_note_file;
pub mod course_session;
pub mod dietary_profile;
pub mod enrollment;
pub mod event;
pub mod exam;
pub mod exam_answer;
pub mod exam_attempt;
pub mod exam_question;
pub mod exam_result;
pub mod fee_plan;
pub mod fee_plan_assignment;
pub mod homework;
pub mod homework_file;
pub mod homework_result;
pub mod homework_submission;
pub mod key;
pub mod meal_attendance;
pub mod meal_booking;
pub mod meal_ledger;
pub mod menu;
pub mod menu_dish;
pub mod message;
pub mod monotonic_id;
pub mod note;
pub mod note_file;
pub mod parent_link;
pub mod payment_ledger;
pub mod pomodoro;
pub mod pool_question;
pub mod pool_question_image;
pub mod preferences;
pub mod profile;
pub mod question_image;
pub mod rag_output;
pub mod registration;
pub mod person;
pub mod role;
pub mod session;
pub mod session_attendance;
pub mod settings;
pub mod solution;
pub mod solution_image;
pub mod subject;
pub mod term;
pub mod text_fold;
pub mod timestamp;
pub mod user;
pub mod work_entry;

/// Fallback for lowercase-string enums whose `#[sqlx(type_name = "TEXT",
/// rename_all = "lowercase")]` derive the `sqlx::Type` macro rejects (an
/// exotic shape, not a plain C-like enum). Emits the same thing the derive
/// would: `Type` reporting `TEXT`, `Encode` writing the lowercase name,
/// `Decode` reading a TEXT value back and refusing an unknown one as a decode
/// error — never a panic. Unused today: every current enum derives fine; kept
/// so the fallback shape stays one `macro_rules!` away, identically across
/// the domain.
#[allow(unused_macros)]
macro_rules! text_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        impl sqlx::Type<sqlx::Postgres> for $name {
            fn type_info() -> sqlx::postgres::PgTypeInfo {
                sqlx::postgres::PgTypeInfo::with_name("TEXT")
            }
        }

        impl<'r> sqlx::Decode<'r, sqlx::Postgres> for $name {
            fn decode(
                value: sqlx::postgres::PgValueRef<'r>,
            ) -> Result<Self, sqlx::error::BoxDynError> {
                let text: &str = sqlx::Decode::<sqlx::Postgres>::decode(value)?;
                match text {
                    $(stringify!($variant) => Ok(Self::$variant),)+
                    other => Err(format!(
                        concat!("unknown ", stringify!($name), " value: `{}`"),
                        other
                    )
                    .into()),
                }
            }
        }

        impl<'q> sqlx::Encode<'q, sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut sqlx::postgres::PgArgumentBuffer,
            ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
                sqlx::Encode::<sqlx::Postgres>::encode(
                    match self {
                        $(Self::$variant => stringify!($variant),)+
                    },
                    buf,
                )
            }
        }
    };
}
