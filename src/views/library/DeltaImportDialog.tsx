import { useEffect, useRef } from "react";
import { useTranslation } from "@/i18n";
import type { DeltaImport, DeltaPlaylistLine, DeltaTrackLine, ImportProgress } from "@/ipc/types";
import styles from "./NewLibraryDialog.module.css";
import own from "./DeltaImportDialog.module.css";

/** How many tracks a section names before it says how many more. */
const SHOWN = 50;

/**
 * File › Import from rekordbox…, into the library rbxport already keeps:
 * how far it is while it runs, then what it did. Nothing in it can be
 * cancelled once started (the library is written in one transaction), so
 * the dialog closes only when it is done or failed.
 */
export function DeltaImportDialog({ progress, report, error, onClose }: {
  progress: ImportProgress | null;
  report: DeltaImport | null;
  error: string | null;
  onClose: () => void;
}) {
  const t = useTranslation();
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const element = dialog.current;
    element?.showModal();
    return () => element?.close();
  }, []);
  const done = report !== null || error !== null;

  return (
    <dialog ref={dialog} className={`${styles.dialog} ${own.wide}`} aria-labelledby="delta-import-title"
      onCancel={event => { event.preventDefault(); if (done) onClose(); }}
      onKeyDown={event => event.stopPropagation()}>
      <form onSubmit={event => { event.preventDefault(); if (done) onClose(); }}>
        <h2 id="delta-import-title" className={styles.title}>
          {report !== null ? t("Import Complete") : t("Import from rekordbox")}
        </h2>
        {!done ? (
          <>
            <p className={styles.text}>
              {t("Only what rbxport doesn't have is taken. Tracks you've edited in rbxport keep their tags, and nothing is removed. rekordbox's library is only read.")}
            </p>
            <p className={styles.progress} role="status">{progressText(progress, t)}</p>
          </>
        ) : null}
        {error !== null ? <p className={styles.error} role="alert">{error}</p> : null}
        {report !== null ? <Report report={report} /> : null}
        <div className={styles.buttons}>
          <button type="submit" disabled={!done} autoFocus>{t("Close")}</button>
        </div>
      </form>
    </dialog>
  );
}

function Report({ report }: { report: DeltaImport }) {
  const t = useTranslation();
  const nothing = report.newTracks.length === 0 && report.updated.length === 0 && report.filled.length === 0
    && report.playlistsCreated.length === 0 && report.playlistsAppended.length === 0 && report.historySessions === 0;
  return (
    <>
      <p className={styles.text}>
        {nothing ? t("rbxport already had everything new in rekordbox's library.") : summaryLine(report, t)}
      </p>
      <p className={report.sourceUnchanged ? styles.text : styles.error}>
        {report.sourceUnchanged
          ? t("rekordbox's library is unchanged.")
          : t("rekordbox changed its library after it was copied. The import used the library as it was when copied.")}
      </p>
      <div className={own.sections}>
        <Tracks title={t("New tracks")} lines={report.newTracks} />
        <Tracks title={t("Tags updated from rekordbox")} lines={report.updated} />
        {report.skipped.length > 0 ? (
          <details className={own.section}>
            <summary>{t("Edited in rbxport, so rekordbox's tags were not taken")} ({report.skipped.length.toLocaleString()})</summary>
            <ul className={own.list}>
              {report.skipped.slice(0, SHOWN).map(skipped => (
                <li key={skipped.track.id}>
                  {skipped.track.name}
                  {skipped.linkedByPath ? <span className={own.note}> · {t("matched by file path")}</span> : null}
                  <ul className={own.fields}>
                    {skipped.fields.map(field => (
                      <li key={field.field}>
                        {fieldName(field.field, t)}: <span className={own.ours}>{shown(field.rbxport, t)}</span>
                        {" "}{t("(rekordbox: {value})", { value: shown(field.rekordbox, t) })}
                      </li>
                    ))}
                  </ul>
                </li>
              ))}
              <More total={report.skipped.length} />
            </ul>
          </details>
        ) : null}
        {report.filled.length > 0 ? (
          <details className={own.section}>
            <summary>{t("Given what rbxport didn't have")} ({report.filled.length.toLocaleString()})</summary>
            <ul className={own.list}>
              {report.filled.slice(0, SHOWN).map(filled => (
                <li key={filled.track.id}>
                  {filled.track.name}: {filled.what.map(kind => fillName(kind, t)).join(", ")}
                </li>
              ))}
              <More total={report.filled.length} />
            </ul>
          </details>
        ) : null}
        <Tracks title={t("Matched by file path")} lines={report.pathMatches} />
        <Playlists title={t("Playlists made")} lines={report.playlistsCreated} />
        <Playlists title={t("Playlists with tracks added")} lines={report.playlistsAppended} />
        <Playlists title={t("Playlists changed in rbxport, so nothing was added")} lines={report.playlistsSkipped} />
        {report.duplicates.length > 0 ? (
          <details className={own.section}>
            <summary>{t("Left out: the same file as another rekordbox track")} ({report.duplicates.length.toLocaleString()})</summary>
            <ul className={own.list}>
              {report.duplicates.slice(0, SHOWN).map(line => <li key={line}>{line}</li>)}
              <More total={report.duplicates.length} />
            </ul>
          </details>
        ) : null}
      </div>
      {report.keptDeleted > 0 ? (
        <p className={styles.text}>
          {t("{n} tracks you deleted in rbxport were left deleted.", { n: report.keptDeleted.toLocaleString() })}
        </p>
      ) : null}
      {report.problems.length > 0 ? (
        <div className={styles.error} role="alert">
          <p>{t("Some files could not be copied. Those tracks can be analysed again:")}</p>
          <ul className={styles.problems}>
            {report.problems.slice(0, 10).map(problem => <li key={problem}>{problem}</li>)}
            <More total={report.problems.length} shownCount={10} />
          </ul>
        </div>
      ) : null}
      <p className={styles.path} title={report.log}>{t("Import log:")} {report.log}</p>
    </>
  );
}

function Tracks({ title, lines }: { title: string; lines: DeltaTrackLine[] }) {
  if (lines.length === 0) return null;
  return (
    <details className={own.section}>
      <summary>{title} ({lines.length.toLocaleString()})</summary>
      <ul className={own.list}>
        {lines.slice(0, SHOWN).map(line => <li key={line.id}>{line.name}</li>)}
        <More total={lines.length} />
      </ul>
    </details>
  );
}

function Playlists({ title, lines }: { title: string; lines: DeltaPlaylistLine[] }) {
  const t = useTranslation();
  if (lines.length === 0) return null;
  return (
    <details className={own.section}>
      <summary>{title} ({lines.length.toLocaleString()})</summary>
      <ul className={own.list}>
        {lines.map((line, index) => (
          <li key={`${line.name}-${index}`}>{line.name} · {t("{n} tracks", { n: line.tracks.toLocaleString() })}</li>
        ))}
      </ul>
    </details>
  );
}

function More({ total, shownCount = SHOWN }: { total: number; shownCount?: number }) {
  const t = useTranslation();
  if (total <= shownCount) return null;
  return <li className={own.note}>{t("…and {n} more (see the import log)", { n: (total - shownCount).toLocaleString() })}</li>;
}

type T = ReturnType<typeof useTranslation>;

function summaryLine(r: DeltaImport, t: T): string {
  const parts = [
    t("{n} new tracks", { n: r.newTracks.length.toLocaleString() }),
    t("{n} updated", { n: r.updated.length.toLocaleString() }),
    t("{n} kept as edited in rbxport", { n: r.skipped.length.toLocaleString() }),
    t("{n} filled in", { n: r.filled.length.toLocaleString() }),
    t("{n} unchanged", { n: r.unchanged.toLocaleString() }),
  ];
  const lists = [
    t("{n} playlists made", { n: r.playlistsCreated.length.toLocaleString() }),
    t("{n} added to", { n: r.playlistsAppended.length.toLocaleString() }),
    t("{n} history sessions", { n: r.historySessions.toLocaleString() }),
  ];
  return `${parts.join(", ")}. ${lists.join(", ")}.`;
}

function progressText(progress: ImportProgress | null, t: T): string {
  switch (progress?.stage) {
    case undefined:
    case "checking": return t("Checking rekordbox's library…");
    case "copyingDatabase": return t("Copying the library database…");
    case "comparing": return t("Comparing the two libraries…");
    case "copyingFiles":
      return t("Matching tracks and copying what's missing… {done} of {total}", {
        done: progress.done.toLocaleString(), total: progress.total.toLocaleString(),
      });
    case "merging": return t("Writing rbxport's library…");
    case "recording": return t("Recording what was imported…");
    case "finishing": return t("Checking that rekordbox's library is unchanged…");
  }
}

function fillName(kind: DeltaImport["filled"][number]["what"][number], t: T): string {
  switch (kind) {
    case "memoryCues": return t("memory cues");
    case "hotCues": return t("hot cues");
    case "analysis": return t("beat grid and waveforms");
    case "key": return t("key");
    case "artwork": return t("artwork");
  }
}

const FIELD_NAMES: Record<string, string> = {
  title: "Track Title", artist: "Artist", album: "Album", album_artist: "Album Artist", remixer: "Remixer",
  original_artist: "Original Artist", composer: "Composer", lyricist: "Lyricist", genre: "Genre", label: "Label",
  release_year: "Year", release_date: "Release Date", track_no: "Track Number", disc_no: "Disc Number",
  comment: "Comments", rating: "Rating", color: "Color", isrc: "ISRC", my_tags: "My Tag",
};

function fieldName(field: string, t: T): string {
  return t(FIELD_NAMES[field] ?? field);
}

function shown(value: string, t: T): string {
  return value === "" ? t("(empty)") : value;
}
