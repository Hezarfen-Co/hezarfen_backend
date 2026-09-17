//! The table of capabilities the **backend** serves — the operations a
//! service calls over the bridge's client-initiated `CapabilityRequest` shape
//! (`hab/2`'s third request kind), and each one's scope.
//!
//! There is one table per capability family, and this module is the walk over
//! them: [`crate::ai::insight`] serves ZEKA's storage surface,
//! [`crate::ai::podcast`] serves the podcast job's state reports. Both answer
//! the same frame, both are dispatched from the same place — the bridge's
//! capability dispatcher in `ai::server` — and neither owns the other's
//! names: a name neither table holds is `unknown_capability`, with no prefix
//! match and no fallback, exactly as the two tables' own doc comments say.
//!
//! The scope decides one thing only: whether the bridge resolves the frame's
//! school before the operation runs. A [`Scope::School`] operation runs against
//! the named school's own database — the frame's school is the *only* tenancy
//! input, and a slug that matches nothing is `unknown_school`, never a
//! fallback to another school or to the control database.

use serde_json::Value;

use crate::ai::podcast;
use crate::constant::AI_PODCAST_REPORT_CAPABILITY;
use crate::database::Database;
use crate::tenant::ResolvedTenant;

pub use crate::ai::insight::Scope;

/// The refusal shape every backend-served capability answers with: a flat,
/// stable code and a message that names the field or reason. Same alias as
/// [`crate::ai::insight::Refusal`] — one vocabulary across families.
pub type Refusal = (&'static str, String);

/// Every capability the podcast family serves, and its scope. One entry today;
/// a table rather than a match arm so the walk in [`scope_of`] and the
/// dispatch in [`serve`] are structurally the same decision, and a second
/// podcast operation is one added row.
pub const PODCAST_SERVED: &[(&str, Scope)] = &[(AI_PODCAST_REPORT_CAPABILITY, Scope::School)];

/// The scope of one backend-served capability, or `None` if no family serves
/// it. Matched exactly, family by family, in a fixed order.
pub fn scope_of(capability: &str) -> Option<Scope> {
    crate::ai::insight::scope_of(capability).or_else(|| {
        PODCAST_SERVED
            .iter()
            .find(|(name, _)| *name == capability)
            .map(|(_, scope)| *scope)
    })
}

/// Run one backend-served capability. The caller (the bridge) resolves the
/// school *before* this is called, from [`scope_of`]'s answer — a
/// [`Scope::Deployment`] operation gets `None`, and a [`Scope::School`] one
/// always gets the tenant.
pub async fn serve(
    capability: &str,
    school: Option<&ResolvedTenant>,
    control: &Database,
    payload: &Value,
) -> Result<Value, Refusal> {
    if crate::ai::insight::scope_of(capability).is_some() {
        return crate::ai::insight::serve(capability, school, control, payload).await;
    }
    match capability {
        AI_PODCAST_REPORT_CAPABILITY => {
            let Some(tenant) = school else {
                return Err((
                    "internal",
                    format!("`{AI_PODCAST_REPORT_CAPABILITY}` is school-scoped but no school was resolved"),
                ));
            };
            // The frame's payload is cloned once per report call, and a report
            // is a handful of short fields — this is the price of one dispatch
            // table across two families that decode differently.
            podcast::report(&tenant.db, payload.clone()).await
        }
        _ => Err((
            "unknown_capability",
            format!("the backend serves no `{capability}` operation"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_name_resolves_and_a_stranger_does_not() {
        assert_eq!(scope_of(AI_PODCAST_REPORT_CAPABILITY), Some(Scope::School));
        assert_eq!(scope_of("podcast.submit"), None, "worker-only, not served");
        assert_eq!(scope_of("podcast.status"), None, "deleted: the row answers");
        assert_eq!(scope_of("podcast.nothing"), None);
        assert_eq!(scope_of(""), None);
    }

    #[test]
    fn the_podcast_table_is_exactly_its_documented_vocabulary() {
        assert_eq!(PODCAST_SERVED.len(), 1);
        assert!(PODCAST_SERVED.iter().all(|(_, scope)| *scope == Scope::School));
    }
}
