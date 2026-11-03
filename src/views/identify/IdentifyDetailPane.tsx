/**
 * One result, in full: the file's tags against the proposal, field by
 * field, with a tick for each field to write; the candidates the pipeline
 * weighed and why; and Accept (write the ticked fields), Reject, Edit and
 * Undo. Nothing is written until Accept: the spec's "never silently
 * overwrite" rule.
 *
 * A file another program holds open defers the write: the result then waits
 * with its ticked fields, says why, and offers Retry now and Cancel write. A
 * deferred write is a notice, not an error.
 *
 * While audio-file writes are off, Accept, Undo and Retry now are disabled
 * with the reason as their tooltip; Reject, Edit and Back to review only
 * change the identify store, so they stay.
 *
 * One way: Accept and Undo write the audio file's tags and nothing else.
 * The rekordbox record imported when the track was identified is shown
 * under a field it disagrees with, for reference; it is never written.
 */
import { useCallback, useEffect, useState } from "react";

import { getBackend } from "@/ipc/client";
import type { IdentifyDetail, IdentifyField, IdentifyReviewAction } from "@/ipc/types";
import { useTranslation } from "@/i18n";
import { errorMessage } from "@/lib/errorMessage";
import {
  canApply, changes, decisionLabel, defaultFields, fieldLabel, formatScore, rekordboxValue, reloadHint, sourceLabel,
  stateLabel, trackName, waitingLabel, waitingNotice, WRITES_OFF_NOTICE_ID,
} from "./identifyText";
import styles from "./IdentifyPanel.module.css";

/** A cover's size: tens of kilobytes, which `formatBytes` would round to 0.0 MB. */
function imageSize(bytes: number): string {
  return bytes >= 1024 * 1024 ? `${(bytes / 1024 / 1024).toFixed(1)} MB` : `${Math.max(1, Math.round(bytes / 1024))} KB`;
}

/** Fields whose proposal can be typed over. Artwork is a picture; the comment is only ever appended to. */
const EDITABLE = (field: IdentifyField) => field !== "artwork" && field !== "subGenre";

/** An answer whose write was deferred until its file is free: not a failure. */
function deferred(outcome: unknown): boolean {
  return !!outcome && typeof outcome === "object" && "deferred" in outcome && outcome.deferred === true;
}

/** An answer that says it failed, from a call that resolves rather than throws. */
function refused(outcome: unknown): string | null {
  if (deferred(outcome)) return null;
  if (outcome && typeof outcome === "object" && "ok" in outcome && outcome.ok === false) {
    return "error" in outcome && typeof outcome.error === "string" ? outcome.error : "";
  }
  return null;
}

export interface IdentifyDetailPaneProps {
  resultId: number;
  /** After a call of this pane's own changed the result. */
  onChanged: () => void;
  /** Writing to audio files is off, or not yet known to be on: nothing here may write. */
  writesOff?: boolean;
  /** Why, when writes are known to be off; shown as the write buttons' tooltip. */
  writesOffReason?: string | null;
}

export function IdentifyDetailPane({ resultId, onChanged, writesOff = false, writesOffReason = null }: IdentifyDetailPaneProps) {
  const t = useTranslation();
  const [detail, setDetail] = useState<IdentifyDetail | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [ticked, setTicked] = useState<ReadonlySet<IdentifyField>>(new Set());
  const [editing, setEditing] = useState<{ field: IdentifyField; value: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  const load = useCallback(async (keepTicks: boolean) => {
    try {
      const next = await (await getBackend()).identify.result(resultId);
      setDetail(next);
      setLoadError(null);
      setTicked((current) => {
        const fields = new Set(next.item.changes.map((c) => c.field));
        // A waiting write shows the fields it will write.
        if (next.item.waiting) return new Set(next.item.waiting.fields.filter((field) => fields.has(field)));
        return keepTicks
          ? new Set([...current].filter((field) => fields.has(field)))
          : new Set(defaultFields(next.item));
      });
    } catch (e) {
      setLoadError(errorMessage(e));
    }
  }, [resultId]);

  // A new result starts with every field that would change ticked.
  useEffect(() => {
    setDetail(null);
    setEditing(null);
    setError(null);
    setNote(null);
    void load(false);
  }, [load]);

  // A change from elsewhere — bulk accept, another window — re-reads it.
  useEffect(() => {
    let live = true;
    let stop: (() => void) | undefined;
    void getBackend().then((backend) => {
      if (!live) return;
      stop = backend.identify.onChanged(({ resultIds }) => {
        if (resultIds.includes(resultId)) void load(true);
      });
    });
    return () => {
      live = false;
      stop?.();
    };
  }, [resultId, load]);

  const run = useCallback(async (call: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    setNote(null);
    try {
      const outcome = await call();
      const outcomes: unknown[] = Array.isArray(outcome) ? outcome : [outcome];
      const why = refused(outcomes.find((o) => refused(o) !== null));
      if (why !== null) setError(why || t("The change could not be made."));
      const waiting = outcomes.filter(deferred).length;
      if (waiting > 0) setNote(waitingNotice(waiting, t));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
      await load(true);
      onChanged();
    }
  }, [load, onChanged, t]);

  if (loadError !== null && detail === null) {
    return <div className={styles.detailEmpty} role="alert">{loadError}</div>;
  }
  if (detail === null) return <div className={styles.detailEmpty}>{t("Loading…")}</div>;

  const { item, rekordbox } = detail;
  const written = item.state === "applied" || item.state === "autoApplied" || item.state === "undone";
  // A waiting write is settled by Retry now or Cancel write first.
  const waiting = item.waiting !== null;
  const writable = canApply(item.state) && !detail.writeInProgress && !waiting;
  const lockTitle = writesOff && writesOffReason !== null ? writesOffReason : undefined;
  const lockNote = lockTitle !== undefined ? WRITES_OFF_NOTICE_ID : undefined;
  const reviewable = (item.state === "pending" || item.state === "accepted") && !detail.writeInProgress && !waiting;
  const setState = (action: IdentifyReviewAction) =>
    void run(async () => (await getBackend()).identify.setState(item.resultId, action));
  const accept = () => void run(async () =>
    (await getBackend()).identify.apply([{ resultId: item.resultId, fields: [...ticked] }]));
  const undo = () => void run(async () => (await getBackend()).identify.undo(item.resultId));
  // Retrying tries every waiting write; this pane answers for its own.
  const retry = () => void run(async () =>
    (await (await getBackend()).identify.retryWaiting()).filter((outcome) => outcome.resultId === item.resultId));
  const cancelWaiting = () => void run(async () => (await getBackend()).identify.cancelWaiting(item.resultId));
  const saveEdit = () => {
    if (!editing) return;
    const edit = editing;
    setEditing(null);
    void run(async () => (await getBackend()).identify.edit(item.resultId, [{ field: edit.field, proposed: edit.value }]));
  };
  const toggle = (field: IdentifyField, on: boolean) => setTicked((current) => {
    const next = new Set(current);
    if (on) next.add(field);
    else next.delete(field);
    return next;
  });

  return (
    <section className={styles.detail} aria-label={t("Identification result")}>
      <header className={styles.detailHeader}>
        <h3 className={styles.detailTitle}>{trackName(item)}</h3>
        <p className={styles.detailPath} title={item.path}>{item.path}</p>
        <p className={styles.detailMeta}>
          <span className={styles.badge} data-state={item.state}>{stateLabel(item.state, t)}</span>
          {waiting ? <span className={styles.badge} data-state="waiting">{waitingLabel(t)}</span> : null}
          <span>{decisionLabel(item.decision, t)}</span>
          <span>{t("Score {score}", { score: formatScore(item.score) })}</span>
        </p>
      </header>

      {detail.writeInProgress ? (
        <p className={styles.notice} role="status">{t("Tags are being written to this file. Its review cannot change until the write is finished.")}</p>
      ) : null}

      {item.waiting !== null ? (
        <section className={styles.waiting} aria-label={t("Waiting for file")}>
          <p className={styles.notice}>
            {t("The file was open in another program, such as a player, so its tags were not written.")}
            {" "}{t("The ticked fields will be written automatically the next time rbxport starts or closes, or when you choose {retry}. Nothing is written if the file changes in the meantime.", { retry: t("Retry now") })}
          </p>
          {item.waiting.reason ? <p className={styles.hint}>{item.waiting.reason}</p> : null}
          <div className={styles.actions}>
            <button type="button" className={styles.button} disabled={busy || writesOff} title={lockTitle}
              aria-describedby={lockNote} onClick={retry}>{t("Retry now")}</button>
            <button type="button" className={styles.button} disabled={busy} onClick={cancelWaiting}>{t("Cancel write")}</button>
          </div>
        </section>
      ) : null}

      {item.changes.length > 0 ? (
        <div className={styles.diffScroll}>
          <table className={styles.diff}>
            <thead>
              <tr>
                <th scope="col"><span className={styles.visuallyHidden}>{t("Write")}</span></th>
                <th scope="col">{t("Field")}</th>
                <th scope="col">{t("Current")}</th>
                <th scope="col">{t("Proposed")}</th>
                <th scope="col"><span className={styles.visuallyHidden}>{t("Edit")}</span></th>
              </tr>
            </thead>
            <tbody>
              {item.changes.map((change) => {
                const label = fieldLabel(change.field, t);
                const same = !changes(change);
                const inRekordbox = rekordbox ? rekordboxValue(change.field, rekordbox) : null;
                return (
                  <tr key={change.field} data-same={same || undefined}>
                    <td>
                      <input
                        type="checkbox"
                        aria-label={t("Write {field}", { field: label })}
                        checked={ticked.has(change.field)}
                        disabled={!writable || busy || same}
                        onChange={(event) => toggle(change.field, event.target.checked)}
                      />
                    </td>
                    <th scope="row" className={styles.fieldName}>{label}</th>
                    <td className={styles.current}>
                      {change.current || <span className={styles.none}>{t("(empty)")}</span>}
                      {inRekordbox !== null && inRekordbox !== change.current ? (
                        <span className={styles.rekordbox}>
                          {t("rekordbox: {value}", { value: inRekordbox || t("(empty)") })}
                        </span>
                      ) : null}
                    </td>
                    <td className={styles.proposed}>
                      {editing?.field === change.field ? (
                        <form className={styles.editForm} onSubmit={(event) => { event.preventDefault(); saveEdit(); }}>
                          <input
                            className={styles.input}
                            aria-label={t("Proposed {field}", { field: label })}
                            value={editing.value}
                            // The edit was asked for; typing goes here.
                            autoFocus
                            onChange={(event) => setEditing({ field: change.field, value: event.target.value })}
                            onKeyDown={(event) => {
                              if (event.key === "Escape") {
                                event.stopPropagation();
                                setEditing(null);
                              }
                            }}
                          />
                          <button type="submit" className={styles.small}>{t("Save")}</button>
                          <button type="button" className={styles.small} onClick={() => setEditing(null)}>{t("Cancel")}</button>
                        </form>
                      ) : (
                        change.proposed || <span className={styles.none}>{t("(empty)")}</span>
                      )}
                    </td>
                    <td>
                      {EDITABLE(change.field) && editing?.field !== change.field ? (
                        <button
                          type="button"
                          className={styles.small}
                          aria-label={t("Edit {field}", { field: label })}
                          disabled={!reviewable || busy}
                          onClick={() => setEditing({ field: change.field, value: change.proposed })}
                        >
                          {t("Edit")}
                        </button>
                      ) : null}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <p className={styles.none}>{t("Nothing to change.")}</p>
      )}

      {item.changes.some((c) => c.field === "subGenre") ? (
        <p className={styles.hint}>{t("The sub-genre is added to the end of the comment; what the comment already says is kept.")}</p>
      ) : null}

      {detail.artwork ? (
        <p className={styles.hint}>
          {t("Proposed artwork: {mime}, {size}, from {source}", {
            mime: detail.artwork.mime,
            size: imageSize(detail.artwork.bytes),
            source: detail.artwork.source === "discogs" ? "Discogs" : "Cover Art Archive",
          })}
        </p>
      ) : null}

      {writable && item.changes.length > 0 ? (
        <p className={styles.hint}>{t("Only the tags in the audio files are written; the rekordbox library is not changed.")}</p>
      ) : null}
      {written ? <p className={styles.hint} role="note">{reloadHint(t)}</p> : null}

      <div className={styles.actions}>
        <button
          type="button"
          className={styles.button}
          disabled={!writable || busy || writesOff || ticked.size === 0}
          title={writable ? lockTitle : undefined}
          aria-describedby={writable ? lockNote : undefined}
          onClick={accept}
        >
          {t("Accept")}
        </button>
        <button type="button" className={styles.button} disabled={!reviewable || busy} onClick={() => setState("reject")}>
          {t("Reject")}
        </button>
        {item.state === "rejected" || item.state === "undone" ? (
          <button type="button" className={styles.button} disabled={busy || detail.writeInProgress} onClick={() => setState("pending")}>
            {t("Back to review")}
          </button>
        ) : null}
        {item.undoable ? (
          <button type="button" className={styles.button} disabled={busy || writesOff} title={lockTitle}
            aria-describedby={lockNote} onClick={undo}>
            {t("Undo")}
          </button>
        ) : null}
      </div>
      {note !== null ? <p className={styles.note} role="status">{note}</p> : null}
      {error !== null ? <p className={styles.error} role="alert">{error}</p> : null}

      {detail.warnings.length > 0 ? (
        <section aria-label={t("Warnings")}>
          <h4 className={styles.subhead}>{t("Warnings")}</h4>
          <ul className={styles.warnings}>
            {detail.warnings.map((warning) => <li key={warning}>{warning}</li>)}
          </ul>
        </section>
      ) : null}

      {detail.enrichment ? (
        <p className={styles.hint}>
          {t("Discogs: {release} · {styles}", {
            release: [detail.enrichment.releaseTitle, detail.enrichment.label, detail.enrichment.catalogNumber]
              .filter(Boolean).join(" · "),
            styles: detail.enrichment.styles.join(", ") || "–",
          })}
        </p>
      ) : null}

      <section aria-label={t("Candidates")}>
        <h4 className={styles.subhead}>{t("Candidates")}</h4>
        {detail.candidates.length === 0 ? <p className={styles.none}>{t("No candidates.")}</p> : (
          <ol className={styles.candidates}>
            {detail.candidates.map((candidate, index) => {
              const release = candidate.releases[0];
              return (
                // The recording id repeats when one recording is found twice.
                <li key={`${candidate.recordingMbid}-${index}`} className={styles.candidate} data-rejected={candidate.rejected !== null || undefined}>
                  <span className={styles.candidateScore}>{formatScore(candidate.score)}</span>
                  <span className={styles.candidateName}>
                    {[candidate.artist, candidate.title].filter(Boolean).join(" – ")}
                    {candidate.disambiguation ? <span className={styles.none}> ({candidate.disambiguation})</span> : null}
                  </span>
                  <span className={styles.candidateMeta}>
                    {sourceLabel(candidate.source, t)}
                    {release ? ` · ${[release.title, release.label, release.catalogNumber, release.date].filter(Boolean).join(" · ")}` : ""}
                  </span>
                  {candidate.rejected !== null ? (
                    <span className={styles.candidateMeta}>{t("Rejected: {reason}", { reason: candidate.rejected })}</span>
                  ) : null}
                </li>
              );
            })}
          </ol>
        )}
      </section>
    </section>
  );
}
