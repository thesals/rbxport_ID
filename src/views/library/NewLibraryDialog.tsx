import { useEffect, useRef, useState } from "react";
import type { ImportProgress, ImportSummary } from "@/ipc/types";
import styles from "./NewLibraryDialog.module.css";

/**
 * The first start's one question, before rbxport has a library of its own:
 * import rekordbox's library (when there is one), make an empty one, or
 * quit. Nothing else in the window works without a library, so there is no
 * other way out; Escape does nothing. The library is rbxport's, in its own
 * folder; rekordbox's is only ever read, by the import.
 */
export function NewLibraryDialog({ masterDb, rekordbox, progress, onCreate, onImport, onQuit }: {
  masterDb: string;
  /** rekordbox's library, when there is one to import. */
  rekordbox: string | null;
  /** How far an import is, while one runs. */
  progress: ImportProgress | null;
  onCreate: () => Promise<void>;
  onImport: () => Promise<void>;
  onQuit: () => void;
}) {
  const [busy, setBusy] = useState<"creating" | "importing" | null>(null);
  const [error, setError] = useState("");
  const dialog = useModal();

  const run = (kind: "creating" | "importing", action: () => Promise<void>) => {
    setBusy(kind);
    setError("");
    action().catch((e: unknown) => {
      setBusy(null);
      setError(errorText(e, kind));
    });
  };

  return (
    <dialog ref={dialog} className={styles.dialog} aria-labelledby="new-library-title"
      aria-describedby="new-library-text"
      onCancel={event => event.preventDefault()}
      onKeyDown={event => event.stopPropagation()}>
      <form onSubmit={event => { event.preventDefault(); if (busy === null) run("creating", onCreate); }}>
        <h2 id="new-library-title" className={styles.title}>Welcome to rbxport</h2>
        <p id="new-library-text" className={styles.text}>
          {rekordbox === null
            ? "rbxport keeps its own library, separate from rekordbox. Create a new, empty library to start. It will be made here:"
            : "rbxport keeps its own library, separate from rekordbox. Import your rekordbox library into it, or start with a new, empty library. rekordbox's library is only read and stays as it is. rbxport's library will be made here:"}
        </p>
        <p className={styles.path} title={masterDb}>{masterDb}</p>
        {rekordbox !== null ? (
          <p className={styles.source} title={rekordbox}>Found rekordbox's library: {rekordbox}</p>
        ) : null}
        {busy === "importing" ? (
          <p className={styles.progress} role="status">{progressText(progress)}</p>
        ) : null}
        {error ? <p className={styles.error} role="alert">{error}</p> : null}
        <div className={styles.buttons}>
          {rekordbox !== null ? (
            <button type="button" disabled={busy !== null} autoFocus
              onClick={() => run("importing", onImport)}>
              {busy === "importing" ? "Importing…" : "Import from rekordbox"}
            </button>
          ) : null}
          <button type="submit" disabled={busy !== null} autoFocus={rekordbox === null}>
            {busy === "creating" ? "Creating…" : "Create a New Library"}
          </button>
          <button type="button" onClick={onQuit} disabled={busy !== null}>Quit</button>
        </div>
      </form>
    </dialog>
  );
}

/** What the import did, once it is done. */
export function ImportSummaryDialog({ summary, onClose }: { summary: ImportSummary; onClose: () => void }) {
  const dialog = useModal();
  const shown = summary.problems.slice(0, 10);
  return (
    <dialog ref={dialog} className={styles.dialog} aria-labelledby="import-summary-title"
      onCancel={event => { event.preventDefault(); onClose(); }}
      onKeyDown={event => event.stopPropagation()}>
      <form onSubmit={event => { event.preventDefault(); onClose(); }}>
        <h2 id="import-summary-title" className={styles.title}>Import Complete</h2>
        <p className={styles.text}>
          Imported {count(summary.tracks, "track")}, {count(summary.playlists, "playlist")},{" "}
          {count(summary.cues, "cue")} and {count(summary.historySessions, "history session")}.
          Copied {count(summary.filesCopied, "analysis and artwork file")} ({sizeText(summary.bytesCopied)}).
        </p>
        <p className={summary.sourceUnchanged ? styles.text : styles.error}>
          {summary.sourceUnchanged
            ? "rekordbox's library is unchanged."
            : "rekordbox changed its library after it was copied. The import is whole, but holds the library as it was when copied."}
        </p>
        {summary.problems.length > 0 ? (
          <div className={styles.error} role="alert">
            <p>{count(summary.problems.length, "file")} could not be copied. Those tracks can be analysed again:</p>
            <ul className={styles.problems}>
              {shown.map(problem => <li key={problem}>{problem}</li>)}
              {summary.problems.length > shown.length ? <li>…and {summary.problems.length - shown.length} more (see the log)</li> : null}
            </ul>
          </div>
        ) : null}
        {summary.setAside !== null ? (
          <p className={styles.text}>A folder that was where the library goes was moved, not deleted: {summary.setAside}</p>
        ) : null}
        <p className={styles.path} title={summary.log}>Import log: {summary.log}</p>
        <div className={styles.buttons}>
          <button type="submit" autoFocus>Open Library</button>
        </div>
      </form>
    </dialog>
  );
}

function useModal() {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const element = dialog.current;
    element?.showModal();
    return () => element?.close();
  }, []);
  return dialog;
}

function progressText(progress: ImportProgress | null): string {
  switch (progress?.stage) {
    case undefined:
    case "checking": return "Checking rekordbox's library…";
    case "copyingDatabase": return "Copying the library database…";
    // A repeat import's own, which this dialog never runs.
    case "comparing":
    case "merging": return "Importing…";
    case "copyingFiles":
      return `Copying analysis files and artwork… ${progress.done.toLocaleString()} of ${progress.total.toLocaleString()}`;
    case "recording": return "Recording what was imported…";
    case "finishing": return "Checking that rekordbox's library is unchanged…";
  }
}

function count(n: number, noun: string): string {
  return `${n.toLocaleString()} ${noun}${n === 1 ? "" : "s"}`;
}

function sizeText(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(2)} GB`;
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(1)} MB`;
  return `${Math.round(bytes / 1e3).toLocaleString()} KB`;
}

/** The backend's message when it sent one, which says what went wrong. */
function errorText(e: unknown, kind: "creating" | "importing"): string {
  if (typeof e === "object" && e !== null && "message" in e && typeof e.message === "string") return e.message;
  if (typeof e === "string") return e;
  return kind === "importing" ? "Could not import rekordbox's library." : "Could not create the library.";
}
