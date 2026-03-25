use crate::AppState;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::{Extension, Json};
use log::{error, info};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct AdminEntry {
    pub value: String,
}

#[derive(Serialize)]
pub struct AdminListResponse {
    pub entries: Vec<String>,
}

/// Maps a URL path segment to a (table_name, column_name) pair.
/// Returns None for unrecognized list names, preventing SQL injection.
fn table_and_column(list: &str) -> Option<(&'static str, &'static str)> {
    match list {
        "banned_domains" => Some(("banned_domains", "domain")),
        "banned_users" => Some(("banned_users", "email")),
        "whitelisted_users" => Some(("whitelisted_users", "email")),
        "premium_users" => Some(("premium_users", "email")),
        _ => None,
    }
}

#[axum::debug_handler]
pub async fn admin_list(
    Extension(state): Extension<AppState>,
    Path(list): Path<String>,
) -> Result<Json<AdminListResponse>, StatusCode> {
    let (table, column) = table_and_column(&list).ok_or(StatusCode::NOT_FOUND)?;
    let query = format!("SELECT {} FROM {} ORDER BY {}", column, table, column);
    let rows: Vec<(String,)> = sqlx::query_as(&query)
        .fetch_all(&state.users_db)
        .await
        .map_err(|e| {
            error!("Admin DB error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(AdminListResponse {
        entries: rows.into_iter().map(|r| r.0).collect(),
    }))
}

#[axum::debug_handler]
pub async fn admin_add(
    Extension(state): Extension<AppState>,
    Path(list): Path<String>,
    Json(payload): Json<AdminEntry>,
) -> Result<StatusCode, StatusCode> {
    let (table, column) = table_and_column(&list).ok_or(StatusCode::NOT_FOUND)?;
    let value = payload.value.trim().to_string();
    if value.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let query = format!("INSERT OR IGNORE INTO {} ({}) VALUES (?)", table, column);
    sqlx::query(&query)
        .bind(&value)
        .execute(&state.users_db)
        .await
        .map_err(|e| {
            error!("Admin DB error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    info!("Admin: added '{}' to {}", value, table);
    Ok(StatusCode::CREATED)
}

#[axum::debug_handler]
pub async fn admin_remove(
    Extension(state): Extension<AppState>,
    Path(list): Path<String>,
    Json(payload): Json<AdminEntry>,
) -> Result<StatusCode, StatusCode> {
    let (table, column) = table_and_column(&list).ok_or(StatusCode::NOT_FOUND)?;
    let query = format!("DELETE FROM {} WHERE {} = ?", table, column);
    let result = sqlx::query(&query)
        .bind(&payload.value)
        .execute(&state.users_db)
        .await
        .map_err(|e| {
            error!("Admin DB error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if result.rows_affected() == 0 {
        Err(StatusCode::NOT_FOUND)
    } else {
        info!("Admin: removed '{}' from {}", payload.value, table);
        Ok(StatusCode::OK)
    }
}
