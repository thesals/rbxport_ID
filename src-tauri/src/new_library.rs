//! Starting with no rbxport library yet: what the window asks at startup,
//! and making the library, in rbxport's own folder, when it is told to.

use std::sync::Arc;

use tauri::{Manager, State};

use crate::commands::blocking;
use crate::dto::LibraryProblemDto;
use crate::error::{AppError, AppResult, ErrorKind};
use crate::state::AppState;

/// Why the library did not load, or nothing while it is loading or loaded.
#[tauri::command]
#[allow(clippy::needless_pass_by_value, reason = "Tauri's State extractor is injected by value")]
pub fn library_problem(state: State<'_, Arc<AppState>>) -> Option<LibraryProblemDto> {
    state.library_problem()
}

/// Makes a new, empty library in rbxport's folder, then loads it as startup
/// would. `library:ready` follows when it is up.
///
/// Planned again here rather than trusted from startup: a library that has
/// appeared since is loaded, never replaced.
#[tauri::command]
pub async fn create_library(app: tauri::AppHandle) -> AppResult<()> {
    let handle = app.clone();
    blocking("create_library", move || {
        let plan = rbl_db::new_library::plan()
            .map_err(|e| AppError::new(ErrorKind::NotFound, "Could not find where the library should go.").with_detail(e.to_string()))?;
        if let Some(plan) = plan {
            let made = rbl_db::new_library::create(&plan).map_err(|e| {
                AppError::new(ErrorKind::Internal, format!("Could not make the library: {e}")).with_detail(e.to_string())
            })?;
            tracing::info!(path = %made.master_db.display(), "made a new library");
        }
        handle.state::<Arc<AppState>>().set_library_problem(None);
        crate::spawn_library_load(handle);
        Ok(())
    })
    .await
}
