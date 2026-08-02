//! The food program: menus and the dishes on them, the seats booked against
//! them, and the money that moved.
//!
//! Reads are open to every authenticated user (a student picks their lunch),
//! writes are manager+ — publishing what the school serves is kitchen
//! administration, not teaching. The balance and the ledger are the exception:
//! they are manager+ like `/payments`, since canteen debt is family debt.
//!
//! Money only ever *appends* here (see [`MealLedger`]): booking charges a price
//! snapshot, cancelling reverses it, and recording a payment is admin-only.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::MAX_DISHES_PER_MENU;
use crate::database::Database;
use crate::domain::dietary_profile::{DietaryNote, DietaryProfile, DietaryTags, conflicts};
use crate::domain::meal_attendance::{MealAttendance, MealAttendanceStatus};
use crate::domain::meal_booking::{MealBooking, MealBookingId, MealCutoff};
use crate::domain::meal_ledger::{LedgerAmount, LedgerMethod, LedgerNote, MealLedger};
use crate::domain::menu::{MENU_LOCK, Menu, MenuDate, MenuId, MenuSlot, validate_capacity};
use crate::domain::menu_dish::{
    DishDescription, DishName, DishPrice, DishTags, MenuDish, MenuDishId,
};
use crate::domain::parent_link::ParentLink;
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireAdmin, RequireManager, RequireTeacher,
    person_map, set_or_clear,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_menu, list_menus))
        .routes(routes!(get_menu, update_menu, delete_menu))
        .routes(routes!(add_dish))
        .routes(routes!(update_dish, delete_dish))
        .routes(routes!(book_meal, list_menu_bookings))
        .routes(routes!(my_bookings))
        .routes(routes!(cancel_booking))
        .routes(routes!(mark_attendance, list_menu_attendance))
        .routes(routes!(user_attendance))
        .routes(routes!(my_profile))
        .routes(routes!(user_profile, update_profile))
        .routes(routes!(my_balance))
        .routes(routes!(user_balance))
        .routes(routes!(user_ledger))
        .routes(routes!(record_credit))
}

#[derive(Deserialize, ToSchema)]
struct CreateMenu {
    /// The calendar day, `YYYY-MM-DD`. Text, not a timestamp: "one menu per
    /// day and slot" is the school's own day, not UTC midnight.
    #[schema(example = "2026-09-14", min_length = 10, max_length = 10)]
    date: String,
    /// One of the school's `meal_slots` (see `GET /settings`).
    #[schema(example = "lunch")]
    slot: String,
    /// How many students may book it. Omit (or `null`) for uncapped.
    #[schema(minimum = 0, maximum = 10000, example = 120)]
    capacity: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateMenu {
    /// New seat cap; `null` clears it back to uncapped. `date` and `slot` are
    /// immutable — a menu on another day is another menu.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>, minimum = 0, maximum = 10000)]
    capacity: Option<Option<i64>>,
}

/// Inclusive `YYYY-MM-DD` bounds on `GET /meals/menus`. Either may be omitted.
#[derive(Debug, Deserialize, IntoParams)]
struct MenuRange {
    /// Keep menus on or after this day.
    #[param(example = "2026-09-01")]
    from: Option<String>,
    /// Keep menus on or before this day.
    #[param(example = "2026-09-30")]
    to: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct CreateDish {
    #[schema(example = "Mercimek çorbası", max_length = 100)]
    name: String,
    #[schema(max_length = 500)]
    description: Option<String>,
    /// Price in **minor units** (kuruş) — `4550` is 45,50 ₺. Integer only:
    /// this API never speaks decimals or floats about money.
    #[schema(minimum = 0, maximum = 1000000, example = 4550_i64)]
    price_minor: i64,
    /// Dietary tags, each one of the school's `dietary_tags` (`GET /settings`).
    #[schema(max_items = 10, example = json!(["vegetarian"]))]
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateDish {
    #[schema(max_length = 100)]
    name: Option<String>,
    /// `null` (or blank) clears the description.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>, max_length = 500)]
    description: Option<Option<String>>,
    #[schema(minimum = 0, maximum = 1000000)]
    price_minor: Option<i64>,
    #[schema(max_items = 10)]
    tags: Option<Vec<String>>,
}

#[derive(Serialize, ToSchema)]
struct DishResponse {
    id: String,
    #[schema(example = "Mercimek çorbası")]
    name: String,
    description: Option<String>,
    /// Minor units (kuruş).
    price_minor: i64,
    tags: Vec<String>,
    /// Which of `tags` the **calling user's** own dietary profile flags — the
    /// set intersection of the two lists. Empty when they do not overlap, and
    /// empty for every reader without a profile (staff included).
    #[schema(example = json!(["nut_allergy"]))]
    conflicts: Vec<String>,
    created_at: i64,
}

impl DishResponse {
    fn new(dish: &MenuDish, viewer_tags: &[String]) -> Self {
        Self {
            id: dish.get_id().key().to_string(),
            name: dish.get_name().as_str().to_string(),
            description: dish.get_description().map(|text| text.as_str().to_string()),
            price_minor: dish.get_price_minor().as_minor(),
            tags: dish.get_tags().as_slice().to_vec(),
            conflicts: conflicts(dish.get_tags().as_slice(), viewer_tags),
            created_at: dish.get_created_at().as_millis(),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct MenuResponse {
    id: String,
    /// The calendar day, `YYYY-MM-DD`.
    #[schema(example = "2026-09-14")]
    date: String,
    /// The meal slot, snapshotted when the menu was published — it stays as it
    /// was even if the school later retires the slot.
    #[schema(example = "lunch")]
    slot: String,
    /// Seat cap; `null` = uncapped.
    capacity: Option<i64>,
    /// Everything on offer, in the order it was added.
    dishes: Vec<DishResponse>,
    created_by: PersonRef,
    created_at: i64,
}

impl MenuResponse {
    fn new(
        menu: &Menu,
        dishes: &[MenuDish],
        people: &std::collections::HashMap<String, PersonRef>,
        viewer_tags: &[String],
    ) -> Self {
        Self {
            id: menu.get_id().key().to_string(),
            date: menu.get_date().as_str().to_string(),
            slot: menu.get_slot().as_str().to_string(),
            capacity: menu.get_capacity(),
            dishes: dishes
                .iter()
                .filter(|dish| dish.get_menu() == menu.get_id())
                .map(|dish| DishResponse::new(dish, viewer_tags))
                .collect(),
            created_by: PersonRef::resolve(people, menu.get_created_by()),
            created_at: menu.get_created_at().as_millis(),
        }
    }
}

/// Join dishes, publishers, and the reader's own dietary tags onto a page of
/// menus — three queries for the whole page, never one per row. The profile in
/// particular is read **once**, not per dish.
async fn menu_responses(
    menus: &[Menu],
    viewer: &UserId,
    db: &Database,
) -> Result<Vec<MenuResponse>, AppError> {
    let ids: Vec<MenuId> = menus.iter().map(|menu| menu.get_id().clone()).collect();
    let dishes = MenuDish::list_for_menus(&ids, db).await?;
    let people = person_map(menus.iter().map(|menu| menu.get_created_by().clone()), db).await?;
    let viewer_tags = DietaryProfile::tags_of(viewer, db).await?;
    Ok(menus
        .iter()
        .map(|menu| MenuResponse::new(menu, &dishes, &people, &viewer_tags))
        .collect())
}

/// One menu, with its dishes.
async fn one_menu(
    menu: &Menu,
    viewer: &UserId,
    db: &Database,
) -> Result<Json<MenuResponse>, AppError> {
    let dishes = MenuDish::list_for_menu(menu.get_id(), db).await?;
    let people = person_map([menu.get_created_by().clone()], db).await?;
    let viewer_tags = DietaryProfile::tags_of(viewer, db).await?;
    Ok(Json(MenuResponse::new(
        menu,
        &dishes,
        &people,
        &viewer_tags,
    )))
}

/// Publish a menu for one day and meal slot. Requires manager+. The slot must
/// be one the school currently serves, and a day+slot already published is a
/// `409` — edit that menu instead of publishing a second one.
#[utoipa::path(
    post,
    path = "/menus",
    tag = "meals",
    security(("session_cookie" = [])),
    request_body = CreateMenu,
    responses(
        (status = 201, description = "Menu published", body = MenuResponse),
        (status = 400, description = "Malformed date, unknown slot, or out-of-range capacity", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "A menu already exists for that date and slot, or the slot was removed from the settings mid-request", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_menu(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Json(req): Json<CreateMenu>,
) -> Result<(StatusCode, Json<MenuResponse>), AppError> {
    let date = MenuDate::try_new(&req.date)?;
    let slot = MenuSlot::try_new(&req.slot, &Settings::load(&st.db).await?.get_meal_slots())?;
    validate_capacity(req.capacity)?;
    let menu = Menu::create(date, slot, req.capacity, user.get_id(), &st.db).await?;
    let body = one_menu(&menu, user.get_id(), &st.db).await?;
    Ok((StatusCode::CREATED, body))
}

/// List published menus, newest day first, each with its dishes. Any
/// authenticated user. Narrow to a date range with `?from=&to=` (inclusive,
/// `YYYY-MM-DD`). Paged via `?limit=&offset=` (omit `limit` for the full list);
/// returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/menus",
    tag = "meals",
    security(("session_cookie" = [])),
    params(MenuRange, PageParams),
    responses(
        (status = 200, description = "A page of menus (the full list when unpaged)", body = Page<MenuResponse>),
        (status = 400, description = "Malformed date bound, limit, or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_menus(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(range): Query<MenuRange>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<MenuResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let from = range.from.as_deref().map(MenuDate::try_new).transpose()?;
    let to = range.to.as_deref().map(MenuDate::try_new).transpose()?;
    let (menus, total) = Menu::list(from.as_ref(), to.as_ref(), limit, offset, &st.db).await?;
    // The dish/people join runs over the page alone, so it shrinks with it.
    let items = menu_responses(&menus, user.get_id(), &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch one menu with its dishes.
#[utoipa::path(
    get,
    path = "/menus/{id}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Menu id")),
    responses(
        (status = 200, description = "The menu", body = MenuResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_menu(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<MenuResponse>, AppError> {
    let menu = Menu::read(&MenuId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    one_menu(&menu, user.get_id(), &st.db).await
}

/// Change a menu's seat cap. Requires manager+. `date` and `slot` are
/// immutable; `"capacity": null` goes back to uncapped.
#[utoipa::path(
    patch,
    path = "/menus/{id}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Menu id")),
    request_body = UpdateMenu,
    responses(
        (status = 200, description = "Updated menu", body = MenuResponse),
        (status = 400, description = "Capacity out of range", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_menu(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateMenu>,
) -> Result<Json<MenuResponse>, AppError> {
    if let Some(capacity) = req.capacity {
        validate_capacity(capacity)?;
    }
    let menu = Menu::read(&MenuId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let updated = menu.update(req.capacity, &st.db).await?;
    one_menu(&updated, user.get_id(), &st.db).await
}

/// Unpublish a menu. Requires manager+. Its dishes go with it — a dish has no
/// meaning apart from the menu it was published on. A menu somebody still
/// holds a seat on is a `409`: cancel the bookings first.
#[utoipa::path(
    delete,
    path = "/menus/{id}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Menu id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The menu still has live bookings", body = ErrorResponse),
    ),
)]
async fn delete_menu(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let menu = Menu::read(&MenuId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // The seat check rides in the delete's own `WHERE` (see `Menu::delete`), so
    // a concurrent booking cannot slip between the read and the delete.
    menu.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Add a dish to a menu. Requires manager+. Tags must come from the school's
/// `dietary_tags`, and a menu holds at most 50 dishes (`409` at the cap).
#[utoipa::path(
    post,
    path = "/menus/{id}/dishes",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Menu id")),
    request_body = CreateDish,
    responses(
        (status = 201, description = "Dish added", body = DishResponse),
        (status = 400, description = "Invalid name, price, or unknown dietary tag", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such menu", body = ErrorResponse),
        (status = 409, description = "The menu already carries the maximum number of dishes", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn add_dish(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<CreateDish>,
) -> Result<(StatusCode, Json<DishResponse>), AppError> {
    let name = DishName::try_new(&req.name)?;
    let description = req
        .description
        .as_deref()
        .map(DishDescription::try_new)
        .transpose()?
        .flatten();
    let price = DishPrice::try_new(req.price_minor)?;
    let tags = DishTags::try_new(&req.tags, &Settings::load(&st.db).await?.get_dietary_tags())?;
    // Dish writes take [`MENU_LOCK`] for the dish cap alone: count-then-write
    // is write-skew, so the count and the insert have to be one step. The
    // *price* no longer needs it — a dish write moves the menu's revision, and
    // a booking claims its seat at the revision it priced itself against.
    // The lock is a leaf again: the revision bump rides the dish write's own
    // transaction now, so nothing held under it takes `cap`'s counter lock. It
    // stays a leaf only while that holds — see [`MENU_LOCK`] for the order a
    // caller that changes it must keep.
    let _guard = MENU_LOCK.lock().await;
    // The menu is read *inside* the lock: read before it, a `DELETE /menus/{id}`
    // running in the gap takes its cascade with it and this dish lands on a menu
    // that no longer exists.
    let menu = Menu::read(&MenuId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if MenuDish::count_for_menu(menu.get_id(), &st.db).await? >= MAX_DISHES_PER_MENU {
        return Err(AppError::Conflict(
            "the menu already carries the maximum number of dishes",
        ));
    }
    let dish = MenuDish::create(menu.get_id(), name, description, price, tags, &st.db).await?;
    let viewer_tags = DietaryProfile::tags_of(user.get_id(), &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(DishResponse::new(&dish, &viewer_tags)),
    ))
}

/// Edit a dish. Requires manager+. Omitted fields keep their value;
/// `"description": null` clears it, and `tags` replaces the whole list.
#[utoipa::path(
    patch,
    path = "/dishes/{did}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("did" = String, Path, description = "Dish id")),
    request_body = UpdateDish,
    responses(
        (status = 200, description = "Updated dish", body = DishResponse),
        (status = 400, description = "Invalid name, price, or unknown dietary tag", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_dish(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Path(did): Path<String>,
    Json(req): Json<UpdateDish>,
) -> Result<Json<DishResponse>, AppError> {
    let dish = MenuDish::read(&MenuDishId::from_key(&did), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let name = req.name.as_deref().map(DishName::try_new).transpose()?;
    // Absent keeps, `null` clears, and a blank string clears too — an empty
    // description is the absence of one, never a stored empty string.
    let description = match req.description {
        None => None,
        Some(None) => Some(None),
        Some(Some(text)) => Some(DishDescription::try_new(&text)?),
    };
    let price = req.price_minor.map(DishPrice::try_new).transpose()?;
    let tags = match &req.tags {
        Some(tags) => Some(DishTags::try_new(
            tags,
            &Settings::load(&st.db).await?.get_dietary_tags(),
        )?),
        None => None,
    };
    // Under [`MENU_LOCK`] like every dish write — see `add_dish`.
    let _guard = MENU_LOCK.lock().await;
    let updated = dish.update(name, description, price, tags, &st.db).await?;
    let viewer_tags = DietaryProfile::tags_of(user.get_id(), &st.db).await?;
    Ok(Json(DishResponse::new(&updated, &viewer_tags)))
}

/// Remove a dish from its menu. Requires manager+.
#[utoipa::path(
    delete,
    path = "/dishes/{did}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("did" = String, Path, description = "Dish id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn delete_dish(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(did): Path<String>,
) -> Result<StatusCode, AppError> {
    let dish = MenuDish::read(&MenuDishId::from_key(&did), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // Under [`MENU_LOCK`] like every dish write — see `add_dish`.
    let _guard = MENU_LOCK.lock().await;
    dish.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- dietary profiles -----------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct UpdateDietaryProfile {
    /// Replaces the whole tag list; each entry must be one of the school's
    /// `dietary_tags` (`GET /settings`) — the same vocabulary a dish is tagged
    /// from, which is what makes a menu's `conflicts` computable.
    #[schema(max_items = 10, example = json!(["nut_allergy"]))]
    tags: Option<Vec<String>>,
    /// Free text for the kitchen; `null` (or blank) clears it.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>, max_length = 500, example = "carries an EpiPen")]
    note: Option<Option<String>>,
}

#[derive(Serialize, ToSchema)]
struct DietaryProfileResponse {
    student: PersonRef,
    /// The tags the school recorded; empty while there is no profile.
    tags: Vec<String>,
    note: Option<String>,
    /// Who last wrote it, and when — both absent while no profile exists.
    updated_by: Option<PersonRef>,
    updated_at: Option<i64>,
}

/// One student's profile, or the empty one. A student who was never recorded
/// has *no* restrictions, so that reads back as an empty profile rather than a
/// `404` — the caller asked "what may they not eat", and "nothing known" is an
/// answer.
async fn profile_response(
    student: &UserId,
    db: &Database,
) -> Result<Json<DietaryProfileResponse>, AppError> {
    let profile = DietaryProfile::read(student, db).await?;
    let people = person_map(
        std::iter::once(student.clone())
            .chain(profile.as_ref().map(|row| row.get_updated_by().clone())),
        db,
    )
    .await?;
    Ok(Json(DietaryProfileResponse {
        student: PersonRef::resolve(&people, student),
        tags: profile
            .as_ref()
            .map(|row| row.get_tags().as_slice().to_vec())
            .unwrap_or_default(),
        note: profile
            .as_ref()
            .and_then(|row| row.get_note())
            .map(|note| note.as_str().to_string()),
        updated_by: profile
            .as_ref()
            .map(|row| PersonRef::resolve(&people, row.get_updated_by())),
        updated_at: profile.as_ref().map(|row| row.get_updated_at().as_millis()),
    }))
}

/// The caller's own dietary profile — what the school recorded about their
/// diet. Empty tags mean nothing was ever recorded.
#[utoipa::path(
    get,
    path = "/profiles/me",
    tag = "meals",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's dietary profile", body = DietaryProfileResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_profile(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<DietaryProfileResponse>, AppError> {
    profile_response(user.get_id(), &st.db).await
}

/// One student's dietary profile. Requires teacher+, or a parent link to them
/// — the caller's own id always passes.
#[utoipa::path(
    get,
    path = "/profiles/{user}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "Student id")),
    responses(
        (status = 200, description = "The student's dietary profile", body = DietaryProfileResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link", body = ErrorResponse),
    ),
)]
async fn user_profile(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
) -> Result<Json<DietaryProfileResponse>, AppError> {
    let target = UserId::from_key(&user);
    ensure_can_read_student(&caller, &target, &st.db).await?;
    profile_response(&target, &st.db).await
}

/// Record what a student may not eat. **Manager+**, deliberately: an allergen
/// list is a safety record the school keeps on the student's behalf, not a
/// self-service preference — a student editing their own would let a mistyped
/// (or removed) allergy reach the kitchen with the school's authority behind
/// it. Omitted fields keep their value; `tags` replaces the whole list, and
/// `"note": null` clears the note. First write creates the row.
#[utoipa::path(
    patch,
    path = "/profiles/{user}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "Student id")),
    request_body = UpdateDietaryProfile,
    responses(
        (status = 200, description = "The stored profile", body = DietaryProfileResponse),
        (status = 400, description = "Unknown dietary tag, too many tags, an over-long note, or a non-student target", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such user", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_profile(
    State(st): State<AppState>,
    RequireManager(manager): RequireManager,
    Path(user): Path<String>,
    Json(req): Json<UpdateDietaryProfile>,
) -> Result<Json<DietaryProfileResponse>, AppError> {
    let target = UserId::from_key(&user);
    // Only a student eats off the school's menus; a profile on anyone else is
    // a typo, and a typo here is an allergy filed against the wrong person.
    if User::read(&target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?
        .get_role()
        != Role::Student
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user",
            reason: "only a student carries a dietary profile",
        }));
    }
    let tags = match &req.tags {
        Some(tags) => Some(DietaryTags::try_new(
            tags,
            &Settings::load(&st.db).await?.get_dietary_tags(),
        )?),
        None => None,
    };
    // Absent keeps, `null` clears, and a blank string clears too — an empty
    // note is the absence of one, never a stored empty string.
    let note = match req.note {
        None => None,
        Some(None) => Some(None),
        Some(Some(text)) => Some(DietaryNote::try_new(&text)?),
    };
    DietaryProfile::save(&target, tags, note, manager.get_id(), &st.db).await?;
    profile_response(&target, &st.db).await
}

// ---- bookings -------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct BookMeal {
    /// Who the seat is for. Omit to book for yourself (a student's own lunch);
    /// a parent must name one of their linked students.
    student_id: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct BookingResponse {
    id: String,
    /// The menu the seat is on.
    menu_id: String,
    student: PersonRef,
    /// Who placed it — the student themselves, or their parent.
    booked_by: PersonRef,
    /// `booked` or `cancelled`. A cancel keeps the row (audit), it only stops
    /// the seat counting against the menu's capacity.
    #[schema(example = "booked")]
    status: String,
    cancelled_at: Option<i64>,
    created_at: i64,
}

impl BookingResponse {
    fn new(booking: &MealBooking, people: &std::collections::HashMap<String, PersonRef>) -> Self {
        Self {
            id: booking.get_id().key().to_string(),
            menu_id: booking.get_menu().key().to_string(),
            student: PersonRef::resolve(people, booking.get_student()),
            booked_by: PersonRef::resolve(people, booking.get_booked_by()),
            status: booking.get_status().as_str().to_string(),
            cancelled_at: booking.get_cancelled_at().map(|at| at.as_millis()),
            created_at: booking.get_created_at().as_millis(),
        }
    }
}

/// Join the people onto a page of bookings — one query for the whole page.
async fn booking_responses(
    bookings: &[MealBooking],
    db: &Database,
) -> Result<Vec<BookingResponse>, AppError> {
    let people = person_map(
        bookings.iter().flat_map(|booking| {
            [
                booking.get_student().clone(),
                booking.get_booked_by().clone(),
            ]
        }),
        db,
    )
    .await?;
    Ok(bookings
        .iter()
        .map(|booking| BookingResponse::new(booking, &people))
        .collect())
}

/// The school's one meal deadline, applied to booking and cancelling alike:
/// the cutoff minutes plus the slot serving times they count back from. Read
/// live on every call, so a serving time corrected today moves the deadline of
/// menus already published for it.
async fn meal_cutoff(db: &Database) -> Result<MealCutoff, AppError> {
    Ok(MealCutoff::from_settings(&Settings::load(db).await?))
}

/// Whose seat is this? A student books only for themselves; a parent only for
/// a student they hold a link to. Nobody else books through this route — a
/// teacher or manager arranging someone's lunch is not a thing this API does.
///
/// The link row alone is not the grant: like a stale enrollment, a link whose
/// student side changed role must be inert, so the target's live role is
/// re-read here. A missing or non-student target falls through to the same
/// 403 — a parent never gets an existence oracle.
async fn booking_target(
    caller: &User,
    student_id: Option<&str>,
    db: &Database,
) -> Result<UserId, AppError> {
    match caller.get_role() {
        Role::Student => match student_id {
            Some(id) if id != caller.get_id().key() => Err(AppError::Forbidden(
                "a student books meals only for themselves",
            )),
            _ => Ok(caller.get_id().clone()),
        },
        Role::Parent => {
            let target = UserId::from_key(student_id.ok_or(AppError::Validation(
                ValidationError::Invalid {
                    field: "student_id",
                    reason: "a parent must name the student the seat is for",
                },
            ))?);
            if ParentLink::exists(caller.get_id(), &target, db).await?
                && User::read(&target, db)
                    .await?
                    .is_some_and(|target| target.get_role() == Role::Student)
            {
                return Ok(target);
            }
            Err(AppError::Forbidden(
                "a parent books meals only for a linked student",
            ))
        }
        _ => Err(AppError::Forbidden(
            "only students and their parents book meals",
        )),
    }
}

/// Take a seat on a published menu. A student books for themselves (omit
/// `student_id`); a parent books for a linked student by naming them. Booking
/// twice is the same seat, not a second one — and bills once, at the price the
/// menu carried when the seat was first taken. Refused (`409`) when the menu is
/// full or the school's `meal_cancel_cutoff_minutes` has closed the meal, and
/// (`400`) when the menu's dishes sum past the chargeable maximum, since a seat
/// is never handed out unbilled.
#[utoipa::path(
    post,
    path = "/menus/{id}/bookings",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Menu id")),
    request_body = BookMeal,
    responses(
        (status = 201, description = "Seat booked (or the seat already held)", body = BookingResponse),
        (status = 400, description = "A parent named no student, or the menu's dishes sum past the chargeable maximum", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not a student booking for themselves, nor a parent booking for a linked student", body = ErrorResponse),
        (status = 404, description = "No such menu", body = ErrorResponse),
        (status = 409, description = "The menu is full, its cutoff has passed, or the menu kept being edited while the seat was being taken", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn book_meal(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<BookMeal>,
) -> Result<(StatusCode, Json<BookingResponse>), AppError> {
    let student = booking_target(&user, req.student_id.as_deref(), &st.db).await?;
    let cutoff = meal_cutoff(&st.db).await?;
    let menu = MenuId::from_key(&id);
    // Price, seat and charge land as one decision (see `MealBooking::book`),
    // keyed by (seat, attempt) — a double-click books one seat and bills it
    // once, and a dish landing between the price and the seat is refused by the
    // claim rather than billed.
    let booking = MealBooking::book(&menu, &student, user.get_id(), &cutoff, &st.db).await?;
    let items = booking_responses(std::slice::from_ref(&booking), &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(items.into_iter().next().expect("one booking in, one out")),
    ))
}

/// The caller's own bookings, newest first: the seats held *for* them, plus —
/// for a parent — the seats held for every student they currently hold a link
/// to. The links are re-read on every call, so an unlinked parent stops seeing
/// the child's meals at once, even the ones they booked themselves. Cancelled
/// bookings stay in the list, flagged. Paged via `?limit=&offset=` (omit
/// `limit` for all of them); returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/bookings/me",
    tag = "meals",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of bookings (all of them when unpaged)", body = Page<BookingResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_bookings(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<BookingResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Own seats always; a parent's children only while the link is live — the
    // booking row's `booked_by` is history, never a standing read grant.
    let mut students = vec![user.get_id().clone()];
    if user.get_role() == Role::Parent {
        students.extend(
            ParentLink::list_for_parent(user.get_id(), &st.db)
                .await?
                .iter()
                .map(|link| link.get_student().clone()),
        );
    }
    let (rows, total) = MealBooking::list_for_students(&students, limit, offset, &st.db).await?;
    let items = booking_responses(&rows, &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Who is eating: every booking on one menu, cancelled ones included so the
/// kitchen can see what changed. Requires manager+ — this is the whole school's
/// list, not one family's. Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/menus/{id}/bookings",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Menu id"), PageParams),
    responses(
        (status = 200, description = "A page of bookings (all of them when unpaged)", body = Page<BookingResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such menu", body = ErrorResponse),
    ),
)]
async fn list_menu_bookings(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<BookingResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let menu = Menu::read(&MenuId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let (rows, total) = MealBooking::list_for_menu(menu.get_id(), limit, offset, &st.db).await?;
    let items = booking_responses(&rows, &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Give the seat back. The row survives, flipped to `cancelled` — the seat is
/// free for someone else, and the cancellation stays auditable. Only the
/// student it is for, or their parent, may cancel it — and for them it is
/// refused (`409`) once the school's `meal_cancel_cutoff_minutes` has closed
/// the meal.
///
/// Manager+ may cancel anyone's booking, and is not bound by the cutoff — the
/// seat and the money have to stay reachable when the student it was booked for
/// is no longer a student, or when the meal has already closed.
///
/// **Idempotent**: cancelling an already-cancelled booking is a `200` with the
/// row as it stands, not a `409`. Repeating it also replays the refund, so a
/// cancel that was cut short mid-flight (the tab closed, a proxy timed out) is
/// recovered by simply sending it again — the reversal is keyed to the seat's
/// attempt, so the money comes back exactly once however often it is retried.
#[utoipa::path(
    delete,
    path = "/bookings/{bid}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("bid" = String, Path, description = "Booking id")),
    responses(
        (status = 200, description = "Cancelled (also when it already was — the call is idempotent and replays the refund)", body = BookingResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the booking's student, their parent, nor a manager", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The cutoff has passed (students and parents only), or the menu was too contended to free the seat", body = ErrorResponse),
    ),
)]
async fn cancel_booking(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(bid): Path<String>,
) -> Result<Json<BookingResponse>, AppError> {
    let booking = MealBooking::read(&MealBookingId::from_key(&bid), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // Manager+ may give back *any* seat. Not a convenience: `booking_target`
    // grants only students and parents, so a student promoted to staff (or a
    // parent unlinked) left a live seat nobody on the API could cancel — the
    // menu 409s its own delete forever and the charge can never be reversed,
    // since cancelling is the only route that reverses one. Promotion itself
    // deliberately sweeps nothing: moving money is a decision, not a side
    // effect of a role change.
    let staff = user.get_role().at_least(Role::Manager);
    if !staff {
        // Same door as booking: whoever may take the seat may give it back.
        let target = booking_target(&user, Some(booking.get_student().key()), &st.db).await?;
        if &target != booking.get_student() {
            return Err(AppError::Forbidden("not your booking"));
        }
    }
    // …and the cutoff does not bind them either. It exists to stop students
    // gaming the kitchen's headcount, which is no reason to leave staff holding
    // a seat they cannot free: past the cutoff the seat was uncancellable, and
    // its menu — which refuses its own delete while a seat is held — was
    // undeletable with it, forever. A deadline that binds nobody is exactly the
    // one a school that set no `meal_cancel_cutoff_minutes` already runs under.
    let cutoff = if staff {
        MealCutoff::default()
    } else {
        meal_cutoff(&st.db).await?
    };
    // Flips the row and appends the reversal for that attempt's charge in one
    // transaction; the charge itself stays.
    let cancelled = booking.cancel(&cutoff, user.get_id(), &st.db).await?;
    let items = booking_responses(std::slice::from_ref(&cancelled), &st.db).await?;
    Ok(Json(
        items.into_iter().next().expect("one booking in, one out"),
    ))
}

// ---- attendance -----------------------------------------------------------
//
// Reporting only: nothing in this section touches [`MealLedger`]. Booking is
// what charges, so a no-show still pays — the kitchen bought the food.

#[derive(Deserialize, ToSchema)]
struct MarkMealAttendance {
    /// Who was (or was not) served — usually a student, but the canteen also
    /// feeds staff, so any existing user may be marked. The mark moves no money.
    student_id: String,
    /// `served` or `missed` — the canteen's own fixed pair, not the school's
    /// editable roll-call statuses.
    #[schema(example = "served")]
    status: String,
}

#[derive(Serialize, ToSchema)]
struct MealAttendanceResponse {
    id: String,
    /// The menu the mark is against.
    menu_id: String,
    student: PersonRef,
    #[schema(example = "served")]
    status: String,
    marked_by: PersonRef,
    marked_at: i64,
}

impl MealAttendanceResponse {
    fn new(row: &MealAttendance, people: &std::collections::HashMap<String, PersonRef>) -> Self {
        Self {
            id: row.get_id().key().to_string(),
            menu_id: row.get_menu().key().to_string(),
            student: PersonRef::resolve(people, row.get_student()),
            status: row.get_status().as_str().to_string(),
            marked_by: PersonRef::resolve(people, row.get_marked_by()),
            marked_at: row.get_marked_at().as_millis(),
        }
    }
}

/// Join the people onto a page of marks — one query for the whole page.
async fn attendance_responses(
    rows: &[MealAttendance],
    db: &Database,
) -> Result<Vec<MealAttendanceResponse>, AppError> {
    let people = person_map(
        rows.iter()
            .flat_map(|row| [row.get_student().clone(), row.get_marked_by().clone()]),
        db,
    )
    .await?;
    Ok(rows
        .iter()
        .map(|row| MealAttendanceResponse::new(row, &people))
        .collect())
}

/// Record whether someone ate off a menu. Requires teacher+ — whoever stands
/// at the canteen door marks, students never mark themselves. One row per
/// (menu, person), so re-marking corrects the row rather than adding another.
///
/// The target need only *exist*: a canteen also serves staff, and a walk-in
/// with no booking is still someone the kitchen fed. Marking is a record of
/// what happened, not a check of who was entitled — and since it moves no
/// money, a non-student mark costs nobody anything.
///
/// **This moves no money.** A booked student who did not show is still charged
/// (the food was cooked), and a walk-in marked `served` without a booking is
/// not charged either — booking is the only charge trigger.
#[utoipa::path(
    post,
    path = "/menus/{id}/attendance",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Menu id")),
    request_body = MarkMealAttendance,
    responses(
        (status = 200, description = "Attendance recorded", body = MealAttendanceResponse),
        (status = 400, description = "Invalid status, or no such user", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "No such menu", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn mark_attendance(
    State(st): State<AppState>,
    RequireTeacher(marker): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<MarkMealAttendance>,
) -> Result<Json<MealAttendanceResponse>, AppError> {
    let menu = Menu::read(&MenuId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let status = MealAttendanceStatus::try_new(&req.status)?;
    let student = UserId::from_key(&req.student_id);
    // The target must exist; no booking is required, since a walk-in was still
    // served and the record is operationally true.
    if User::read(&student, &st.db).await?.is_none() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "student_id",
            reason: "target user does not exist",
        }));
    }
    // The menu rides in the write's own target (see [`MealAttendance::mark`]),
    // so a delete landing between the read above and this write leaves no mark
    // behind — it answers 404 instead, exactly as the read would have.
    let row = MealAttendance::mark(menu.get_id(), &student, status, marker.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let items = attendance_responses(std::slice::from_ref(&row), &st.db).await?;
    Ok(Json(
        items.into_iter().next().expect("one mark in, one out"),
    ))
}

/// Who ate off one menu. Requires teacher+. Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/menus/{id}/attendance",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Menu id"), PageParams),
    responses(
        (status = 200, description = "A page of marks (all of them when unpaged)", body = Page<MealAttendanceResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "No such menu", body = ErrorResponse),
    ),
)]
async fn list_menu_attendance(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<MealAttendanceResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Menu must exist — a missing menu is a 404, not an empty list.
    let menu = Menu::read(&MenuId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let (rows, total) = MealAttendance::list_for_menu(menu.get_id(), limit, offset, &st.db).await?;
    let items = attendance_responses(&rows, &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// One student's meal-attendance history, newest mark first. Requires teacher+,
/// or a parent linked to them — the caller's own id always passes, like the
/// balance and ledger reads. Narrow to a date range with `?from=&to=`
/// (inclusive `YYYY-MM-DD` bounds on the menu's day). Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/attendance/{user}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "Student id"), MenuRange, PageParams),
    responses(
        (status = 200, description = "A page of marks (all of them when unpaged)", body = Page<MealAttendanceResponse>),
        (status = 400, description = "Malformed date bound, limit, or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link", body = ErrorResponse),
    ),
)]
async fn user_attendance(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
    Query(range): Query<MenuRange>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<MealAttendanceResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let target = UserId::from_key(&user);
    ensure_can_read_student(&caller, &target, &st.db).await?;
    let from = range.from.as_deref().map(MenuDate::try_new).transpose()?;
    let to = range.to.as_deref().map(MenuDate::try_new).transpose()?;
    let (rows, total) = MealAttendance::list_for_student(
        &target,
        from.as_ref(),
        to.as_ref(),
        limit,
        offset,
        &st.db,
    )
    .await?;
    let items = attendance_responses(&rows, &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

// ---- money ----------------------------------------------------------------

#[derive(Serialize, ToSchema)]
struct BalanceResponse {
    student: PersonRef,
    /// `credits + reversals - charges`, in **minor units** (kuruş). Negative
    /// means the student owes the school. Derived on every read, never stored.
    #[schema(example = -4500)]
    balance_minor: i64,
}

#[derive(Serialize, ToSchema)]
struct LedgerResponse {
    id: String,
    student: PersonRef,
    /// `charge`, `credit`, or `reversal`.
    #[schema(example = "charge")]
    kind: String,
    /// Always positive — the sign is the `kind`'s business.
    #[schema(example = 4500)]
    amount_minor: i64,
    /// What caused the line: a booking id on a `charge`, the reversed charge's
    /// own id on a `reversal`, absent on a `credit`.
    source: Option<String>,
    method: Option<String>,
    note: Option<String>,
    /// Who wrote it: the booker on a charge or reversal, the admin on a credit.
    recorded_by: PersonRef,
    created_at: i64,
}

impl LedgerResponse {
    fn new(line: &MealLedger, people: &std::collections::HashMap<String, PersonRef>) -> Self {
        Self {
            id: line.get_id().key().to_string(),
            student: PersonRef::resolve(people, line.get_student()),
            kind: line.get_kind().as_str().to_string(),
            amount_minor: line.get_amount_minor().as_minor(),
            source: line.get_source_key().map(str::to_string),
            method: line.get_method().map(|m| m.as_str().to_string()),
            note: line.get_note().map(|n| n.as_str().to_string()),
            recorded_by: PersonRef::resolve(people, line.get_recorded_by()),
            created_at: line.get_created_at().as_millis(),
        }
    }
}

/// Join the people onto a page of ledger lines — one query for the whole page.
async fn ledger_responses(
    lines: &[MealLedger],
    db: &Database,
) -> Result<Vec<LedgerResponse>, AppError> {
    let people = person_map(
        lines
            .iter()
            .flat_map(|line| [line.get_student().clone(), line.get_recorded_by().clone()]),
        db,
    )
    .await?;
    Ok(lines
        .iter()
        .map(|line| LedgerResponse::new(line, &people))
        .collect())
}

/// One student's balance, resolved to a response. Shared by both balance routes.
async fn balance_response(
    student: &UserId,
    db: &Database,
) -> Result<Json<BalanceResponse>, AppError> {
    let people = person_map(std::iter::once(student.clone()), db).await?;
    Ok(Json(BalanceResponse {
        student: PersonRef::resolve(&people, student),
        balance_minor: MealLedger::balance_of(student, db).await?,
    }))
}

/// Reading someone else's meal record — money, attendance, dietary profile:
/// teacher+, or a parent linked to the student. Reading your *own* is always
/// allowed, since every `{user}` route here accepts the caller's own id; that
/// is the one way this gate differs from [`super::ensure_can_observe`], which
/// 403s a self-read because its subjects have their own `/me` routes.
async fn ensure_can_read_student(
    caller: &User,
    target: &UserId,
    db: &Database,
) -> Result<(), AppError> {
    if caller.get_id() == target {
        return Ok(());
    }
    super::ensure_can_observe(caller, target, db).await
}

/// May `caller` read `target`'s meal *money* — balance and ledger? Own always;
/// a parent only for a student they hold a live link to; manager+ for anyone.
///
/// Deliberately narrower than [`ensure_can_read_student`], which still gates
/// the profile, booking and attendance reads at teacher+: **a teacher sees no
/// money**. Canteen debt is family debt, so this is the same rule (and the same
/// 403) `/payments` has always held — what a family owes the school is not
/// classroom information, and neither is what it owes the canteen.
async fn ensure_can_read_money(
    caller: &User,
    target: &UserId,
    db: &Database,
) -> Result<(), AppError> {
    if caller.get_id() == target || caller.get_role().at_least(Role::Manager) {
        return Ok(());
    }
    // The link row alone is not the grant: a link whose student side changed
    // role must be inert, so the target's live role is re-read. A missing or
    // non-student target falls through to the same 403 — a parent never gets an
    // existence oracle.
    if caller.get_role() == Role::Parent
        && ParentLink::exists(caller.get_id(), target, db).await?
        && User::read(target, db)
            .await?
            .is_some_and(|target| target.get_role() == Role::Student)
    {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "requires manager role or higher, or a parent link to this student",
    ))
}

/// What the caller owes or has on account.
#[utoipa::path(
    get,
    path = "/balance/me",
    tag = "meals",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's meal balance", body = BalanceResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_balance(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<BalanceResponse>, AppError> {
    balance_response(user.get_id(), &st.db).await
}

/// One student's meal balance. Requires manager+, or a parent link to them —
/// the caller's own id always passes. A teacher gets a `403`: canteen debt is
/// family debt, gated exactly like `/payments`.
#[utoipa::path(
    get,
    path = "/balance/{user}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "Student id")),
    responses(
        (status = 200, description = "The student's meal balance", body = BalanceResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher, or a parent link", body = ErrorResponse),
    ),
)]
async fn user_balance(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
) -> Result<Json<BalanceResponse>, AppError> {
    let target = UserId::from_key(&user);
    ensure_can_read_money(&caller, &target, &st.db).await?;
    balance_response(&target, &st.db).await
}

/// One student's statement, newest line first: every charge, credit, and
/// reversal. Nothing here is ever edited or deleted — a correction is another
/// line. Same gate as the balance. Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/ledger/{user}",
    tag = "meals",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "Student id"), PageParams),
    responses(
        (status = 200, description = "A page of ledger lines (all of them when unpaged)", body = Page<LedgerResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher, or a parent link", body = ErrorResponse),
    ),
)]
async fn user_ledger(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<LedgerResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let target = UserId::from_key(&user);
    ensure_can_read_money(&caller, &target, &st.db).await?;
    let (rows, total) = MealLedger::list_for_student(&target, limit, offset, &st.db).await?;
    let items = ledger_responses(&rows, &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[derive(Deserialize, ToSchema)]
struct RecordCredit {
    /// The student the money is for.
    student_id: String,
    /// How much came in, **minor units** (kuruş), positive.
    #[schema(minimum = 1, maximum = 10000000, example = 25000)]
    amount_minor: i64,
    /// How it arrived ("cash", "havale", …). Free text — the backend speaks to
    /// no payment gateway and stores no card data.
    #[schema(max_length = 50, example = "cash")]
    method: Option<String>,
    #[schema(max_length = 500, example = "receipt 2026-114")]
    note: Option<String>,
}

/// Record money received from a student. **Admin only** — not manager: writing
/// down cash is the highest-trust action in the app. The target must be a
/// student, or anyone who already carries meal-ledger lines — a debt survives
/// its debtor's role change, and it has to stay settleable. Appends a `credit` line;
/// nothing in the ledger is ever edited or removed, so an over-credit is
/// corrected by a compensating line, not by a fix-up.
#[utoipa::path(
    post,
    path = "/credits",
    tag = "meals",
    security(("session_cookie" = [])),
    request_body = RecordCredit,
    responses(
        (status = 201, description = "Credit recorded", body = LedgerResponse),
        (status = 400, description = "Invalid amount, method, note, or a non-student target", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires the admin role", body = ErrorResponse),
        (status = 404, description = "No such user", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn record_credit(
    State(st): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    Json(req): Json<RecordCredit>,
) -> Result<(StatusCode, Json<LedgerResponse>), AppError> {
    let student = UserId::from_key(&req.student_id);
    // Only a student carries a meal balance; crediting anyone else is a typo,
    // and a typo here is money in the wrong ledger. Unless they already carry
    // ledger lines: a debt outlives its debtor's role change (a student
    // promoted to staff keeps what they owed), and the student rule alone made
    // that debt permanently unsettleable — there is no other route that
    // appends a credit. A typo'd staff id has no lines, so it still 400s.
    if User::read(&student, &st.db)
        .await?
        .ok_or(AppError::NotFound)?
        .get_role()
        != Role::Student
        && MealLedger::list_for_student(&student, Some(1), 0, &st.db)
            .await?
            .0
            .is_empty()
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "student_id",
            reason: "only a student carries a meal balance",
        }));
    }
    let line = MealLedger::credit(
        &student,
        LedgerAmount::try_new(req.amount_minor)?,
        req.method
            .as_deref()
            .map(LedgerMethod::try_new)
            .transpose()?
            .flatten(),
        req.note
            .as_deref()
            .map(LedgerNote::try_new)
            .transpose()?
            .flatten(),
        admin.get_id(),
        &st.db,
    )
    .await?;
    let items = ledger_responses(std::slice::from_ref(&line), &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(items.into_iter().next().expect("one line in, one out")),
    ))
}
