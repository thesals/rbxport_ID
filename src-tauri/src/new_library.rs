//! Starting with no rbxport library yet: what the window asks at startup,
//! and making the library, in rbxport's own folder, when it is told to:
//! empty, or imported from rekordbox's (which is only read).

use std::sync::Arc;

use tauri::{Manager, State};

use crate::commands::blocking;
use crate::dto::{ImportProgressDto, ImportSummaryDto, LibraryProblemDto};
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

/// Imports rekordbox's library as rbxport's, then loads it as startup would.
/// Progress is `library-import:progress`; `library:ready` follows when the
/// library is up. rekordbox's files are only read
/// ([`rbl_db::rekordbox_import`]).
#[tauri::command]
pub async fn import_rekordbox(app: tauri::AppHandle) -> AppResult<ImportSummaryDto> {
    let handle = app.clone();
    blocking("import_rekordbox", move || {
        let source = rbl_db::rekordbox_import::find_rekordbox()
            .ok_or_else(|| AppError::new(ErrorKind::NotFound, "rekordbox's library was not found on this computer."))?;
        let target = rbl_db::own_library_dir()
            .map_err(|e| AppError::new(ErrorKind::NotFound, "Could not find where the library should go.").with_detail(e.to_string()))?;
        let emitter = handle.clone();
        let summary = rbl_db::rekordbox_import::first_import(&source, &target, &mut |p| {
            let _ = tauri::Emitter::emit(
                &emitter,
                "library-import:progress",
                ImportProgressDto { stage: p.stage, done: p.done, total: p.total },
            );
        })
        .map_err(|e| AppError::new(ErrorKind::Internal, e.to_string()).with_detail(e.to_string()))?;
        tracing::info!(
            tracks = summary.tracks,
            files = summary.files_copied,
            problems = summary.problems.len(),
            source_unchanged = summary.source_unchanged,
            "imported rekordbox's library"
        );
        handle.state::<Arc<AppState>>().set_library_problem(None);
        crate::spawn_library_load(handle);
        Ok(ImportSummaryDto {
            tracks: summary.tracks,
            playlists: summary.playlists,
            cues: summary.cues,
            history_sessions: summary.history_sessions,
            files_copied: summary.files_copied,
            bytes_copied: summary.bytes_copied,
            problems: summary.problems,
            source_unchanged: summary.source_unchanged,
            set_aside: summary.set_aside.map(|p| p.display().to_string()),
            log: summary.log.display().to_string(),
        })
    })
    .await
}
