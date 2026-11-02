/**
 * Track identification's review queue (fork-only;
 * `docs/identify/IDENTIFY_SPEC.md`), drawn over the window as the app's other
 * secondary views are and loaded only when first opened.
 *
 * At the top, the background job — how far it is, the track it is on, how
 * the finished ones came out, and Cancel. The job belongs to the backend, so
 * closing this panel leaves it running and reopening it picks it up. Below,
 * the queue under one of four filters, a window of rows at a time, and the
 * chosen result in full beside it. Bulk accept marks every pending result at
 * or above a threshold accepted; Apply accepted then writes them, each with
 * every field that would change.
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

import { getBackend } from "@/ipc/client";
import type { IdentifyApplyItem, IdentifyFilter, IdentifyQueueItem } from "@/ipc/types";
import { useTranslation } from "@/i18n";
import { errorMessage } from "@/lib/errorMessage";
import { visibleWindow } from "@/lib/virtual";
import type { IdentifyJob } from "@/store/useIdentifyJob";
import { useElementSize } from "@/store/useElementSize";
import { IdentifyDetailPane } from "./IdentifyDetailPane";
import {
  FILTERS, decisionLabel, defaultFields, filterLabel, formatScore, stateLabel, trackName, type Translate,
} from "./identifyText";
import { useIdentifyQueue } from "./useIdentifyQueue";
import styles from "./IdentifyPanel.module.css";

const ROW_H = 44;
export const DEFAULT_THRESHOLD = 0.92;
/** The most rows the store answers in one queue call. */
const MAX_ROWS = 128;
/** Results written per apply call, so one call never holds the edit gate for long. */
const APPLY_BATCH = 32;

export interface IdentifyPanelProps {
  job: IdentifyJob;
  readOnly: boolean;
  readOnlyReason: string;
  onClose: () => void;
}

function jobSummary(job: IdentifyJob, t: Translate): string {
  const p = job.progress;
  if (!p) return t("No identification has run yet.");
  switch (p.state) {
    case "running": return t("Identifying {done} of {total}", { done: p.done, total: p.total });
    case "done": return t("Identified {done} of {total}", { done: p.done, total: p.total });
    case "cancelled": return t("Cancelled after {done} of {total}", { done: p.done, total: p.total });
    case "failed": return p.message ?? t("Identification failed.");
  }
}

export function IdentifyPanel({ job, readOnly, readOnlyReason, onClose }: IdentifyPanelProps) {
  const t = useTranslation();
  const [filter, setFilter] = useState<IdentifyFilter>("needsReview");
  const queue = useIdentifyQueue(filter);
  const [selected, setSelected] = useState<{ id: number; index: number } | null>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [listRef, listSize] = useElementSize<HTMLDivElement>();
  const list = useRef<HTMLDivElement | null>(null);
  const [threshold, setThreshold] = useState(DEFAULT_THRESHOLD);
  const [bulkNote, setBulkNote] = useState<{ text: string; failed: boolean } | null>(null);
  const [bulkBusy, setBulkBusy] = useState(false);
  const window_ = visibleWindow(scrollTop, listSize.height || 480, ROW_H, queue.total ?? 0);
  const { show } = queue;

  useEffect(() => { show(window_.start, window_.end); }, [show, window_.start, window_.end]);

  // A new filter is a new list, read from the top.
  useEffect(() => {
    setSelected(null);
    setScrollTop(0);
    if (list.current) list.current.scrollTop = 0;
  }, [filter]);

  // Focus starts in the list, so the arrows work at once; it goes back to
  // where it was when the panel closes.
  useEffect(() => {
    const previous = document.activeElement;
    list.current?.focus();
    return () => {
      if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
    };
  }, []);

  const setListRef = useCallback((element: HTMLDivElement | null) => {
    list.current = element;
    listRef(element);
  }, [listRef]);

  // Keep the chosen row in view as the arrows move it.
  useLayoutEffect(() => {
    const element = list.current;
    if (!element || !selected) return;
    const top = selected.index * ROW_H;
    if (top < element.scrollTop) element.scrollTop = top;
    else if (top + ROW_H > element.scrollTop + element.clientHeight) element.scrollTop = top + ROW_H - element.clientHeight;
  }, [selected]);

  const pick = (index: number) => {
    const row = queue.rowAt(index);
    if (row) setSelected({ id: row.resultId, index });
  };

  const onListKey = (event: React.KeyboardEvent) => {
    const count = queue.total ?? 0;
    if (count === 0) return;
    const at = selected?.index ?? -1;
    const step = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1
      : event.key === "PageDown" ? Math.max(1, Math.floor((listSize.height || ROW_H) / ROW_H))
      : event.key === "PageUp" ? -Math.max(1, Math.floor((listSize.height || ROW_H) / ROW_H))
      : event.key === "Home" ? -count : event.key === "End" ? count : 0;
    if (step === 0) return;
    event.preventDefault();
    pick(Math.min(count - 1, Math.max(0, at + step)));
  };

  const acceptAbove = async () => {
    setBulkBusy(true);
    setBulkNote(null);
    try {
      const count = await (await getBackend()).identify.acceptAbove(threshold);
      setBulkNote({ text: count === 1 ? t("1 result accepted.") : t("{count} results accepted.", { count }), failed: false });
      queue.refresh();
    } catch (e) {
      setBulkNote({ text: errorMessage(e), failed: true });
    } finally {
      setBulkBusy(false);
    }
  };

  const applyAccepted = async () => {
    setBulkBusy(true);
    setBulkNote(null);
    try {
      const backend = await getBackend();
      // Every accepted result, read a page at a time.
      const accepted: IdentifyQueueItem[] = [];
      for (let offset = 0; ; offset += MAX_ROWS) {
        const page = await backend.identify.queue("all", offset, MAX_ROWS);
        accepted.push(...page.items.filter((item) => item.state === "accepted"));
        if (page.items.length === 0 || offset + page.items.length >= page.total) break;
      }
      const items: IdentifyApplyItem[] = accepted
        .map((item) => ({ resultId: item.resultId, fields: defaultFields(item) }))
        .filter((item) => item.fields.length > 0);
      let written = 0;
      let failed = 0;
      let firstError: string | null = null;
      for (let at = 0; at < items.length; at += APPLY_BATCH) {
        for (const outcome of await backend.identify.apply(items.slice(at, at + APPLY_BATCH))) {
          if (outcome.ok) written += 1;
          else {
            failed += 1;
            firstError ??= outcome.error;
          }
        }
      }
      const text = items.length === 0
        ? t("No accepted results to apply.")
        : failed === 0
          ? t("{count} applied.", { count: written })
          : t("{count} applied, {failed} failed: {reason}", { count: written, failed, reason: firstError ?? "" });
      setBulkNote({ text, failed: failed > 0 });
    } catch (e) {
      setBulkNote({ text: errorMessage(e), failed: true });
    } finally {
      setBulkBusy(false);
      queue.refresh();
    }
  };

  const progress = job.progress;
  const percent = progress && progress.total > 0 ? Math.floor(progress.done / progress.total * 100) : 0;
  const rows: React.ReactNode[] = [];
  for (let index = window_.start; index < window_.end; index++) {
    const row = queue.rowAt(index);
    const chosen = row !== undefined && row.resultId === selected?.id;
    rows.push(
      <div
        key={index}
        id={`identify-row-${index}`}
        role="option"
        aria-selected={chosen}
        className={styles.row}
        style={{ top: index * ROW_H }}
        onClick={() => pick(index)}
      >
        {row ? (
          <>
            <span className={styles.rowName}>{trackName(row)}</span>
            <span className={styles.rowMeta}>
              <span className={styles.rowScore}>{formatScore(row.score)}</span>
              <span className={styles.badge} data-state={row.state}>{stateLabel(row.state, t)}</span>
              <span>{decisionLabel(row.decision, t)}</span>
            </span>
          </>
        ) : <span className={styles.none}>{t("Loading…")}</span>}
      </div>,
    );
  }

  return (
    <div className={styles.backdrop} role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div
        className={styles.window}
        role="dialog"
        aria-modal="true"
        aria-label={t("Identify Tracks")}
        // The shell's shortcuts listen on the window: arrows there would move
        // the track list under this panel, as they would under the analysis dialog.
        onKeyDown={(event) => {
          event.stopPropagation();
          if (event.key === "Escape") onClose();
        }}
      >
        <div className={styles.titlebar}>
          <span>{t("Identify Tracks")}</span>
          <button type="button" className={styles.close} aria-label={t("Close")} onClick={onClose}>×</button>
        </div>

        <section className={styles.job} aria-label={t("Identification job")}>
          <div className={styles.jobLine}>
            <span role="status">{jobSummary(job, t)}</span>
            {job.running ? (
              <button type="button" className={styles.small} onClick={job.cancel}>{t("Cancel")}</button>
            ) : null}
          </div>
          {progress ? (
            <>
              <progress className={styles.progress} aria-label={t("Identification progress")} max={100} value={percent} />
              {progress.currentTitle ? (
                <div className={styles.jobCurrent} title={progress.currentPath}>{progress.currentTitle}</div>
              ) : null}
              <div className={styles.jobCounts}>
                {t("{identified} auto-apply · {review} to review · {unidentified} unidentified · {failed} failed", {
                  identified: progress.identified, review: progress.review,
                  unidentified: progress.unidentified, failed: progress.failed,
                })}
              </div>
            </>
          ) : null}
          {job.error !== null ? <p className={styles.error} role="alert">{job.error}</p> : null}
        </section>

        <div className={styles.toolbar}>
          <div className={styles.filters} role="tablist" aria-label={t("Show")}>
            {FILTERS.map((value) => (
              <button
                key={value}
                type="button"
                role="tab"
                aria-selected={filter === value}
                className={styles.filter}
                onClick={() => setFilter(value)}
              >
                {filterLabel(value, t)}
              </button>
            ))}
          </div>
          <span className={styles.spacer} />
          <label className={styles.threshold}>
            <span>{t("Accept all scoring at least")}</span>
            <input
              type="range" min={0.5} max={1} step={0.01} value={threshold}
              aria-label={t("Accept threshold")}
              onChange={(event) => setThreshold(Number(event.target.value))}
            />
            <input
              type="number" min={0.5} max={1} step={0.01} value={threshold}
              className={styles.number}
              aria-label={t("Accept threshold value")}
              onChange={(event) => {
                const value = Number(event.target.value);
                if (Number.isFinite(value)) setThreshold(Math.min(1, Math.max(0.5, value)));
              }}
            />
          </label>
          <button type="button" className={styles.small} disabled={bulkBusy} onClick={() => void acceptAbove()}>
            {t("Accept above")}
          </button>
          <button
            type="button"
            className={styles.small}
            disabled={bulkBusy || readOnly}
            title={readOnly ? readOnlyReason : undefined}
            onClick={() => void applyAccepted()}
          >
            {t("Apply accepted")}
          </button>
        </div>
        {bulkNote ? (
          <p className={bulkNote.failed ? styles.error : styles.note} role={bulkNote.failed ? "alert" : "status"}>{bulkNote.text}</p>
        ) : null}

        <div className={styles.body}>
          <div
            ref={setListRef}
            className={styles.list}
            role="listbox"
            tabIndex={0}
            aria-label={filterLabel(filter, t)}
            aria-activedescendant={selected ? `identify-row-${selected.index}` : undefined}
            onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}
            onKeyDown={onListKey}
          >
            <div className={styles.listInner} style={{ height: (queue.total ?? 0) * ROW_H }}>{rows}</div>
            {queue.total === 0 ? <p className={styles.empty}>{t("Nothing here.")}</p> : null}
            {queue.error !== null ? <p className={styles.error} role="alert">{queue.error}</p> : null}
          </div>
          <div className={styles.detailColumn}>
            {selected ? (
              <IdentifyDetailPane
                resultId={selected.id}
                readOnly={readOnly}
                readOnlyReason={readOnlyReason}
                onChanged={queue.refresh}
              />
            ) : (
              <p className={styles.detailEmpty}>{t("Choose a result to review it.")}</p>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
