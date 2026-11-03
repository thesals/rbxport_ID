//! Track identification: the background job, its progress and the review
//! queue's commands. Fork-only; the rules are `docs/identify/IDENTIFY_SPEC.md`.
//!
//! The job runs on a thread of its own, one track at a time: resolve the
//! file, identify it with `rbl-identify`, save the result (and any fetched
//! cover) to the identify store, and emit one `identify:result` event; a
//! `identify:progress` event goes out as each track starts and once at the
//! end. A track is saved the moment it finishes, so a cancel or a crash
//! keeps every finished track.
//!
//! Identification is ONE-WAY: it reads the rekordbox library (the track's
//! path, and a snapshot of its record imported into the identify store) but
//! never writes to it, never takes a library writer and never needs
//! rekordbox closed. Review commands change review state in the identify
//! store only. Tags are written only by `identify_apply`, on the user's
//! explicit request, and only to the audio FILE (`rbl_identify::write`,
//! snapshotted in the store before it is touched), so `identify_undo` can
//! put the file back. Nothing here goes near the USB export path.
//!
//! The store is this feature's own SQLite file under the app's data folder
//! (`<data>/rbxport/identify/identify.db`): a local disk, never the library
//! folder, never a USB volume. Keys come from `.env.identify` beside it (or
//! the environment) and are never logged or returned; the settings commands
//! ([`settings`]) say only where each comes from.

mod settings;

pub use settings::{GenrePolicyDto, IdentifySettingsDto, IdentifySettingsUpdateDto, KeySource, SecretUpdate};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use rbl_identify::acoustid::AcoustIdClient;
use rbl_identify::clock::{Clock, SystemClock};
use rbl_identify::coverart::CoverArtClient;
use rbl_identify::discogs::DiscogsClient;
use rbl_identify::model::{Decision, Field, FieldChange, Proposal, Scored};
use rbl_identify::musicbrainz::MusicBrainzClient;
use rbl_identify::pipeline::{self, Services, Settings, TrackResult};
use rbl_identify::ratelimit::Limiters;
use rbl_identify::store::{
    Filter, LibraryTrack, QueueItem, Reconciliation, RekordboxRecord, ResultId, ReviewState, Store, TrackKey,
};
use rbl_identify::transport::{HttpTransport, Transport};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::commands::{blocking, MAX_ROWS};
use crate::error::{AppError, AppResult, ErrorKind};
use crate::state::AppState;

/// Under the platform data folder, beside rbxport's logs and settings.
const DIR: &str = "rbxport/identify";
const STORE_FILE: &str = "identify.db";
const ENV_FILE: &str = ".env.identify";

pub const PROGRESS_EVENT: &str = "identify:progress";
pub const RESULT_EVENT: &str = "identify:result";
/// Results changed by apply, undo or bulk accept: `{ resultIds }`.
pub const CHANGED_EVENT: &str = "identify:changed";

/// The folder the store and `.env.identify` live in.
pub fn default_root() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join(DIR)
}

/// Identifies one file. The live [`Engine`] in the app, a fake in tests.
pub(crate) trait Identifier: Send + Sync {
    fn identify(&self, path: &Path) -> rbl_identify::Result<TrackResult>;
}

/// The live services, built from the keys and settings on first use and
/// kept until the settings are saved (a running job keeps its own). The
/// clients hold reqwest blocking clients, so an engine is
/// only ever built or dropped on a plain thread, never on the async
/// runtime (see [`off_runtime`] and `Drop for IdentifyState`).
pub(crate) struct Engine {
    acoustid: AcoustIdClient,
    musicbrainz: MusicBrainzClient,
    discogs: Option<DiscogsClient>,
    coverart: Option<CoverArtClient>,
    settings: Settings,
}

impl Engine {
    fn build(
        config: &rbl_identify::config::Config,
        settings: Settings,
        clock: &Arc<dyn Clock>,
        limiters: &Limiters,
    ) -> rbl_identify::Result<Self> {
        let transport: Arc<dyn Transport> = Arc::new(HttpTransport::new()?);
        let acoustid = AcoustIdClient::new(
            Arc::clone(&transport),
            Arc::clone(clock),
            Arc::clone(&limiters.acoustid),
            config.acoustid_key.clone(),
        );
        let musicbrainz = MusicBrainzClient::new(
            Arc::clone(&transport),
            Arc::clone(clock),
            Arc::clone(&limiters.musicbrainz),
            config.mb_user_agent.clone(),
        );
        // Discogs only with the user's personal token: without one,
        // enrichment is skipped rather than failing every track.
        let discogs = config.discogs_token.clone().map(|token| {
            DiscogsClient::new(
                Arc::clone(&transport),
                Arc::clone(clock),
                Arc::clone(&limiters.discogs),
                token,
                config.mb_user_agent.clone(),
            )
        });
        // Artwork is optional: a cover client that cannot be built means
        // no covers, not no identification.
        let coverart =
            match CoverArtClient::live(Arc::clone(clock), Arc::clone(&limiters.coverart), config.mb_user_agent.clone()) {
                Ok(client) => Some(client),
                Err(e) => {
                    tracing::warn!(error = %e, "identify: Cover Art Archive unavailable");
                    None
                }
            };
        tracing::info!(
            discogs = discogs.is_some(),
            coverart = coverart.is_some(),
            auto_apply = settings.auto_apply,
            "identify: services ready"
        );
        Ok(Self { acoustid, musicbrainz, discogs, coverart, settings })
    }
}

impl Identifier for Engine {
    fn identify(&self, path: &Path) -> rbl_identify::Result<TrackResult> {
        let services = Services {
            acoustid: &self.acoustid,
            musicbrainz: &self.musicbrainz,
            discogs: self.discogs.as_ref(),
            coverart: self.coverart.as_ref(),
        };
        pipeline::identify(path, &services, self.settings)
    }
}

/// Runs `f` on a plain thread and waits for it: the thread has no async
/// runtime context, which reqwest's blocking client needs to be built or
/// dropped safely.
fn off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> AppResult<T> {
    std::thread::scope(|scope| scope.spawn(f).join())
        .map_err(|_| AppError::internal("the identify services thread panicked"))
}

/// The job running now: its id and the flag that stops it.
struct RunningJob {
    id: u32,
    cancel: Arc<AtomicBool>,
}

/// Identification's state for the app: one per process, managed by Tauri.
pub struct IdentifyState {
    root: PathBuf,
    /// Where the keys and settings are read from.
    sources: settings::Sources,
    /// Serialises settings saves.
    settings_lock: Mutex<()>,
    clock: Arc<dyn Clock>,
    /// One per process: the services count requests per client IP.
    limiters: Limiters,
    engine: Mutex<Option<Arc<Engine>>>,
    /// Opened on first use. A rusqlite connection is not `Sync`; every use
    /// holds this lock, and none holds it across network work.
    store: Mutex<Option<Store>>,
    job: Mutex<Option<RunningJob>>,
    /// The running or last job's progress, for a view that mounts mid-job.
    progress: Mutex<Option<IdentifyProgressDto>>,
    next_job: AtomicU32,
}

impl Default for IdentifyState {
    fn default() -> Self {
        Self::with_root(default_root())
    }
}

impl Drop for IdentifyState {
    fn drop(&mut self) {
        // reqwest's blocking client must not be dropped on the async runtime.
        if let Some(engine) = self.engine.get_mut().take() {
            let _ = off_runtime(move || drop(engine));
        }
    }
}

impl IdentifyState {
    /// State whose store and `.env.identify` live in `root`, keys read from
    /// the environment and (in a development build) the checkout's file.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let dev_files = dev_files();
        Self::with_sources(root, dev_files, Arc::new(|key: &str| std::env::var(key).ok()))
    }

    /// State reading keys from exactly these places: for tests, which must
    /// never see the real environment or a developer's keys.
    pub(crate) fn with_sources(root: PathBuf, dev_files: Vec<PathBuf>, env: settings::EnvLookup) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        Self {
            sources: settings::Sources { root: root.clone(), dev_files, env },
            settings_lock: Mutex::new(()),
            root,
            limiters: Limiters::new(Arc::clone(&clock)),
            clock,
            engine: Mutex::new(None),
            store: Mutex::new(None),
            job: Mutex::new(None),
            progress: Mutex::new(None),
            next_job: AtomicU32::new(1),
        }
    }

    fn store_path(&self) -> PathBuf {
        self.root.join(STORE_FILE)
    }

    /// Runs `f` with the store, opening it first if need be. Blocking: call
    /// from a worker thread.
    fn with_store<T>(&self, f: impl FnOnce(&Store) -> AppResult<T>) -> AppResult<T> {
        let mut slot = self.store.lock();
        if slot.is_none() {
            std::fs::create_dir_all(&self.root).map_err(|e| {
                AppError::new(ErrorKind::NotFound, "The identify data folder could not be created.")
                    .with_detail(format!("{}: {e}", self.root.display()))
            })?;
            *slot = Some(Store::open(&self.store_path()).map_err(store_error)?);
        }
        match slot.as_ref() {
            Some(store) => f(store),
            None => Err(AppError::internal("the identify store is not open")),
        }
    }

    /// On startup: writes a crash left open are counted. Opens the store
    /// only if it exists, so a user who never identifies gets no file.
    pub fn reconcile_on_startup(&self) {
        if !self.store_path().is_file() {
            return;
        }
        // A write whose file still holds its before-snapshot never happened
        // and is abandoned; one whose file holds exactly the written tags is
        // recorded as written; one that cannot be told is closed as unknown
        // and its result returned to review. A file that is not there, or
        // cannot be read now, stays open for the next start. No file is
        // written here.
        match self.with_store(|store| store.reconcile_open_applications_from_files().map_err(store_error)) {
            Ok(outcomes) if outcomes.is_empty() => {}
            Ok(outcomes) => {
                let count = |f: fn(&Reconciliation) -> bool| outcomes.iter().filter(|o| f(o)).count();
                tracing::warn!(
                    abandoned = count(|o| matches!(o, Reconciliation::Abandoned { .. })),
                    completed = count(|o| matches!(o, Reconciliation::Completed { .. })),
                    unknown = count(|o| matches!(o, Reconciliation::Unknown { .. })),
                    open = count(|o| matches!(o, Reconciliation::NeedsUser { .. })),
                    "identify: tag writes left open by an earlier run"
                );
            }
            Err(e) => tracing::warn!(error = %e, "identify: the store could not be checked on startup"),
        }
    }

    /// The live services, built on first use from the keys and settings
    /// as they are then, and kept until the settings are saved. Blocking:
    /// call from a worker thread.
    fn engine(&self) -> AppResult<Arc<Engine>> {
        let mut slot = self.engine.lock();
        if let Some(engine) = slot.as_ref() {
            return Ok(Arc::clone(engine));
        }
        let config = self.sources.config()?;
        let settings = self.sources.stored().pipeline();
        let (clock, limiters) = (&self.clock, &self.limiters);
        let engine = off_runtime(|| Engine::build(&config, settings, clock, limiters))?.map_err(|e| {
            AppError::new(ErrorKind::Internal, "The identification services could not be started.")
                .with_detail(e.to_string())
        })?;
        let engine = Arc::new(engine);
        *slot = Some(Arc::clone(&engine));
        Ok(engine)
    }

    /// The settings as they are now. Blocking: call from a worker thread.
    pub(crate) fn settings(&self) -> IdentifySettingsDto {
        self.sources.dto()
    }

    /// Saves the settings, then drops the cached services so the next job
    /// is built from them. A running job keeps the services it started
    /// with. Blocking: call from a worker thread.
    pub(crate) fn save_settings(&self, update: &IdentifySettingsUpdateDto) -> AppResult<IdentifySettingsDto> {
        let _saving = self.settings_lock.lock();
        // The engine slot stays locked across the writes: a job starting
        // meanwhile waits in `engine()` and then builds from the new files,
        // never caching one from the old ones. The cached engine is dropped
        // before the first file is written, so even a save that fails
        // halfway leaves none built from the old settings. (Lock order:
        // settings, then engine; `engine()` takes only the engine lock.)
        let mut old = None;
        let saved = {
            let mut slot = self.engine.lock();
            self.sources.save(update, || old = slot.take())
        };
        // reqwest's blocking client must not be dropped on the async
        // runtime; a running job's own Arc keeps it alive meanwhile.
        if let Some(old) = old {
            off_runtime(move || drop(old))?;
        }
        saved?;
        Ok(self.sources.dto())
    }

    fn is_running(&self) -> bool {
        self.job.lock().is_some()
    }

    /// Claims the job slot, or refuses while a job runs.
    fn begin_job(&self) -> AppResult<(u32, Arc<AtomicBool>)> {
        let mut job = self.job.lock();
        if job.is_some() {
            return Err(already_running());
        }
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        *job = Some(RunningJob { id, cancel: Arc::clone(&cancel) });
        Ok((id, cancel))
    }

    /// Frees the job slot, if `id` still holds it.
    fn end_job(&self, id: u32) {
        let mut job = self.job.lock();
        if job.as_ref().is_some_and(|j| j.id == id) {
            *job = None;
        }
    }

    /// Asks job `id` to stop after the track it is on.
    pub fn cancel(&self, id: u32) {
        if let Some(job) = self.job.lock().as_ref().filter(|j| j.id == id) {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn progress(&self) -> Option<IdentifyProgressDto> {
        self.progress.lock().clone()
    }

    /// Records progress; a final one frees the job slot first, so a view
    /// that starts another job on hearing `done` is not refused.
    fn record(&self, progress: &IdentifyProgressDto) {
        if progress.state != JobState::Running {
            self.end_job(progress.job_id);
        }
        *self.progress.lock() = Some(progress.clone());
    }

    fn queue(&self, filter: QueueFilter, offset: u32, limit: u32) -> AppResult<IdentifyQueueDto> {
        let filter = filter.into();
        let limit = limit.min(MAX_ROWS);
        self.with_store(|store| {
            let total = store.count(filter).map_err(store_error)?;
            let items = store.list(filter, offset, limit).map_err(store_error)?;
            let items = items.iter().map(|item| item_dto(store, item)).collect::<AppResult<Vec<_>>>()?;
            Ok(IdentifyQueueDto { total, items })
        })
    }

    fn detail(&self, result: ResultId) -> AppResult<IdentifyDetailDto> {
        self.with_store(|store| {
            let (item, candidates, enrichment, warnings) =
                store.result(result).map_err(store_error)?.ok_or_else(no_such_result)?;
            let write_in_progress = write_open(store, &item.path)?;
            let rekordbox = store.rekordbox_record(result).map_err(store_error)?.map(|r| rekordbox_dto(&r));
            Ok(IdentifyDetailDto {
                rekordbox,
                existing: ExistingTagsDto::from(&item.existing),
                artwork: item.proposal.as_ref().and_then(|p| p.artwork.as_ref()).map(|a| ArtworkDto {
                    source: camel(&a.source),
                    url: a.url.clone(),
                    sha256: a.sha256.clone(),
                    mime: a.mime.clone(),
                    bytes: a.bytes,
                }),
                item: item_dto(store, &item)?,
                candidates: candidates.iter().map(candidate_dto).collect(),
                enrichment: enrichment.map(|e| EnrichmentDto {
                    discogs_release_id: e.discogs_release_id,
                    release_title: e.release_title,
                    label: e.label,
                    catalog_number: e.catalog_number,
                    year: e.year,
                    styles: e.styles,
                    genres: e.genres,
                }),
                warnings,
                write_in_progress,
            })
        })
    }

    fn set_review(&self, result: ResultId, action: ReviewAction) -> AppResult<IdentifyQueueItemDto> {
        let to = match action {
            ReviewAction::Accept => ReviewState::Accepted,
            ReviewAction::Reject => ReviewState::Rejected,
            ReviewAction::Pending => ReviewState::Pending,
        };
        self.with_store(|store| {
            let item = changeable(store, result)?;
            if !item.state.may_become(to) {
                return Err(AppError::new(
                    ErrorKind::Malformed,
                    format!("A {} result cannot become {}.", camel(&item.state), camel(&to)),
                ));
            }
            store.set_state(result, to).map_err(refused)?;
            current_item(store, result)
        })
    }

    fn edit(&self, result: ResultId, edits: &[FieldEditDto]) -> AppResult<IdentifyQueueItemDto> {
        let edits = edits
            .iter()
            .map(|e| {
                parse_field(&e.field)
                    .map(|field| (field, e.proposed.clone()))
                    .ok_or_else(|| AppError::new(ErrorKind::Malformed, format!("There is no tag field \"{}\".", e.field)))
            })
            .collect::<AppResult<Vec<_>>>()?;
        self.with_store(|store| {
            changeable(store, result)?;
            store.edit_proposal(result, &edits).map_err(refused)?;
            current_item(store, result)
        })
    }
}

fn already_running() -> AppError {
    AppError::new(ErrorKind::ReadOnly, "Identification is already running. Wait for it to finish or cancel it.")
}

fn no_such_result() -> AppError {
    AppError::new(ErrorKind::NotFound, "That identification result is no longer in the store.")
}

/// A store failure the user did not cause: the disk, or a bug. Store
/// errors never carry a secret, a path or tag text.
#[allow(clippy::needless_pass_by_value, reason = "used as a map_err adapter")]
fn store_error(e: rbl_identify::IdentifyError) -> AppError {
    AppError::internal(format!("identify store: {e}"))
}

/// The store turned a review change down (a superseded result, a field the
/// proposal lacks). Its message names the reason, never a value.
#[allow(clippy::needless_pass_by_value, reason = "used as a map_err adapter")]
fn refused(e: rbl_identify::IdentifyError) -> AppError {
    AppError::new(ErrorKind::Malformed, "That change could not be made to this result.").with_detail(e.to_string())
}

/// Whether a write to this file is open (begun, not completed or abandoned).
fn write_open(store: &Store, path: &str) -> AppResult<bool> {
    Ok(store.open_applications().map_err(store_error)?.iter().any(|a| a.path == path))
}

/// The result, if it exists and its file has no write open.
fn changeable(store: &Store, result: ResultId) -> AppResult<QueueItem> {
    let (item, ..) = store.result(result).map_err(store_error)?.ok_or_else(no_such_result)?;
    if write_open(store, &item.path)? {
        return Err(AppError::new(
            ErrorKind::ReadOnly,
            "Tags are being written to this file. Its review cannot change until the write is finished.",
        ));
    }
    Ok(item)
}

fn current_item(store: &Store, result: ResultId) -> AppResult<IdentifyQueueItemDto> {
    let (item, ..) = store.result(result).map_err(store_error)?.ok_or_else(no_such_result)?;
    item_dto(store, &item)
}

// ---------------------------------------------------------------- config

/// In a development build only, the `.env.identify` of the working
/// directory and its parent, which is the repository when `tauri dev` runs
/// from `src-tauri`. Each key is looked for there after the environment and
/// the data folder ([`settings::Sources::resolve`]).
fn dev_files() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if cfg!(debug_assertions) {
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(ENV_FILE));
            if let Some(parent) = cwd.parent() {
                candidates.push(parent.join(ENV_FILE));
            }
        }
    }
    candidates
}

// ---------------------------------------------------------------- the job

/// One track of a job, as resolved from the library.
#[derive(Debug, Clone)]
pub(crate) struct JobTrack {
    pub track_id: String,
    pub title: String,
    /// `None` when the library holds no path for it.
    pub path: Option<String>,
    /// The library record, read (never written) when the job started, to
    /// be imported into the identify store with the result. `None` for a
    /// track the library cannot place.
    pub rekordbox: Option<RekordboxRecord>,
}

#[derive(Debug, Clone)]
pub(crate) enum JobEvent {
    Progress(IdentifyProgressDto),
    Result(IdentifyResultDto),
}

/// Runs a job to the end or to a cancel, emitting every event through
/// `emit` in order: per track a `running` progress (naming it) then its
/// result; then one final progress (`done` or `cancelled`). A track that
/// cannot be read or saved is a failed track, never a failed job.
pub(crate) fn run_job(
    state: &IdentifyState,
    job_id: u32,
    tracks: &[JobTrack],
    identifier: &dyn Identifier,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(JobEvent),
) {
    let mut progress = IdentifyProgressDto::new(job_id, u32::try_from(tracks.len()).unwrap_or(u32::MAX));
    for track in tracks {
        // TODO: rbl-identify's pipeline has no cancel hook between its
        // stages, so a cancel takes effect once the current track ends.
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        progress.current_title.clone_from(&track.title);
        progress.current_path = track.path.clone().unwrap_or_default();
        emit(JobEvent::Progress(progress.clone()));

        let result = identify_track(state, identifier, job_id, track);
        match result.decision {
            ResultDecision::AutoApply => progress.identified += 1,
            ResultDecision::Review => progress.review += 1,
            ResultDecision::Unidentified => progress.unidentified += 1,
            ResultDecision::Failed => progress.failed += 1,
        }
        progress.done += 1;
        emit(JobEvent::Result(result));
    }
    progress.state = if cancel.load(Ordering::Relaxed) && progress.done < progress.total {
        JobState::Cancelled
    } else {
        JobState::Done
    };
    progress.current_title.clear();
    progress.current_path.clear();
    emit(JobEvent::Progress(progress));
}

/// Identifies and saves one track; every failure becomes a failed result.
fn identify_track(state: &IdentifyState, identifier: &dyn Identifier, job_id: u32, track: &JobTrack) -> IdentifyResultDto {
    let mut dto = IdentifyResultDto {
        job_id,
        track_id: track.track_id.clone(),
        result_id: None,
        path: track.path.clone().unwrap_or_default(),
        decision: ResultDecision::Failed,
        score: None,
        error: None,
    };
    match identify_and_save(state, identifier, track) {
        Ok((path, result_id, decision, score)) => {
            dto.path = path;
            dto.result_id = Some(result_id);
            dto.decision = decision.into();
            dto.score = score;
        }
        Err(message) => dto.error = Some(message),
    }
    dto
}

fn identify_and_save(
    state: &IdentifyState,
    identifier: &dyn Identifier,
    track: &JobTrack,
) -> Result<(String, ResultId, Decision, Option<f64>), String> {
    let raw = track.path.as_deref().filter(|p| !p.is_empty()).ok_or("The library holds no file for this track.")?;
    if !Path::new(raw).is_file() {
        return Err("The file could not be found, or is not a file.".to_owned());
    }
    // The canonical path, size and time: one store key per file.
    let key = TrackKey::for_file(Path::new(raw)).map_err(|e| format!("The file could not be read: {e}"))?;
    let path_text = key.path.clone();
    let path = PathBuf::from(&path_text);
    let mut result = identifier.identify(&path).map_err(|e| e.to_string())?;
    let score = result.best().map(|b| b.score);
    let mut decision = result.decision;
    // A refusal (a track whose result was applied, or has a write open) is
    // this track's failure, not an internal error to report.
    let result_id = state
        .with_store(|store| {
            let library = LibraryTrack { track_id: track.track_id.clone(), path: raw.to_owned() };
            let id = save(store, &key, &mut result, &library, track.rekordbox.as_ref())
                .map_err(|e| AppError::new(ErrorKind::Malformed, e.to_string()))?;
            // Auto-apply (opted in, and decided only for a fingerprint match
            // at or above the threshold that fills empty fields): written now,
            // or left for review with the reason.
            if decision == Decision::AutoApply {
                if let Err(reason) = auto_apply(store, id) {
                    tracing::warn!(result = id, "identify: an auto-apply was not written; left for review");
                    tracing::debug!(%reason, "identify: auto-apply refused");
                    decision = Decision::Review;
                }
            }
            Ok(id)
        })
        .map_err(|e| e.detail.as_ref().map_or_else(|| e.message.clone(), |d| format!("{}: {d}", e.message)))?;
    Ok((path_text, result_id, decision, score))
}

/// Writes an auto-apply result to its file, every field of its proposal,
/// through the same recorded path as a user's apply
/// ([`rbl_identify::write::apply_result`]), so it ends `AutoApplied` and can
/// be undone. The proposal must still only fill empty fields (never a
/// sub-genre, never an overwrite; artwork only for a file without any).
/// When it is not written the result becomes a pending review carrying the
/// reason; the error is that reason.
fn auto_apply(store: &Store, result: ResultId) -> Result<(), String> {
    let written = (|| {
        let (item, ..) = store.result(result).map_err(|e| e.to_string())?.ok_or("the result is not in the store")?;
        let proposal = item.proposal.as_ref().filter(|p| p.fills_only_empty()).ok_or("the proposal does not only fill empty fields")?;
        let fields: Vec<Field> = proposal.changes.iter().map(|c| c.field).collect();
        rbl_identify::write::apply_result(store, result, &fields).map(|_| ()).map_err(|e| e.to_string())
    })();
    written.map_err(|e| {
        let reason = format!("Auto-apply did not write this file: {e}");
        // A write left open (it may have half landed) keeps the result
        // accepted until startup reconciles it.
        if let Err(e) = store.demote_auto_apply(result, &reason) {
            tracing::warn!(error = %e, "identify: an unwritten auto-apply could not be returned to review");
        }
        reason
    })
}

/// Saves a finished track (its cover, row, result and the imported library
/// record) in one transaction, then drops the cover's bytes: the store
/// holds them now.
fn save(
    store: &Store,
    key: &TrackKey,
    result: &mut TrackResult,
    library: &LibraryTrack,
    rekordbox: Option<&RekordboxRecord>,
) -> rbl_identify::Result<ResultId> {
    let (_, saved) = store.save_track_result_with(key, result, Some(library), rekordbox, false)?;
    result.artwork = None;
    Ok(saved)
}

/// Starts the job thread, which emits its events and frees the job slot
/// when it ends, however it ends.
fn spawn_job<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: &Arc<IdentifyState>,
    identifier: Arc<dyn Identifier>,
    tracks: Vec<JobTrack>,
) -> AppResult<u32> {
    let (job_id, cancel) = state.begin_job()?;
    let total = u32::try_from(tracks.len()).unwrap_or(u32::MAX);
    state.record(&IdentifyProgressDto::new(job_id, total));
    let thread_state = Arc::clone(state);
    let spawned = std::thread::Builder::new().name("identify-job".into()).spawn(move || {
        let state = thread_state;
        let mut emit = |event: JobEvent| match event {
            JobEvent::Progress(progress) => {
                state.record(&progress);
                let _ = tauri::Emitter::emit(&app, PROGRESS_EVENT, progress);
            }
            JobEvent::Result(result) => {
                let _ = tauri::Emitter::emit(&app, RESULT_EVENT, result);
            }
        };
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_job(&state, job_id, &tracks, identifier.as_ref(), &cancel, &mut emit);
        }));
        if run.is_err() {
            tracing::error!(job = job_id, "identify job panicked");
            let mut failed = state.progress().filter(|p| p.job_id == job_id).unwrap_or_else(|| IdentifyProgressDto::new(job_id, total));
            failed.state = JobState::Failed;
            failed.message = Some("Identification stopped unexpectedly.".to_owned());
            emit(JobEvent::Progress(failed));
        }
        // Whatever happened, the slot is free once the thread ends.
        state.end_job(job_id);
    });
    match spawned {
        Ok(_) => Ok(job_id),
        Err(e) => {
            state.end_job(job_id);
            Err(AppError::internal(format!("could not start the identify job: {e}")))
        }
    }
}

/// The library's tracks for these ids, in order, each id once. A track the
/// library cannot place gets no path, and fails on its own in the job.
/// Each placed track carries its library record as of now (`imported_unix`),
/// read from the in-memory library; the ISRC, which that does not hold,
/// comes from `isrcs`.
fn resolve_tracks(
    library: &rbl_index::Library,
    ids: Vec<String>,
    isrcs: &std::collections::HashMap<String, String>,
    imported_unix: i64,
) -> Vec<JobTrack> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .map(|id| {
            let path = library.audio_path_of(&id).filter(|p| !p.is_empty()).map(str::to_owned);
            let row = library.row_of(&id).map(|row| row as usize);
            let title = row
                .map(|row| library.title.get(row).to_owned())
                .filter(|t| !t.is_empty())
                .or_else(|| {
                    path.as_deref().and_then(|p| Path::new(p).file_name()).map(|n| n.to_string_lossy().into_owned())
                })
                .unwrap_or_default();
            let rekordbox = row.map(|row| rekordbox_record(library, row, &id, isrcs, imported_unix));
            JobTrack { track_id: id, title, path, rekordbox }
        })
        .collect()
}

/// One track's library record, read from the in-memory library.
fn rekordbox_record(
    library: &rbl_index::Library,
    row: usize,
    id: &str,
    isrcs: &std::collections::HashMap<String, String>,
    imported_unix: i64,
) -> RekordboxRecord {
    let lookup = |ids: &[u32], names: &rbl_index::strings::Interner| ids.get(row).map_or("", |&n| names.name(n)).to_owned();
    let year = library.year.get(row).copied().unwrap_or(0);
    RekordboxRecord {
        track_id: id.to_owned(),
        title: library.title.get(row).to_owned(),
        artist: lookup(&library.artist, &library.artists),
        album: lookup(&library.album, &library.albums),
        label: lookup(&library.label, &library.labels),
        genre: lookup(&library.genre, &library.genres),
        year: if year == 0 { String::new() } else { year.to_string() },
        comment: library.comment.get(row).to_owned(),
        isrc: isrcs.get(id).cloned().unwrap_or_default(),
        imported_unix,
    }
}

/// The ISRCs of these library tracks (`djmdContent.ISRC`, which the
/// in-memory library does not hold), read through the app's read-only
/// connection. Tracks without one are left out.
fn read_isrcs(db: &rbl_db::Library, ids: &[String]) -> Result<std::collections::HashMap<String, String>, rbl_db::DbError> {
    // One query per chunk of ids, not per id: the reader is shared, and a
    // whole-library job must not hold it for one query per track. A short
    // last chunk is padded with "", which no track id is.
    let marks = vec!["?"; ISRC_CHUNK].join(", ");
    let mut stmt = db
        .connection()
        .prepare(&format!("SELECT ID, ISRC FROM djmdContent WHERE ID IN ({marks}) AND ISRC IS NOT NULL AND ISRC != ''"))?;
    let mut isrcs = std::collections::HashMap::new();
    for chunk in ids.chunks(ISRC_CHUNK) {
        let mut params = [""; ISRC_CHUNK];
        for (slot, id) in params.iter_mut().zip(chunk) {
            *slot = id.as_str();
        }
        let rows = stmt.query_map(params, |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, isrc) = row?;
            if !isrc.trim().is_empty() {
                isrcs.insert(id, isrc.trim().to_owned());
            }
        }
    }
    Ok(isrcs)
}

/// Track ids per ISRC query: the largest fixed-size array rusqlite binds
/// as parameters (32), so ~1,600 queries for 50,000 tracks instead of 50,000.
const ISRC_CHUNK: usize = 32;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------- commands

/// Starts identifying these library tracks in the background; resolves to
/// the job's id. Refused while a job runs, and without keys.
#[tauri::command]
pub async fn identify_start<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, Arc<AppState>>,
    identify: State<'_, Arc<IdentifyState>>,
    track_ids: Vec<String>,
) -> AppResult<u32> {
    if track_ids.is_empty() {
        return Err(AppError::new(ErrorKind::Malformed, "Choose at least one track to identify."));
    }
    let identify = Arc::clone(&identify);
    if identify.is_running() {
        return Err(already_running());
    }
    let library = state.library()?;
    let state = Arc::clone(&state);
    blocking("identify_start", move || {
        let engine: Arc<dyn Identifier> = identify.engine()?;
        // Opened now so a store that cannot open fails the start, not
        // every track.
        identify.with_store(|_| Ok(()))?;
        // Read-only: the imported records lack an ISRC rather than the
        // job failing when the library cannot be read.
        // The shared reader is taken per batch, so other readers get a turn
        // during a whole-library job.
        let mut isrcs = std::collections::HashMap::new();
        for batch in track_ids.chunks(ISRC_CHUNK * 16) {
            match state.read_db(|db| read_isrcs(db, batch)) {
                Ok(found) => isrcs.extend(found),
                Err(e) => {
                    tracing::warn!(error = %e, "identify: the library's ISRCs could not be read");
                    break;
                }
            }
        }
        let tracks = resolve_tracks(&library, track_ids, &isrcs, now_unix());
        spawn_job(app, &identify, engine, tracks)
    })
    .await
}

/// Stops job `job_id` after the track it is on. Nothing happens for a job
/// that has already ended.
#[tauri::command]
#[allow(clippy::needless_pass_by_value, reason = "Tauri's State extractor is injected by value")]
pub fn identify_cancel(identify: State<'_, Arc<IdentifyState>>, job_id: u32) {
    identify.cancel(job_id);
}

/// The running or last job's progress; `None` before the first job.
#[tauri::command]
#[allow(clippy::needless_pass_by_value, reason = "Tauri's State extractor is injected by value")]
pub fn identify_progress(identify: State<'_, Arc<IdentifyState>>) -> Option<IdentifyProgressDto> {
    identify.progress()
}

/// A window of the review queue and the filter's total.
#[tauri::command]
pub async fn identify_queue(
    identify: State<'_, Arc<IdentifyState>>,
    filter: QueueFilter,
    offset: u32,
    limit: u32,
) -> AppResult<IdentifyQueueDto> {
    let identify = Arc::clone(&identify);
    blocking("identify_queue", move || identify.queue(filter, offset, limit)).await
}

/// One result in full: the per-field diff, every candidate, warnings.
#[tauri::command]
pub async fn identify_result(identify: State<'_, Arc<IdentifyState>>, result_id: ResultId) -> AppResult<IdentifyDetailDto> {
    let identify = Arc::clone(&identify);
    blocking("identify_result", move || identify.detail(result_id)).await
}

/// Accepts, rejects or returns a result to review. The identify store
/// only: nothing is written to the file until Phase 4's apply.
#[tauri::command]
pub async fn identify_set_state(
    identify: State<'_, Arc<IdentifyState>>,
    result_id: ResultId,
    action: ReviewAction,
) -> AppResult<IdentifyQueueItemDto> {
    let identify = Arc::clone(&identify);
    blocking("identify_set_state", move || identify.set_review(result_id, action)).await
}

/// Replaces proposed values with the user's. The identify store only.
#[tauri::command]
pub async fn identify_edit(
    identify: State<'_, Arc<IdentifyState>>,
    result_id: ResultId,
    changes: Vec<FieldEditDto>,
) -> AppResult<IdentifyQueueItemDto> {
    let identify = Arc::clone(&identify);
    blocking("identify_edit", move || identify.edit(result_id, &changes)).await
}

/// The identification settings: where each key comes from (never a key),
/// the user agent, the genre policy and auto-apply.
#[tauri::command]
pub async fn identify_settings(identify: State<'_, Arc<IdentifyState>>) -> AppResult<IdentifySettingsDto> {
    let identify = Arc::clone(&identify);
    blocking("identify_settings", move || Ok(identify.settings())).await
}

/// Saves a settings change and answers with the settings as now saved. The
/// next job runs with them; a running job keeps its own.
#[tauri::command]
pub async fn identify_save_settings(
    identify: State<'_, Arc<IdentifyState>>,
    update: IdentifySettingsUpdateDto,
) -> AppResult<IdentifySettingsDto> {
    let identify = Arc::clone(&identify);
    blocking("identify_save_settings", move || identify.save_settings(&update)).await
}

// ---------------------------------------------------------------- writing

impl IdentifyState {
    /// Writes one result's ticked fields to its file, snapshotted before so
    /// the write can be undone. The file only: never the library.
    fn apply_one(&self, item: &ApplyItemDto) -> Result<(), String> {
        let fields = item
            .fields
            .iter()
            .map(|name| parse_field(name).ok_or_else(|| format!("There is no tag field \"{name}\".")))
            .collect::<Result<Vec<_>, _>>()?;
        let outcome = self.with_store(|store| {
            Ok(rbl_identify::write::apply_result(store, item.result_id, &fields).map(|_| ()).map_err(|e| e.to_string()))
        });
        flatten(outcome)
    }

    /// Undoes one result's write: the file's exact tags. A library side an
    /// older build recorded is ignored; the library is never written.
    fn undo_one(&self, result: ResultId) -> Result<(), String> {
        let outcome = self.with_store(|store| {
            Ok(rbl_identify::write::undo_result(store, result).map(|_| ()).map_err(|e| e.to_string()))
        });
        flatten(outcome)
    }

    /// Applies each item; returns each item's outcome.
    pub(crate) fn apply_items(&self, items: &[ApplyItemDto]) -> Vec<IdentifyWriteOutcomeDto> {
        items.iter().map(|item| IdentifyWriteOutcomeDto::new(item.result_id, self.apply_one(item))).collect()
    }

    pub(crate) fn undo_item(&self, result: ResultId) -> IdentifyWriteOutcomeDto {
        IdentifyWriteOutcomeDto::new(result, self.undo_one(result))
    }

    fn accept_above(&self, threshold: f64) -> AppResult<Vec<ResultId>> {
        self.with_store(|store| store.accept_at_or_above(threshold).map_err(refused))
    }
}

/// A store call's own refusal, or the store's failure, as one message.
fn flatten(outcome: AppResult<Result<(), String>>) -> Result<(), String> {
    match outcome {
        Ok(result) => result,
        Err(e) => Err(e.detail.map_or(e.message.clone(), |d| format!("{}: {d}", e.message))),
    }
}

/// Tells the window which results changed. Only `identify:changed`: the
/// library is never written, so it never changes here.
fn announce<R: tauri::Runtime>(app: &tauri::AppHandle<R>, result_ids: Vec<ResultId>) {
    if !result_ids.is_empty() {
        let _ = tauri::Emitter::emit(app, CHANGED_EVENT, IdentifyChangedDto { result_ids });
    }
}

/// Writes the ticked fields of each result to its file. Each item answers
/// for itself; refused items change nothing. rekordbox may be running:
/// its library is not touched.
#[tauri::command]
pub async fn identify_apply<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    identify: State<'_, Arc<IdentifyState>>,
    items: Vec<ApplyItemDto>,
) -> AppResult<Vec<IdentifyWriteOutcomeDto>> {
    let identify = Arc::clone(&identify);
    let outcomes = blocking("identify_apply", move || Ok(identify.apply_items(&items))).await?;
    announce(&app, outcomes.iter().filter(|o| o.ok).map(|o| o.result_id).collect());
    Ok(outcomes)
}

/// Puts back the file's tags as they were before the result's last write.
/// Refused (ok: false) when the file changed since.
#[tauri::command]
pub async fn identify_undo<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    identify: State<'_, Arc<IdentifyState>>,
    result_id: ResultId,
) -> AppResult<IdentifyWriteOutcomeDto> {
    let identify = Arc::clone(&identify);
    let outcome = blocking("identify_undo", move || Ok(identify.undo_item(result_id))).await?;
    announce(&app, if outcome.ok { vec![result_id] } else { Vec::new() });
    Ok(outcome)
}

/// Accepts every pending result scoring at least `threshold`. Writes
/// nothing; resolves to how many were accepted.
#[tauri::command]
pub async fn identify_accept_above<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    identify: State<'_, Arc<IdentifyState>>,
    threshold: f64,
) -> AppResult<usize> {
    let identify = Arc::clone(&identify);
    let accepted = blocking("identify_accept_above", move || identify.accept_above(threshold)).await?;
    let count = accepted.len();
    announce(&app, accepted);
    Ok(count)
}

/// One item of `identify_apply`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyItemDto {
    pub result_id: ResultId,
    /// Frontend field names (`title`, `catalogNumber`, `subGenre`, ...).
    pub fields: Vec<String>,
}

/// How one apply or undo went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyWriteOutcomeDto {
    pub result_id: ResultId,
    pub ok: bool,
    pub error: Option<String>,
}

impl IdentifyWriteOutcomeDto {
    fn new(result_id: ResultId, result: Result<(), String>) -> Self {
        match result {
            Ok(()) => Self { result_id, ok: true, error: None },
            Err(error) => Self { result_id, ok: false, error: Some(error) },
        }
    }
}

/// `identify:changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyChangedDto {
    pub result_ids: Vec<ResultId>,
}

// ---------------------------------------------------------------- DTOs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JobState {
    Running,
    Done,
    Cancelled,
    Failed,
}

/// `identify:progress`, and what `identify_progress` answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyProgressDto {
    pub job_id: u32,
    pub state: JobState,
    /// Tracks finished, whatever their outcome.
    pub done: u32,
    pub total: u32,
    /// The track being identified; empty once the job has ended.
    pub current_title: String,
    pub current_path: String,
    /// Finished tracks by outcome: identified (auto-apply), needing review,
    /// unidentified, failed (no file, unreadable, not saved).
    pub identified: u32,
    pub review: u32,
    pub unidentified: u32,
    pub failed: u32,
    /// Why a job failed; `None` otherwise.
    pub message: Option<String>,
}

impl IdentifyProgressDto {
    fn new(job_id: u32, total: u32) -> Self {
        Self {
            job_id,
            state: JobState::Running,
            done: 0,
            total,
            current_title: String::new(),
            current_path: String::new(),
            identified: 0,
            review: 0,
            unidentified: 0,
            failed: 0,
            message: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResultDecision {
    AutoApply,
    Review,
    Unidentified,
    /// The track could not be identified or saved; see `error`.
    Failed,
}

impl From<Decision> for ResultDecision {
    fn from(decision: Decision) -> Self {
        match decision {
            Decision::AutoApply => Self::AutoApply,
            Decision::Review => Self::Review,
            Decision::Unidentified => Self::Unidentified,
        }
    }
}

/// `identify:result`: one finished track.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyResultDto {
    pub job_id: u32,
    /// The library's track id, as the job was given it.
    pub track_id: String,
    /// The stored result, for `identify_result`; `None` when it failed.
    pub result_id: Option<ResultId>,
    /// The file, canonicalised once it was found.
    pub path: String,
    pub decision: ResultDecision,
    pub score: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueFilter {
    Unidentified,
    NeedsReview,
    Applied,
    All,
}

impl From<QueueFilter> for Filter {
    fn from(filter: QueueFilter) -> Self {
        match filter {
            QueueFilter::Unidentified => Self::Unidentified,
            QueueFilter::NeedsReview => Self::NeedsReview,
            QueueFilter::Applied => Self::Applied,
            QueueFilter::All => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewAction {
    Accept,
    Reject,
    /// Back to review, from accepted or rejected.
    Pending,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldEditDto {
    pub field: String,
    pub proposed: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyQueueDto {
    pub total: u64,
    pub items: Vec<IdentifyQueueItemDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldChangeDto {
    /// `title`, `artist`, `catalogNumber`, `mbRecordingId`, `artwork`, ...
    pub field: String,
    pub current: String,
    pub proposed: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyQueueItemDto {
    pub result_id: ResultId,
    /// The library track the result was identified for; empty for one
    /// stored before tracks were linked.
    pub track_id: String,
    pub path: String,
    /// `unidentified`, `pending`, `accepted`, `rejected`, `applied`,
    /// `autoApplied` or `undone`.
    pub state: String,
    /// `autoApply`, `review` or `unidentified`.
    pub decision: String,
    pub score: Option<f64>,
    /// The file's own artist and title, as last read.
    pub artist: String,
    pub title: String,
    pub changes: Vec<FieldChangeDto>,
    pub updated_unix: i64,
    /// Applied, so `identify_undo` can put its last write back.
    pub undoable: bool,
    /// Where the proposed candidate came from: `fingerprint`, `textSearch`
    /// or `discogsText`; `None` without a proposal.
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingTagsDto {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub label: String,
    pub year: Option<u16>,
    pub isrc: String,
    pub genre: String,
    pub catalog_number: String,
    /// The comment as rekordbox reads it from the file: what a `subGenre`
    /// change appends to.
    pub comment: String,
    pub has_artwork: bool,
}

impl From<&rbl_identify::model::ExistingTags> for ExistingTagsDto {
    fn from(t: &rbl_identify::model::ExistingTags) -> Self {
        Self {
            artist: t.artist.clone(),
            title: t.title.clone(),
            album: t.album.clone(),
            label: t.label.clone(),
            year: t.year,
            isrc: t.isrc.clone(),
            genre: t.genre.clone(),
            catalog_number: t.catalog_number.clone(),
            comment: t.comment.clone(),
            has_artwork: t.has_artwork,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseDto {
    pub mbid: String,
    pub title: String,
    pub date: String,
    pub country: String,
    pub label: String,
    pub catalog_number: String,
    pub release_group_mbid: String,
    pub release_group_type: String,
    pub secondary_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateDto {
    pub recording_mbid: String,
    /// The artist credit as MusicBrainz joins it.
    pub artist: String,
    pub title: String,
    pub disambiguation: String,
    pub remixers: Vec<String>,
    pub length_ms: Option<u32>,
    pub isrcs: Vec<String>,
    pub genres: Vec<String>,
    pub releases: Vec<ReleaseDto>,
    /// `fingerprint`, `textSearch` or `discogsText`.
    pub source: String,
    pub score: f64,
    pub acoustid_score: Option<f64>,
    pub duration_score: f64,
    pub text_score: f64,
    pub mix_score: f64,
    /// Why a hard rule rejected or capped it.
    pub rejected: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichmentDto {
    pub discogs_release_id: Option<u64>,
    pub release_title: String,
    pub label: String,
    pub catalog_number: String,
    pub year: Option<u16>,
    pub styles: Vec<String>,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtworkDto {
    /// `coverArtArchive` or `discogs`.
    pub source: String,
    pub url: String,
    pub sha256: String,
    pub mime: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyDetailDto {
    pub item: IdentifyQueueItemDto,
    pub existing: ExistingTagsDto,
    /// Best first.
    pub candidates: Vec<CandidateDto>,
    pub enrichment: Option<EnrichmentDto>,
    /// The cover proposed for a file without one.
    pub artwork: Option<ArtworkDto>,
    pub warnings: Vec<String>,
    /// A write to the file is open: review changes are refused until it is
    /// reconciled.
    pub write_in_progress: bool,
    /// The rekordbox library record as imported when the track was
    /// identified; `None` for a result saved before imports existed.
    pub rekordbox: Option<IdentifyRekordboxRecordDto>,
}

/// A rekordbox library record imported into the identify store. Read from
/// the library; identification never writes it back. Absent values are "".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyRekordboxRecordDto {
    pub track_id: String,
    /// RFC 3339, UTC.
    pub imported_at: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub label: String,
    pub genre: String,
    pub year: String,
    pub comment: String,
    pub isrc: String,
}

fn rekordbox_dto(r: &RekordboxRecord) -> IdentifyRekordboxRecordDto {
    IdentifyRekordboxRecordDto {
        track_id: r.track_id.clone(),
        imported_at: rfc3339(r.imported_unix),
        title: r.title.clone(),
        artist: r.artist.clone(),
        album: r.album.clone(),
        label: r.label.clone(),
        genre: r.genre.clone(),
        year: r.year.clone(),
        comment: r.comment.clone(),
        isrc: r.isrc.clone(),
    }
}

/// Seconds since the Unix epoch as RFC 3339 in UTC; "" when out of range.
fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default()
}

/// A model enum as the frontend spells it: its serde name with the first
/// letter lowercased (`CatalogNumber` → `catalogNumber`). Derived rather
/// than listed, so a field added to the model needs no change here.
fn camel<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => lower_first(&name),
        _ => String::new(),
    }
}

fn lower_first(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| first.to_lowercase().chain(chars).collect())
}

/// The model field a frontend name spells, the inverse of [`camel`].
fn parse_field(name: &str) -> Option<Field> {
    let mut chars = name.chars();
    let pascal: String = chars.next()?.to_uppercase().chain(chars).collect();
    serde_json::from_value(serde_json::Value::String(pascal)).ok()
}

fn change_dto(change: &FieldChange) -> FieldChangeDto {
    FieldChangeDto { field: camel(&change.field), current: change.current.clone(), proposed: change.proposed.clone() }
}

/// A queue row, `undoable` only when undo would go ahead: the result is
/// applied and its last write recorded the file's tags after it (a write
/// recorded by a version 1 store did not, and undo refuses it).
fn item_dto(store: &Store, item: &QueueItem) -> AppResult<IdentifyQueueItemDto> {
    let mut dto = queue_item_dto(item);
    if dto.undoable {
        let last = store.last_application(item.result).map_err(store_error)?;
        dto.undoable = match last {
            Some(application) if application.undone_unix.is_none() => {
                store.application_after_snapshot(application.id).map_err(store_error)?.is_some()
            }
            _ => false,
        };
    }
    Ok(dto)
}

fn queue_item_dto(item: &QueueItem) -> IdentifyQueueItemDto {
    IdentifyQueueItemDto {
        result_id: item.result,
        track_id: item.library.as_ref().map(|l| l.track_id.clone()).unwrap_or_default(),
        path: item.path.clone(),
        state: camel(&item.state),
        decision: camel(&item.decision),
        score: item.score,
        artist: item.existing.artist.clone(),
        title: item.existing.title.clone(),
        changes: item.proposal.as_ref().map(|p: &Proposal| p.changes.iter().map(change_dto).collect()).unwrap_or_default(),
        updated_unix: item.updated_unix,
        undoable: item.state.is_applied(),
        source: item.source.as_ref().map(camel),
    }
}

fn candidate_dto(scored: &Scored) -> CandidateDto {
    let c = &scored.candidate;
    CandidateDto {
        recording_mbid: c.recording_mbid.clone(),
        artist: c.artist_credit.clone(),
        title: c.title.clone(),
        disambiguation: c.disambiguation.clone(),
        remixers: c.remixers.clone(),
        length_ms: c.length_ms,
        isrcs: c.isrcs.clone(),
        genres: c.genres.clone(),
        releases: c
            .releases
            .iter()
            .map(|r| ReleaseDto {
                mbid: r.mbid.clone(),
                title: r.title.clone(),
                date: r.date.clone(),
                country: r.country.clone(),
                label: r.label.clone(),
                catalog_number: r.catalog_number.clone(),
                release_group_mbid: r.release_group_mbid.clone(),
                release_group_type: r.release_group_type.clone(),
                secondary_types: r.secondary_types.clone(),
            })
            .collect(),
        source: camel(&c.source),
        score: scored.score,
        acoustid_score: scored.breakdown.acoustid,
        duration_score: scored.breakdown.duration,
        text_score: scored.breakdown.text,
        mix_score: scored.breakdown.mix,
        rejected: scored.breakdown.rejected.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use rbl_identify::coverart::FetchedImage;
    use rbl_identify::model::{
        ArtworkRef, ArtworkSource, Candidate, CandidateSource, ExistingTags, ScoreBreakdown, TagSnapshot,
    };
    use rbl_identify::pipeline::FingerprintOutcome;
    use rbl_identify::tags;

    /// Answers by file name: `review*` → Review with a cover, `broken*` →
    /// an error, anything else → Unidentified. Can raise the cancel flag on
    /// a chosen call.
    struct Fake {
        calls: AtomicU32,
        cancel_on_call: Option<(u32, Arc<AtomicBool>)>,
    }

    impl Fake {
        fn new() -> Self {
            Self { calls: AtomicU32::new(0), cancel_on_call: None }
        }
    }

    const JPEG: &[u8] = b"\xff\xd8\xff\xe0fake jpeg";

    fn candidate() -> Scored {
        Scored {
            candidate: Candidate {
                recording_mbid: "41f4b491-cc31-40f1-8d81-cc58b7d8f761".into(),
                artists: vec!["Deepack".into()],
                artist_credit: "Deepack".into(),
                title: "The Music (Extended Mix)".into(),
                disambiguation: String::new(),
                remixers: Vec::new(),
                length_ms: Some(300_000),
                isrcs: Vec::new(),
                genres: Vec::new(),
                releases: Vec::new(),
                acoustid_score: Some(0.9),
                source: CandidateSource::Fingerprint,
            },
            score: 0.8,
            breakdown: ScoreBreakdown { acoustid: Some(0.9), duration: 1.0, text: 0.7, mix: 1.0, rejected: None },
        }
    }

    fn result(path: &Path, decision: Decision) -> TrackResult {
        let reviewed = decision == Decision::Review;
        let artwork = reviewed.then(|| FetchedImage {
            bytes: JPEG.to_vec(),
            mime: "image/jpeg".into(),
            url: "https://coverartarchive.org/release/x/front-500".into(),
        });
        let art_ref = artwork.as_ref().map(|image| ArtworkRef {
            source: ArtworkSource::CoverArtArchive,
            url: image.url.clone(),
            // The real hash: the store refuses a proposal naming other bytes.
            sha256: Store::open_in_memory().unwrap().put_artwork(&image.bytes, &image.mime).unwrap(),
            mime: image.mime.clone(),
            bytes: image.bytes.len() as u64,
        });
        let proposal = reviewed.then(|| {
            Proposal::new(
                vec![FieldChange { field: Field::CatalogNumber, current: String::new(), proposed: "ABC001".into() }],
                art_ref,
                false,
            )
        });
        TrackResult {
            path: path.to_owned(),
            existing: ExistingTags { artist: "Deepack".into(), title: "The Music".into(), ..ExistingTags::default() },
            duration_secs: 300.0,
            fingerprint: FingerprintOutcome::default(),
            encoded_fingerprint: None,
            candidates: if reviewed { vec![candidate()] } else { Vec::new() },
            decision,
            snapshot: TagSnapshot::default(),
            proposal,
            enrichment: None,
            artwork,
            warnings: Vec::new(),
        }
    }

    /// A fingerprint match that only fills the file's empty title and
    /// catalog number, decided `decision`.
    fn fill_result(path: &Path, decision: Decision) -> TrackResult {
        let mut r = result(path, Decision::Review);
        r.decision = decision;
        r.artwork = None;
        r.proposal = Some(Proposal::new(
            vec![
                FieldChange { field: Field::Title, current: String::new(), proposed: "The Music (Extended Mix)".into() },
                FieldChange { field: Field::CatalogNumber, current: String::new(), proposed: "ABC001".into() },
            ],
            None,
            true,
        ));
        r
    }

    impl Identifier for Fake {
        fn identify(&self, path: &Path) -> rbl_identify::Result<TrackResult> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((on, flag)) = &self.cancel_on_call {
                if *on == call {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with("broken") {
                return Err(rbl_identify::IdentifyError::Fingerprint("not audio".into()));
            }
            if name.starts_with("auto") || name.starts_with("fill") {
                return Ok(fill_result(path, if name.starts_with("auto") { Decision::AutoApply } else { Decision::Review }));
            }
            Ok(result(path, if name.starts_with("review") { Decision::Review } else { Decision::Unidentified }))
        }
    }

    fn track(dir: &Path, name: &str, create: bool) -> JobTrack {
        let path = dir.join(name);
        if create {
            std::fs::write(&path, b"x").unwrap();
        }
        JobTrack {
            track_id: name.to_owned(),
            title: name.to_owned(),
            path: Some(path.to_string_lossy().into_owned()),
            rekordbox: None,
        }
    }

    fn run(state: &IdentifyState, tracks: &[JobTrack], fake: &Fake, cancel: &AtomicBool) -> Vec<JobEvent> {
        let mut events = Vec::new();
        run_job(state, 7, tracks, fake, cancel, &mut |e| events.push(e));
        events
    }

    #[test]
    fn a_job_reports_each_track_in_order_and_saves_what_finished() {
        let files = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let tracks = vec![
            track(files.path(), "review.mp3", true),
            track(files.path(), "unknown.mp3", true),
            track(files.path(), "broken.mp3", true),
            track(files.path(), "gone.mp3", false),
            JobTrack { track_id: "nopath".into(), title: "No path".into(), path: None, rekordbox: None },
        ];
        let events = run(&state, &tracks, &Fake::new(), &AtomicBool::new(false));

        // A running progress, then a result, per track; then the final one.
        assert_eq!(events.len(), 2 * tracks.len() + 1);
        for (i, pair) in events.chunks(2).take(tracks.len()).enumerate() {
            let JobEvent::Progress(p) = &pair[0] else { panic!("expected progress, got {:?}", pair[0]) };
            assert_eq!(
                (p.state, p.done, p.total, p.current_title.as_str()),
                (JobState::Running, i as u32, 5, tracks[i].title.as_str())
            );
            let JobEvent::Result(r) = &pair[1] else { panic!("expected a result, got {:?}", pair[1]) };
            assert_eq!((r.job_id, r.track_id.as_str()), (7, tracks[i].track_id.as_str()));
        }
        let decisions: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                JobEvent::Result(r) => Some((r.decision, r.result_id.is_some(), r.error.is_some())),
                JobEvent::Progress(_) => None,
            })
            .collect();
        assert_eq!(
            decisions,
            vec![
                (ResultDecision::Review, true, false),
                (ResultDecision::Unidentified, true, false),
                (ResultDecision::Failed, false, true),
                (ResultDecision::Failed, false, true),
                (ResultDecision::Failed, false, true),
            ]
        );
        let Some(JobEvent::Progress(last)) = events.last() else { panic!("no final progress") };
        assert_eq!(
            (last.state, last.done, last.identified, last.review, last.unidentified, last.failed),
            (JobState::Done, 5, 0, 1, 1, 3)
        );
        assert_eq!(last.current_title, "");

        // Both finished tracks are in the store, the cover with them.
        assert_eq!(state.queue(QueueFilter::All, 0, 50).unwrap().total, 2);
        let review = state.queue(QueueFilter::NeedsReview, 0, 50).unwrap();
        assert_eq!(
            review.items[0].changes.iter().map(|c| c.field.as_str()).collect::<Vec<_>>(),
            ["catalogNumber", "artwork"]
        );
        let hash = Store::open_in_memory().unwrap().put_artwork(JPEG, "image/jpeg").unwrap();
        let stored = state.with_store(|s| s.artwork(&hash).map_err(store_error)).unwrap();
        assert_eq!(stored.map(|(bytes, _)| bytes), Some(JPEG.to_vec()));
    }

    #[test]
    fn an_auto_apply_result_is_written_by_the_job_and_can_be_undone() {
        let files = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let audio = files.path().join("auto.wav");
        wav(&audio);
        let original = std::fs::read(&audio).unwrap();
        let job = JobTrack {
            track_id: "1".into(),
            title: "auto".into(),
            path: Some(audio.to_string_lossy().into_owned()),
            rekordbox: None,
        };
        let events = run(&state, &[job], &Fake::new(), &AtomicBool::new(false));
        let JobEvent::Result(done) = &events[1] else { panic!("expected a result") };
        assert_eq!((done.decision, done.error.as_deref()), (ResultDecision::AutoApply, None));
        let Some(JobEvent::Progress(last)) = events.last() else { panic!("no final progress") };
        assert_eq!((last.identified, last.review), (1, 0));
        let written = tags::read(&audio).unwrap().snapshot;
        assert_eq!(rbl_identify::pipeline::current(&written, Field::Title), "The Music (Extended Mix)");
        assert_eq!(rbl_identify::pipeline::current(&written, Field::CatalogNumber), "ABC001");
        let applied = state.queue(QueueFilter::Applied, 0, 10).unwrap().items.remove(0);
        assert_eq!((applied.state.as_str(), applied.undoable), ("autoApplied", true));
        assert!(state.undo_item(applied.result_id).ok);
        assert_eq!(std::fs::read(&audio).unwrap(), original);
    }

    #[test]
    fn an_auto_apply_that_cannot_be_written_is_left_for_review_and_the_job_goes_on() {
        let files = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let audio = files.path().join("auto.wav");
        wav(&audio);
        let original = std::fs::read(&audio).unwrap();
        let mut permissions = std::fs::metadata(&audio).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&audio, permissions.clone()).unwrap();
        let tracks = [
            JobTrack { track_id: "1".into(), title: "auto".into(), path: Some(audio.to_string_lossy().into_owned()), rekordbox: None },
            track(files.path(), "unknown.mp3", true),
        ];
        let events = run(&state, &tracks, &Fake::new(), &AtomicBool::new(false));
        let JobEvent::Result(first) = &events[1] else { panic!("expected a result") };
        assert_eq!(first.decision, ResultDecision::Review, "reported as what happened");
        let Some(JobEvent::Progress(last)) = events.last() else { panic!("no final progress") };
        assert_eq!((last.done, last.identified, last.review, last.unidentified), (2, 0, 1, 1));
        #[allow(clippy::permissions_set_readonly_false, reason = "the test file must be deletable again")]
        permissions.set_readonly(false);
        std::fs::set_permissions(&audio, permissions).unwrap();
        assert_eq!(std::fs::read(&audio).unwrap(), original, "nothing written");
        let item = state.queue(QueueFilter::NeedsReview, 0, 10).unwrap().items.remove(0);
        assert_eq!((item.state.as_str(), item.decision.as_str()), ("pending", "review"));
        let detail = state.detail(item.result_id).unwrap();
        assert!(detail.warnings.iter().any(|w| w.contains("read-only")), "{:?}", detail.warnings);
        assert_eq!(state.with_store(|s| s.open_applications().map_err(store_error)).unwrap(), []);
        // Applied by the user later, it is the user's write.
        assert!(state.apply_items(&ticked(item.result_id, &["title"]))[0].ok);
        assert_eq!(state.detail(item.result_id).unwrap().item.state, "applied");
    }

    #[test]
    fn a_fill_only_result_not_decided_for_auto_apply_is_never_written() {
        let files = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let audio = files.path().join("fill.wav");
        wav(&audio);
        let original = std::fs::read(&audio).unwrap();
        let job = JobTrack { track_id: "1".into(), title: "fill".into(), path: Some(audio.to_string_lossy().into_owned()), rekordbox: None };
        run(&state, &[job], &Fake::new(), &AtomicBool::new(false));
        assert_eq!(std::fs::read(&audio).unwrap(), original);
        assert_eq!(state.queue(QueueFilter::NeedsReview, 0, 10).unwrap().items[0].state, "pending");
    }

    #[test]
    fn a_cancel_stops_between_tracks_and_keeps_the_finished_one() {
        let files = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let tracks: Vec<_> =
            ["review1.mp3", "review2.mp3", "review3.mp3"].iter().map(|n| track(files.path(), n, true)).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let fake = Fake { calls: AtomicU32::new(0), cancel_on_call: Some((1, Arc::clone(&cancel))) };
        let events = run(&state, &tracks, &fake, &cancel);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1, "no track starts after the cancel");
        assert_eq!(events.len(), 3, "one progress, one result, the final progress");
        let Some(JobEvent::Progress(last)) = events.last() else { panic!("no final progress") };
        assert_eq!((last.state, last.done, last.total), (JobState::Cancelled, 1, 3));
        assert_eq!(state.queue(QueueFilter::All, 0, 50).unwrap().total, 1);
    }

    #[test]
    fn only_one_job_runs_and_a_final_progress_frees_the_slot() {
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let (id, cancel) = state.begin_job().unwrap();
        assert_eq!(state.begin_job().unwrap_err().kind, ErrorKind::ReadOnly);
        state.cancel(id + 1);
        assert!(!cancel.load(Ordering::Relaxed), "another job's id cancels nothing");
        state.cancel(id);
        assert!(cancel.load(Ordering::Relaxed));
        let mut done = IdentifyProgressDto::new(id, 0);
        done.state = JobState::Cancelled;
        state.record(&done);
        assert!(!state.is_running());
        assert_eq!(state.progress(), Some(done));
        assert!(state.begin_job().is_ok());
    }

    #[test]
    fn review_changes_go_through_the_store_and_its_rules() {
        let files = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let tracks = [track(files.path(), "review.mp3", true), track(files.path(), "unknown.mp3", true)];
        run(&state, &tracks, &Fake::new(), &AtomicBool::new(false));
        let review = state.queue(QueueFilter::NeedsReview, 0, 10).unwrap().items.remove(0);
        let unknown = state.queue(QueueFilter::Unidentified, 0, 10).unwrap().items.remove(0);
        let edit = |field: &str, value: &str| vec![FieldEditDto { field: field.into(), proposed: value.into() }];

        let edited = state.edit(review.result_id, &edit("catalogNumber", "ABC-001")).unwrap();
        assert_eq!(edited.changes[0].proposed, "ABC-001");
        assert_eq!(state.edit(review.result_id, &edit("bpm", "128")).unwrap_err().kind, ErrorKind::Malformed);
        assert_eq!(state.edit(review.result_id, &edit("label", "x")).unwrap_err().kind, ErrorKind::Malformed);

        assert_eq!(state.set_review(review.result_id, ReviewAction::Accept).unwrap().state, "accepted");
        assert_eq!(state.set_review(unknown.result_id, ReviewAction::Accept).unwrap_err().kind, ErrorKind::Malformed);
        assert_eq!(state.set_review(9_999, ReviewAction::Reject).unwrap_err().kind, ErrorKind::NotFound);

        let detail = state.detail(review.result_id).unwrap();
        assert_eq!(detail.item.state, "accepted");
        assert_eq!(detail.candidates[0].source, "fingerprint");
        assert_eq!(detail.item.source.as_deref(), Some("fingerprint"));
        assert_eq!(unknown.source, None, "no proposal, no source");
        assert_eq!(detail.rekordbox, None, "the fake job imported no record");
        assert_eq!(detail.artwork.as_ref().map(|a| a.source.as_str()), Some("coverArtArchive"));
        assert!(!detail.write_in_progress);

        // An open write locks the review.
        state
            .with_store(|s| {
                s.begin_application(review.result_id, &[], None, &TagSnapshot::default(), None).map_err(store_error)
            })
            .unwrap();
        assert!(state.detail(review.result_id).unwrap().write_in_progress);
        assert_eq!(state.set_review(review.result_id, ReviewAction::Pending).unwrap_err().kind, ErrorKind::ReadOnly);
        assert_eq!(state.edit(review.result_id, &edit("catalogNumber", "Z")).unwrap_err().kind, ErrorKind::ReadOnly);
    }

    #[test]
    fn a_track_already_written_fails_alone_and_keeps_its_result() {
        let files = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let tracks = [track(files.path(), "review.mp3", true), track(files.path(), "unknown.mp3", true)];
        run(&state, &tracks, &Fake::new(), &AtomicBool::new(false));
        let review = state.queue(QueueFilter::NeedsReview, 0, 10).unwrap().items.remove(0);
        state.set_review(review.result_id, ReviewAction::Accept).unwrap();
        state
            .with_store(|s| {
                let application =
                    s.begin_application(review.result_id, &[], None, &TagSnapshot::default(), None).map_err(store_error)?;
                let after = TrackKey { path: review.path.clone(), file_size: 2, modified_unix: 1 };
                s.complete_application(application, &after).map_err(store_error)
            })
            .unwrap();

        let events = run(&state, &tracks, &Fake::new(), &AtomicBool::new(false));
        let Some(JobEvent::Progress(last)) = events.last() else { panic!("no final progress") };
        assert_eq!((last.state, last.failed, last.unidentified), (JobState::Done, 1, 1));
        let applied = state.queue(QueueFilter::Applied, 0, 10).unwrap();
        assert_eq!(applied.items.iter().map(|i| i.result_id).collect::<Vec<_>>(), [review.result_id]);
        // Written without an after-state (as a version 1 store recorded
        // writes): undo would refuse it, so it is not offered.
        assert!(!applied.items[0].undoable);
        assert!(!state.detail(review.result_id).unwrap().item.undoable);
    }

    #[test]
    fn the_queue_is_windowed_and_capped() {
        let files = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(root.path());
        let tracks: Vec<_> = (0..5).map(|i| track(files.path(), &format!("unknown{i}.mp3"), true)).collect();
        run(&state, &tracks, &Fake::new(), &AtomicBool::new(false));
        let page = state.queue(QueueFilter::All, 3, 10).unwrap();
        assert_eq!((page.total, page.items.len()), (5, 2));
        assert_eq!(state.queue(QueueFilter::Applied, 0, 10).unwrap().total, 0);
        assert!(state.queue(QueueFilter::All, 0, u32::MAX).unwrap().items.len() <= MAX_ROWS as usize);
    }

    /// State in `root` that sees no environment and no developer's file.
    fn isolated(root: &Path) -> IdentifyState {
        IdentifyState::with_sources(root.to_owned(), Vec::new(), Arc::new(|_: &str| None))
    }

    #[test]
    fn missing_keys_are_a_clear_error_that_never_shows_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let state = isolated(dir.path());
        let env_file = dir.path().join(ENV_FILE);
        let error = state.sources.config().unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound);
        assert!(error.message.contains("ACOUSTID_API_KEY"), "{}", error.message);

        std::fs::write(&env_file, "ACOUSTID_API_KEY=sekrit-key\n").unwrap();
        let error = state.sources.config().unwrap_err();
        let shown = format!("{error:?}");
        assert!(shown.contains("MB_USER_AGENT") && !shown.contains("sekrit-key"), "{shown}");

        std::fs::write(&env_file, "ACOUSTID_API_KEY=sekrit-key\nMB_USER_AGENT=rbxport-test/1 ( a@b.c )\n").unwrap();
        let config = state.sources.config().unwrap();
        assert_eq!(config.acoustid_key.expose(), "sekrit-key");
        assert!(config.discogs_token.is_none());
    }

    #[test]
    fn saving_settings_rebuilds_the_services_for_the_next_job_only() {
        let dir = tempfile::tempdir().unwrap();
        let state = isolated(dir.path());
        assert_eq!(state.engine().err().map(|e| e.kind), Some(ErrorKind::NotFound), "no keys yet");
        let keys: IdentifySettingsUpdateDto =
            serde_json::from_str(r#"{"acoustidKey":"sekrit-key","mbUserAgent":"rbxport-test/1 ( a@b.c )"}"#).unwrap();
        let saved = state.save_settings(&keys).unwrap();
        assert_eq!(saved.acoustid_key, Some(KeySource::Settings));
        assert!(!format!("{saved:?}").contains("sekrit-key"));

        let first = state.engine().unwrap();
        assert!(Arc::ptr_eq(&first, &state.engine().unwrap()), "kept between jobs");
        assert_eq!(first.settings, Settings::default(), "auto-apply off by default");
        assert!(first.discogs.is_none());

        // A job holds `first`; the user turns auto-apply on, raises the
        // threshold and adds a Discogs token.
        let change: IdentifySettingsUpdateDto = serde_json::from_str(
            r#"{"autoApply":true,"autoApplyThreshold":0.95,"genrePolicy":"replace","discogsToken":"tok"}"#,
        )
        .unwrap();
        state.save_settings(&change).unwrap();
        let next = state.engine().unwrap();
        assert!(!Arc::ptr_eq(&first, &next), "the next job gets new services");
        assert_eq!(
            next.settings,
            Settings { auto_apply: true, genre: rbl_identify::pipeline::GenrePolicy::Replace, auto_apply_threshold: 0.95 }
        );
        assert!(next.discogs.is_some());
        assert_eq!(first.settings, Settings::default(), "the running job's are unchanged");
        // A refused save keeps the services.
        let bad: IdentifySettingsUpdateDto = serde_json::from_str(r#"{"autoApplyThreshold":0.5}"#).unwrap();
        assert_eq!(state.save_settings(&bad).unwrap_err().kind, ErrorKind::Malformed);
        assert!(Arc::ptr_eq(&next, &state.engine().unwrap()));
    }

    /// A save whose second write fails (here `.env.identify` is read-only,
    /// which refuses the rename over it on Windows) still drops the cached
    /// services: the first write already changed the settings.
    #[cfg(windows)]
    #[test]
    fn a_save_that_fails_halfway_still_drops_the_cached_services() {
        let dir = tempfile::tempdir().unwrap();
        let state = isolated(dir.path());
        let env_file = dir.path().join(ENV_FILE);
        std::fs::write(&env_file, "ACOUSTID_API_KEY=sekrit-key\nMB_USER_AGENT=rbxport-test/1 ( a@b.c )\n").unwrap();
        let first = state.engine().unwrap();
        let mut permissions = std::fs::metadata(&env_file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&env_file, permissions.clone()).unwrap();
        let change: IdentifySettingsUpdateDto = serde_json::from_str(r#"{"autoApply":true,"discogsToken":"tok"}"#).unwrap();
        let error = state.save_settings(&change);
        #[allow(clippy::permissions_set_readonly_false, reason = "the test file must be deletable again")]
        permissions.set_readonly(false);
        std::fs::set_permissions(&env_file, permissions).unwrap();
        assert!(error.is_err(), "the key could not be written");
        let next = state.engine().unwrap();
        assert!(!Arc::ptr_eq(&first, &next), "not the services built from the old settings");
        assert!(next.settings.auto_apply, "the settings that were written are used");
    }

    #[test]
    fn a_job_starting_during_a_save_never_caches_the_old_settings() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(isolated(dir.path()));
        let keys: IdentifySettingsUpdateDto =
            serde_json::from_str(r#"{"acoustidKey":"sekrit-key","mbUserAgent":"rbxport-test/1 ( a@b.c )"}"#).unwrap();
        state.save_settings(&keys).unwrap();
        for round in 0..20 {
            let on = round % 2 == 0;
            let change: IdentifySettingsUpdateDto = serde_json::from_str(&format!(r#"{{"autoApply":{on}}}"#)).unwrap();
            let starter = {
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    for _ in 0..5 {
                        drop(state.engine().unwrap());
                    }
                })
            };
            state.save_settings(&change).unwrap();
            starter.join().unwrap();
            assert_eq!(state.engine().unwrap().settings.auto_apply, on, "round {round}");
        }
    }

    /// One second of silent 16-bit mono WAV.
    fn wav(path: &Path) {
        let rate = 44_100_u32;
        let data_len = rate * 2;
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16_u32.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2_u16.to_le_bytes());
        out.extend_from_slice(&16_u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        std::fs::write(path, out).unwrap();
    }

    /// Stores a review result for `audio`, linked to `track` as the job links it.
    fn identified(identify: &IdentifyState, audio: &Path, track: &str, changes: Vec<FieldChange>) -> ResultId {
        identify
            .with_store(|store| {
                let key = TrackKey::for_file(audio).map_err(store_error)?;
                let mut track_result = result(audio, Decision::Review);
                track_result.proposal = Some(Proposal::new(changes, None, true));
                track_result.artwork = None;
                let library = LibraryTrack { track_id: track.to_owned(), path: key.path.clone() };
                Ok(store.save_track_result(&key, &track_result, Some(&library), false).map_err(store_error)?.1)
            })
            .unwrap()
    }

    fn ticked(result: ResultId, fields: &[&str]) -> [ApplyItemDto; 1] {
        [ApplyItemDto { result_id: result, fields: fields.iter().map(|f| (*f).to_owned()).collect() }]
    }

    #[test]
    fn apply_writes_the_file_only_and_undo_puts_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("track.wav");
        wav(&audio);
        let original = std::fs::read(&audio).unwrap();

        let identify = IdentifyState::with_root(dir.path().join("identify"));
        let changes = vec![
            FieldChange { field: Field::Title, current: String::new(), proposed: "New Title (Extended Mix)".into() },
            FieldChange { field: Field::SubGenre, current: String::new(), proposed: "Tech House".into() },
            FieldChange { field: Field::Isrc, current: String::new(), proposed: "GBAAA1400001".into() },
            FieldChange { field: Field::CatalogNumber, current: String::new(), proposed: "CAT001".into() },
        ];
        let result = identified(&identify, &audio, "42", changes);
        let item = identify.queue(QueueFilter::NeedsReview, 0, 10).unwrap().items.remove(0);
        assert_eq!((item.result_id, item.track_id.as_str(), item.undoable), (result, "42", false));

        // A field the proposal lacks is refused alone, changing nothing.
        let items = [
            ApplyItemDto { result_id: result, fields: vec!["genre".into()] },
            ApplyItemDto { result_id: result, fields: vec!["title".into(), "subGenre".into(), "isrc".into(), "catalogNumber".into()] },
        ];
        let outcomes = identify.apply_items(&items);
        assert_eq!(outcomes.iter().map(|o| o.ok).collect::<Vec<_>>(), [false, true], "{outcomes:?}");
        let written = tags::read(&audio).unwrap().snapshot;
        assert_eq!(rbl_identify::pipeline::current(&written, Field::Title), "New Title (Extended Mix)");
        assert_eq!(rbl_identify::pipeline::current(&written, Field::CatalogNumber), "CAT001");
        assert_eq!(rbl_identify::pipeline::current(&written, Field::Isrc), "GBAAA1400001");
        let item = identify.detail(result).unwrap().item;
        assert_eq!((item.state.as_str(), item.undoable), ("applied", true));

        // Applied once: not again.
        assert!(!identify.apply_items(&items[1..])[0].ok);

        let undone = identify.undo_item(result);
        assert!(undone.ok, "{undone:?}");
        assert_eq!(std::fs::read(&audio).unwrap(), original, "the file byte for byte");
        assert_eq!(identify.detail(result).unwrap().item.state, "undone");
        assert!(!identify.undo_item(result).ok);
    }

    #[test]
    fn undo_of_a_write_recorded_with_library_values_restores_the_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("track.wav");
        wav(&audio);
        let original = std::fs::read(&audio).unwrap();
        let identify = IdentifyState::with_root(dir.path().join("identify"));
        let title = vec![FieldChange { field: Field::Title, current: String::new(), proposed: "Mine".into() }];
        let result = identified(&identify, &audio, "42", title.clone());
        // As an older build recorded it: library values before and after.
        identify
            .with_store(|store| {
                store.set_state(result, ReviewState::Accepted).map_err(store_error)?;
                let before = tags::read(&audio).map_err(store_error)?.snapshot;
                let library_before = r#"{"trackId":"42","values":[{"column":"Title","value":{"Text":""}}]}"#;
                let application =
                    store.begin_application(result, &title, None, &before, Some(library_before)).map_err(store_error)?;
                let edits = rbl_identify::write::plan(&before, &title).map_err(store_error)?;
                let after = rbl_identify::write::write(&audio, &before, &edits, None).map_err(store_error)?;
                let key = TrackKey::for_file(&audio).map_err(store_error)?;
                let library_after = r#"{"trackId":"42","values":[{"column":"Title","value":{"Text":"Mine"}}]}"#;
                store.complete_application_with(application, &key, Some(&after), Some(library_after)).map_err(store_error)
            })
            .unwrap();
        assert_ne!(std::fs::read(&audio).unwrap(), original);
        let undone = identify.undo_item(result);
        assert!(undone.ok, "{undone:?}");
        assert_eq!(std::fs::read(&audio).unwrap(), original);
    }

    #[test]
    fn undo_is_refused_when_the_file_changed_since_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("track.wav");
        wav(&audio);
        let identify = IdentifyState::with_root(dir.path().join("identify"));
        let title = vec![FieldChange { field: Field::Title, current: String::new(), proposed: "Mine".into() }];
        let result = identified(&identify, &audio, "42", title);
        let outcomes = identify.apply_items(&ticked(result, &["title"]));
        assert!(outcomes[0].ok, "{outcomes:?}");
        // Someone retags the file afterwards.
        let after = tags::read(&audio).unwrap().snapshot;
        let edits = rbl_identify::write::plan(
            &after,
            &[FieldChange { field: Field::Title, current: "Mine".into(), proposed: "By Hand".into() }],
        )
        .unwrap();
        rbl_identify::write::write(&audio, &after, &edits, None).unwrap();
        let undone = identify.undo_item(result);
        assert!(!undone.ok);
        assert!(undone.error.as_deref().unwrap_or_default().contains("changed since"), "{undone:?}");
        assert_eq!(identify.detail(result).unwrap().item.state, "applied");
    }

    #[test]
    fn a_job_imports_the_tracks_rekordbox_record_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let location = rbl_db::fixture::build(dir.path(), rbl_db::fixture::Shape::default()).unwrap();
        let db = rbl_db::Library::open(location, rbl_db::OpenMode::ReadOnly).unwrap();
        let (library, _) = rbl_index::load(&db).unwrap();
        let id = rbl_db::fixture::track_id(1);
        let isrcs = read_isrcs(&db, std::slice::from_ref(&id)).unwrap();
        let tracks = resolve_tracks(&library, vec![id.clone(), id.clone(), "no-such".into()], &isrcs, 1_800_000_000);
        assert_eq!(tracks.len(), 2, "each id once");
        assert!(tracks[1].rekordbox.is_none(), "a track the library cannot place has no record");
        let mut record = tracks[0].rekordbox.clone().unwrap();
        let row = library.row_of(&id).unwrap() as usize;
        assert_eq!((record.track_id.as_str(), record.title.as_str()), (id.as_str(), library.title.get(row)));
        assert_eq!(record.artist, library.artists.name(library.artist[row]));
        assert_eq!(record.imported_unix, 1_800_000_000);
        record.isrc = "GBAAA1400001".into();

        // Saved with the result, in the store, and shown with it.
        let files = tempfile::tempdir().unwrap();
        let state = IdentifyState::with_root(dir.path().join("identify"));
        let mut job = track(files.path(), "review.mp3", true);
        job.rekordbox = Some(record.clone());
        run(&state, &[job], &Fake::new(), &AtomicBool::new(false));
        let item = state.queue(QueueFilter::NeedsReview, 0, 10).unwrap().items.remove(0);
        let shown = state.detail(item.result_id).unwrap().rekordbox.unwrap();
        assert_eq!(shown.imported_at, "2027-01-15T08:00:00Z");
        assert_eq!((shown.title, shown.isrc, shown.year), (record.title, record.isrc, record.year));
        let json = serde_json::to_value(state.detail(item.result_id).unwrap()).unwrap();
        assert!(json["rekordbox"]["importedAt"].is_string() && json["item"]["source"] == "fingerprint", "{json}");
    }

    #[test]
    fn startup_removes_staged_files_an_interrupted_write_left_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("track.wav");
        wav(&audio);
        let original = std::fs::read(&audio).unwrap();
        let identify = IdentifyState::with_root(dir.path().join("identify"));
        let title = vec![FieldChange { field: Field::Title, current: String::new(), proposed: "Mine".into() }];
        let result = identified(&identify, &audio, "42", title.clone());
        // A write begun, then a crash between staging and the rename.
        identify
            .with_store(|store| {
                store.set_state(result, ReviewState::Accepted).map_err(store_error)?;
                let before = tags::read(&audio).map_err(store_error)?.snapshot;
                store.begin_application(result, &title, None, &before, None).map_err(store_error)
            })
            .unwrap();
        let leftovers = [".rbxport-tags-Ab12Cd.tmp", ".rbxport-tags-Ef34Gh.wav"];
        let keep = [".rbxport-tags-Ab12C.tmp", "other.tmp", ".rbxport-tags-Ab12Cd.mp3"];
        for name in leftovers.iter().chain(&keep) {
            std::fs::write(dir.path().join(name), b"staged").unwrap();
        }
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join(leftovers[0]), b"not beside an open write").unwrap();

        identify.reconcile_on_startup();
        for name in leftovers {
            assert!(!dir.path().join(name).exists(), "{name}");
        }
        for name in keep {
            assert!(dir.path().join(name).exists(), "{name}");
        }
        assert!(elsewhere.path().join(leftovers[0]).exists(), "only open applications' folders are cleaned");
        assert_eq!(std::fs::read(&audio).unwrap(), original);
        // The file was untouched, so the write was abandoned, as before.
        assert_eq!(identify.with_store(|s| s.open_applications().map_err(store_error)).unwrap(), []);
    }

    #[test]
    fn startup_records_a_write_that_landed_and_frees_one_it_cannot_tell() {
        let dir = tempfile::tempdir().unwrap();
        let identify = IdentifyState::with_root(dir.path().join("identify"));
        let title = vec![FieldChange { field: Field::Title, current: String::new(), proposed: "Mine".into() }];
        let begin = |audio: &Path, result: ResultId| {
            identify
                .with_store(|store| {
                    store.set_state(result, ReviewState::Accepted).map_err(store_error)?;
                    let before = tags::read(audio).map_err(store_error)?.snapshot;
                    store.begin_application(result, &title, None, &before, None).map_err(store_error)
                })
                .unwrap()
        };
        // Crash after the rename: the file holds exactly the write.
        let landed = dir.path().join("landed.wav");
        wav(&landed);
        let original = std::fs::read(&landed).unwrap();
        let landed_result = identified(&identify, &landed, "1", title.clone());
        begin(&landed, landed_result);
        let before = tags::read(&landed).unwrap().snapshot;
        rbl_identify::write::write(&landed, &before, &rbl_identify::write::plan(&before, &title).unwrap(), None).unwrap();
        // Crash, then someone else retagged the file: cannot be told.
        let other = dir.path().join("other.wav");
        wav(&other);
        let other_result = identified(&identify, &other, "2", title.clone());
        begin(&other, other_result);
        let before = tags::read(&other).unwrap().snapshot;
        let by_hand = [FieldChange { field: Field::Title, current: String::new(), proposed: "By Hand".into() }];
        rbl_identify::write::write(&other, &before, &rbl_identify::write::plan(&before, &by_hand).unwrap(), None).unwrap();
        let other_bytes = std::fs::read(&other).unwrap();

        identify.reconcile_on_startup();
        assert_eq!(identify.with_store(|s| s.open_applications().map_err(store_error)).unwrap(), []);
        let landed_item = identify.detail(landed_result).unwrap().item;
        assert_eq!((landed_item.state.as_str(), landed_item.undoable), ("applied", true));
        assert!(identify.undo_item(landed_result).ok);
        assert_eq!(std::fs::read(&landed).unwrap(), original);
        let other_detail = identify.detail(other_result).unwrap();
        assert_eq!((other_detail.item.state.as_str(), other_detail.write_in_progress), ("pending", false));
        assert_eq!(std::fs::read(&other).unwrap(), other_bytes, "never rewritten by reconcile");
    }

    /// A file another program holds exclusively at startup cannot be read:
    /// its write stays open (never closed as unknown) and the next start,
    /// with the file free, records it as written.
    #[cfg(windows)]
    #[test]
    fn startup_leaves_a_write_open_while_its_file_cannot_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let identify = IdentifyState::with_root(dir.path().join("identify"));
        let title = vec![FieldChange { field: Field::Title, current: String::new(), proposed: "Mine".into() }];
        let audio = dir.path().join("held.wav");
        wav(&audio);
        let result = identified(&identify, &audio, "1", title.clone());
        identify
            .with_store(|store| {
                store.set_state(result, ReviewState::Accepted).map_err(store_error)?;
                let before = tags::read(&audio).map_err(store_error)?.snapshot;
                store.begin_application(result, &title, None, &before, None).map_err(store_error)
            })
            .unwrap();
        // The write landed, then the crash.
        let before = tags::read(&audio).unwrap().snapshot;
        rbl_identify::write::write(&audio, &before, &rbl_identify::write::plan(&before, &title).unwrap(), None).unwrap();

        let held = hold(&audio, 0);
        identify.reconcile_on_startup();
        assert_eq!(identify.with_store(|s| s.open_applications().map_err(store_error)).unwrap().len(), 1);
        assert!(identify.detail(result).unwrap().write_in_progress);
        drop(held);

        identify.reconcile_on_startup();
        assert_eq!(identify.with_store(|s| s.open_applications().map_err(store_error)).unwrap(), []);
        let item = identify.detail(result).unwrap().item;
        assert_eq!((item.state.as_str(), item.undoable), ("applied", true));
    }

    /// Holds `path` open the way a player does, with `share` (`FILE_SHARE_*`).
    #[cfg(windows)]
    fn hold(path: &Path, share: u32) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new().read(true).share_mode(share).open(path).unwrap()
    }

    #[cfg(windows)]
    const FILE_SHARE_READ: u32 = 0x1;
    #[cfg(windows)]
    const FILE_SHARE_DELETE: u32 = 0x4;

    /// A player holding the file open, sharing read only or read and delete:
    /// Windows refuses the rename over it either way (an open handle blocks
    /// replacing the file even when it shares delete), so apply answers with
    /// the "open in another program" message, the file is unchanged, the
    /// write is abandoned (the result back to review, no write left open)
    /// and no staged file stays behind. Once closed, it goes through.
    #[cfg(windows)]
    #[test]
    fn a_file_a_player_holds_open_is_refused_and_left_as_it_was() {
        for share in [FILE_SHARE_READ, FILE_SHARE_READ | FILE_SHARE_DELETE] {
            let dir = tempfile::tempdir().unwrap();
            let audio = dir.path().join("track.wav");
            wav(&audio);
            let original = std::fs::read(&audio).unwrap();
            let identify = IdentifyState::with_root(dir.path().join("identify"));
            let title = vec![FieldChange { field: Field::Title, current: String::new(), proposed: "Mine".into() }];
            let result = identified(&identify, &audio, "42", title);

            let held = hold(&audio, share);
            let outcome = identify.apply_items(&ticked(result, &["title"])).remove(0);
            assert!(!outcome.ok, "share {share:#x}");
            let error = outcome.error.unwrap_or_default();
            assert!(error.contains("open in another program"), "share {share:#x}: {error}");
            assert_eq!(std::fs::read(&audio).unwrap(), original, "share {share:#x}: byte for byte");
            drop(held);
            assert_eq!(identify.detail(result).unwrap().item.state, "pending", "share {share:#x}");
            assert_eq!(identify.with_store(|s| s.open_applications().map_err(store_error)).unwrap(), []);
            let names: Vec<String> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(names.iter().all(|n| !n.starts_with(".rbxport-tags-")), "no staged file left: {names:?}");
            let applied = identify.apply_items(&ticked(result, &["title"])).remove(0);
            assert!(applied.ok, "share {share:#x}: {applied:?}");
        }
    }

    #[test]
    fn isrcs_are_read_in_chunks_for_every_id_asked() {
        let dir = tempfile::tempdir().unwrap();
        let location = rbl_db::fixture::build(dir.path(), rbl_db::fixture::Shape::default()).unwrap();
        // A copy of the disposable fixture, given ISRCs through a writable
        // connection of its own; the app reads it read-only.
        {
            let mut db = rbl_db::Library::open(location.clone(), rbl_db::OpenMode::ReadWrite).unwrap();
            let conn = db.connection_mut();
            for n in 1..=3 {
                let isrc = if n == 2 { " ".to_owned() } else { format!("GBAAA140000{n}") };
                conn.execute("UPDATE djmdContent SET ISRC = ?1 WHERE ID = ?2", [isrc, rbl_db::fixture::track_id(n)]).unwrap();
            }
        }
        let db = rbl_db::Library::open(location, rbl_db::OpenMode::ReadOnly).unwrap();
        // More ids than one chunk, the known ones at both ends.
        let mut ids: Vec<String> = (0..ISRC_CHUNK + 7).map(|n| format!("no-such-{n}")).collect();
        ids.insert(0, rbl_db::fixture::track_id(1));
        ids.push(rbl_db::fixture::track_id(2));
        ids.push(rbl_db::fixture::track_id(3));
        let isrcs = read_isrcs(&db, &ids).unwrap();
        assert_eq!(isrcs.len(), 2, "{isrcs:?}");
        assert_eq!(isrcs.get(&rbl_db::fixture::track_id(1)).map(String::as_str), Some("GBAAA1400001"));
        assert_eq!(isrcs.get(&rbl_db::fixture::track_id(3)).map(String::as_str), Some("GBAAA1400003"));
        assert!(read_isrcs(&db, &[]).unwrap().is_empty());
    }

    #[test]
    fn model_names_reach_the_frontend_in_camel_case_and_back() {
        assert_eq!(camel(&Field::CatalogNumber), "catalogNumber");
        assert_eq!(camel(&Field::MbRecordingId), "mbRecordingId");
        assert_eq!(camel(&ReviewState::AutoApplied), "autoApplied");
        assert_eq!(camel(&Decision::AutoApply), "autoApply");
        assert_eq!(parse_field("catalogNumber"), Some(Field::CatalogNumber));
        assert_eq!(parse_field("bpm"), None);
        assert_eq!(parse_field(""), None);
    }
}
