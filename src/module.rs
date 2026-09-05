//! Per-school **module entitlements**: which parts of the product a school has
//! bought. A module is exactly one router nest (`/meals`, `/exams`, …), so the
//! unit a school is sold and the unit the router refuses are the same thing —
//! there is no per-route entitlement to keep in sync with a per-route guard.
//!
//! A disabled module answers `403 {"error":"module disabled","module":"<name>"}`
//! on every route in its nest ([`crate::web::module_gate`]), while an unmatched
//! path inside it still `404`s, because the gate is a `route_layer`.
//!
//! Stored on the school registry row as bare lowercase strings, like
//! [`crate::tenant::SchoolStatus`] — see [`crate::tenant::School::modules`].

use std::collections::BTreeSet;

use serde::Serialize;

use crate::error::{AppError, ValidationError};

/// One router nest a school may have switched on or off.
///
/// `Ord` is the declaration order and is only ever used to key the
/// [`ModuleSet`] — every user-visible ordering goes through
/// [`ModuleSet::names`], which sorts the strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Module {
    Chatbot,
    Notes,
    Messages,
    Events,
    Appointments,
    Courses,
    CourseNotes,
    Classes,
    Sessions,
    Exams,
    Marks,
    Meals,
    Payments,
    Work,
    Pomodoro,
    Questions,
    BankQuestions,
    Attendance,
    Subjects,
    Homework,
    Boards,
}

/// The commercial bundle a module is sold in. A package is a name for a set of
/// modules and nothing more — entitlement is always stored per module, so a
/// re-packaging never has to migrate a school's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Package {
    Academics,
    Communication,
    Operations,
    Ai,
}

impl Module {
    pub const ALL: [Module; 21] = [
        Module::Chatbot,
        Module::Notes,
        Module::Messages,
        Module::Events,
        Module::Appointments,
        Module::Courses,
        Module::CourseNotes,
        Module::Classes,
        Module::Sessions,
        Module::Exams,
        Module::Marks,
        Module::Meals,
        Module::Payments,
        Module::Work,
        Module::Pomodoro,
        Module::Questions,
        Module::BankQuestions,
        Module::Attendance,
        Module::Subjects,
        Module::Homework,
        Module::Boards,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Module::Chatbot => "chatbot",
            Module::Notes => "notes",
            Module::Messages => "messages",
            Module::Events => "events",
            Module::Appointments => "appointments",
            Module::Courses => "courses",
            Module::CourseNotes => "course_notes",
            Module::Classes => "classes",
            Module::Sessions => "sessions",
            Module::Exams => "exams",
            Module::Marks => "marks",
            Module::Meals => "meals",
            Module::Payments => "payments",
            Module::Work => "work",
            Module::Pomodoro => "pomodoro",
            Module::Questions => "questions",
            Module::BankQuestions => "bank_questions",
            Module::Attendance => "attendance",
            Module::Subjects => "subjects",
            Module::Homework => "homework",
            Module::Boards => "boards",
        }
    }

    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        Module::ALL
            .into_iter()
            .find(|m| m.as_str() == value)
            .ok_or_else(|| ValidationError::Unknown {
                field: "module",
                value: value.to_string(),
            })
    }

    /// The modules this one cannot work without. Every edge is a hard
    /// structural reference — a `record<…>` field in
    /// [`crate::migration_sql`] or a handler that reads the other module's row
    /// — never a merely-nicer-together pairing; the evidence is on each line.
    pub fn requires(self) -> &'static [Module] {
        match self {
            // `DEFINE FIELD course ON subject TYPE record<course>`
            Module::Subjects => &[Module::Courses],
            // `DEFINE FIELD course ON course_note TYPE record<course>`
            Module::CourseNotes => &[Module::Courses],
            // `DEFINE FIELD course ON course_session TYPE record<course>`
            Module::Sessions => &[Module::Courses],
            // `DEFINE FIELD course ON exam TYPE record<course>` +
            // `DEFINE FIELD subject ON exam_question TYPE record<subject>`
            Module::Exams => &[Module::Courses, Module::Subjects],
            // `DEFINE FIELD course ON homework TYPE record<course>` +
            // `DEFINE FIELD subject ON homework TYPE record<subject>`
            Module::Homework => &[Module::Courses, Module::Subjects],
            // `DEFINE FIELD course ON class_course TYPE record<course>` — a
            // class is bulk enrollment into courses.
            Module::Classes => &[Module::Courses],
            // `DEFINE FIELD exam ON exam_result TYPE record<exam>`; the marks
            // report is a weighted read of exam results.
            Module::Marks => &[Module::Exams],
            // The report tallies both halves: `attendance.event
            // record<event>` and `session_attendance.session
            // record<course_session>`.
            Module::Attendance => &[Module::Events, Module::Sessions],
            // No edge: `bank_question.subject` is `option<record<subject>>`,
            // so a bank question stands on its own.
            _ => &[],
        }
    }

    pub fn package(self) -> Package {
        match self {
            Module::Courses
            | Module::Subjects
            | Module::Sessions
            | Module::Exams
            | Module::Marks
            | Module::Homework
            | Module::Classes
            | Module::CourseNotes
            | Module::Attendance
            | Module::BankQuestions => Package::Academics,
            Module::Notes
            | Module::Messages
            | Module::Events
            | Module::Appointments
            | Module::Questions
            | Module::Boards => Package::Communication,
            Module::Meals | Module::Payments | Module::Work | Module::Pomodoro => {
                Package::Operations
            }
            Module::Chatbot => Package::Ai,
        }
    }

    /// The modules that require this one — the reverse of [`Module::requires`],
    /// derived rather than written down, so the two can never disagree.
    pub fn dependents(self) -> Vec<Module> {
        Module::ALL
            .into_iter()
            .filter(|other| other.requires().contains(&self))
            .collect()
    }
}

impl std::fmt::Display for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Package {
    pub const ALL: [Package; 4] = [
        Package::Academics,
        Package::Communication,
        Package::Operations,
        Package::Ai,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Package::Academics => "academics",
            Package::Communication => "communication",
            Package::Operations => "operations",
            Package::Ai => "ai",
        }
    }

    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        Package::ALL
            .into_iter()
            .find(|p| p.as_str() == value)
            .ok_or_else(|| ValidationError::Unknown {
                field: "package",
                value: value.to_string(),
            })
    }

    /// The modules this package sells, derived from [`Module::package`].
    pub fn modules(self) -> Vec<Module> {
        Module::ALL
            .into_iter()
            .filter(|m| m.package() == self)
            .collect()
    }
}

/// What one school has switched on. Ordered, so a set has exactly one
/// serialization and a diff between two schools reads the same every time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModuleSet(BTreeSet<Module>);

impl ModuleSet {
    pub fn all() -> Self {
        ModuleSet(Module::ALL.into_iter().collect())
    }

    pub fn empty() -> Self {
        ModuleSet(BTreeSet::new())
    }

    pub fn contains(&self, module: Module) -> bool {
        self.0.contains(&module)
    }

    pub fn insert(&mut self, module: Module) {
        self.0.insert(module);
    }

    pub fn remove(&mut self, module: Module) {
        self.0.remove(&module);
    }

    pub fn iter(&self) -> impl Iterator<Item = Module> + '_ {
        self.0.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Parse a client-supplied list. An unknown name is refused by name rather
    /// than dropped: silently ignoring it would sell a school a module that
    /// does not exist and report success.
    pub fn from_names(names: &[String]) -> Result<Self, ValidationError> {
        let mut set = BTreeSet::new();
        for name in names {
            set.insert(Module::try_from_str(name)?);
        }
        Ok(ModuleSet(set))
    }

    /// The stored form: sorted, so the row is byte-stable.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.iter().map(|m| m.as_str().to_string()).collect();
        names.sort();
        names
    }

    /// Every `(module, missing requirement)` pair this set holds, in a stable
    /// order.
    pub fn missing_requirements(&self) -> Vec<(Module, Module)> {
        self.iter()
            .flat_map(|module| {
                module
                    .requires()
                    .iter()
                    .filter(|needed| !self.contains(**needed))
                    .map(move |needed| (module, *needed))
            })
            .collect()
    }

    /// Refuse a set that switches a module on without what it structurally
    /// needs. One `409` naming **every** violation, so a caller fixing a set
    /// does not discover them one round trip at a time.
    pub fn validate(&self) -> Result<(), AppError> {
        let missing = self.missing_requirements();
        if missing.is_empty() {
            return Ok(());
        }
        let message = missing
            .into_iter()
            .map(|(module, needed)| format!("{module} requires {needed}, which is not enabled"))
            .collect::<Vec<_>>()
            .join("; ");
        Err(AppError::ConflictOwned(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_module_names_itself_and_round_trips() {
        assert_eq!(Module::ALL.len(), 21);
        let mut seen = BTreeSet::new();
        for module in Module::ALL {
            assert!(seen.insert(module.as_str()), "duplicate {module}");
            assert_eq!(Module::try_from_str(module.as_str()).unwrap(), module);
        }
        assert!(Module::try_from_str("course-notes").is_err(), "snake_case");
        assert!(Module::try_from_str("").is_err());
    }

    /// A cycle would make some set of modules unsatisfiable — every member
    /// would need another member that needs it back.
    #[test]
    fn the_dependency_graph_is_acyclic() {
        fn depth(module: Module, seen: &mut Vec<Module>) {
            assert!(!seen.contains(&module), "cycle through {module}: {seen:?}");
            seen.push(module);
            for needed in module.requires() {
                depth(*needed, seen);
            }
            seen.pop();
        }
        for module in Module::ALL {
            depth(module, &mut Vec::new());
        }
    }

    #[test]
    fn every_module_is_sold_in_exactly_one_package() {
        let packaged: Vec<Module> = Package::ALL
            .into_iter()
            .flat_map(Package::modules)
            .collect();
        assert_eq!(packaged.len(), Module::ALL.len());
        for module in Module::ALL {
            assert_eq!(
                packaged.iter().filter(|m| **m == module).count(),
                1,
                "{module} is not in exactly one package"
            );
        }
    }

    #[test]
    fn dependents_is_the_exact_reverse_of_requires() {
        for module in Module::ALL {
            for dependent in module.dependents() {
                assert!(dependent.requires().contains(&module));
            }
            for needed in module.requires() {
                assert!(needed.dependents().contains(&module));
            }
        }
        // The phrasing a caller builds from the reverse edges.
        assert_eq!(Module::Exams.dependents(), vec![Module::Marks]);
        assert_eq!(
            Module::Courses.dependents(),
            vec![
                Module::CourseNotes,
                Module::Classes,
                Module::Sessions,
                Module::Exams,
                Module::Subjects,
                Module::Homework,
            ]
        );
    }

    #[test]
    fn a_full_set_is_valid_and_a_lone_module_names_all_it_misses() {
        assert!(ModuleSet::all().validate().is_ok());
        assert!(ModuleSet::empty().validate().is_ok());

        let mut lone = ModuleSet::empty();
        lone.insert(Module::Exams);
        let err = lone.validate().expect_err("exams alone is unsatisfiable");
        let message = err.to_string();
        assert!(
            message.contains("exams requires courses, which is not enabled"),
            "{message}"
        );
        assert!(
            message.contains("exams requires subjects, which is not enabled"),
            "both misses in one message: {message}"
        );
        assert!(matches!(err, AppError::ConflictOwned(_)));
    }

    #[test]
    fn names_round_trip_and_an_unknown_name_is_refused() {
        let all = ModuleSet::all();
        let names = all.names();
        assert_eq!(names.len(), 21);
        assert!(names.windows(2).all(|w| w[0] < w[1]), "sorted: {names:?}");
        assert_eq!(ModuleSet::from_names(&names).unwrap(), all);

        let err = ModuleSet::from_names(&["meals".into(), "kantin".into()]).unwrap_err();
        assert!(
            err.to_string().contains("`kantin`"),
            "names the offender: {err}"
        );
    }
}
