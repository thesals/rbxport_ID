/**
 * Words and small rules the identify review screen shares: what a field,
 * a state and a candidate's source are called, which fields start ticked,
 * and which states can still be written.
 */
import type {
  IdentifyCandidate, IdentifyDecision, IdentifyField, IdentifyFieldChange, IdentifyFilter, IdentifyQueueItem,
  IdentifyRekordboxRecord, IdentifyReviewState,
} from "@/ipc/types";
import type { useTranslation } from "@/i18n";

export type Translate = ReturnType<typeof useTranslation>;

export function fieldLabel(field: IdentifyField, t: Translate): string {
  switch (field) {
    case "title": return t("Track Title");
    case "artist": return t("Artist");
    case "album": return t("Album");
    case "label": return t("Label");
    case "year": return t("Year");
    case "genre": return t("Genre");
    case "isrc": return t("ISRC");
    case "catalogNumber": return t("Catalog number");
    case "mbRecordingId": return t("MusicBrainz recording ID");
    case "mbReleaseId": return t("MusicBrainz release ID");
    case "mbReleaseGroupId": return t("MusicBrainz release group ID");
    case "mbArtistId": return t("MusicBrainz artist ID");
    case "mbReleaseTrackId": return t("MusicBrainz release track ID");
    case "mbAlbumArtistId": return t("MusicBrainz album artist ID");
    case "discogsReleaseId": return t("Discogs release ID");
    case "subGenre": return t("Sub-genre (added to Comment)");
    case "artwork": return t("Artwork");
  }
}

export function stateLabel(state: IdentifyReviewState, t: Translate): string {
  switch (state) {
    case "unidentified": return t("Unidentified");
    case "pending": return t("Needs review");
    case "accepted": return t("Accepted");
    case "rejected": return t("Rejected");
    case "applied": return t("Applied");
    case "autoApplied": return t("Applied automatically");
    case "undone": return t("Undone");
  }
}

export function decisionLabel(decision: IdentifyDecision, t: Translate): string {
  switch (decision) {
    case "autoApply": return t("Auto-apply match");
    case "review": return t("Review match");
    case "unidentified": return t("No match");
  }
}

export function sourceLabel(source: IdentifyCandidate["source"], t: Translate): string {
  switch (source) {
    case "fingerprint": return t("Fingerprint");
    case "textSearch": return t("Text search");
    case "discogsText": return t("Discogs text search");
  }
}

/** The source as a queue row shows it: a word, with `sourceLabel` as its title. */
export function sourceShort(source: IdentifyCandidate["source"], t: Translate): string {
  switch (source) {
    case "fingerprint": return t("Fingerprint");
    case "textSearch": return t("Text search");
    case "discogsText": return t("Discogs");
  }
}

/**
 * What rekordbox's imported record holds for a field the diff shows, or
 * null for a field rekordbox does not keep. The sub-genre is appended to the
 * comment, so its rekordbox value is the comment.
 */
export function rekordboxValue(field: IdentifyField, record: IdentifyRekordboxRecord): string | null {
  switch (field) {
    case "title": return record.title;
    case "artist": return record.artist;
    case "album": return record.album;
    case "label": return record.label;
    case "genre": return record.genre;
    case "year": return record.year;
    case "isrc": return record.isrc;
    case "subGenre": return record.comment;
    default: return null;
  }
}

/**
 * How to see a written change in rekordbox, which reads a file's tags again
 * only when asked: its own Reload Tag, in the words its menu uses.
 */
export function reloadHint(t: Translate): string {
  return t("To see the changes in rekordbox, select the tracks there and choose {reloadTag}.", { reloadTag: t("Reload Tag") });
}

export const FILTERS: readonly IdentifyFilter[] = ["needsReview", "unidentified", "waiting", "applied", "all"];

export function filterLabel(filter: IdentifyFilter, t: Translate): string {
  switch (filter) {
    case "needsReview": return t("Needs review");
    case "unidentified": return t("Unidentified");
    case "waiting": return t("Waiting for file");
    case "applied": return t("Applied (undoable)");
    case "all": return t("All");
  }
}

/** What a row shows for a result whose write waits for its file. */
export function waitingLabel(t: Translate): string {
  return t("Waiting — file in use");
}

/**
 * The neutral notice for writes deferred because their files were in use:
 * not a failure, they are written once the files are free.
 */
export function waitingNotice(count: number, t: Translate): string {
  return t("{count} waiting for files in use.", { count });
}

/** A score as the list shows it: a whole percentage, or a dash for none. */
export function formatScore(score: number | null): string {
  return score === null ? "–" : `${Math.round(score * 100)}%`;
}

/** Only these can be written; a pending result is accepted by writing it. */
export function canApply(state: IdentifyReviewState): boolean {
  return state === "pending" || state === "accepted";
}

/** A change that would alter the file: a proposal different from what is there. */
export function changes(change: IdentifyFieldChange): boolean {
  return change.proposed !== "" && change.proposed !== change.current;
}

/** The fields ticked when a result is opened: every one that would change. */
export function defaultFields(item: Pick<IdentifyQueueItem, "changes">): IdentifyField[] {
  return item.changes.filter(changes).map((change) => change.field);
}

/**
 * What Apply accepted would write. Bulk only fills: a field the file has
 * empty, and artwork for a file without any. An overwrite of a value the
 * file has, and the sub-genre (an addition to the comment), are written only
 * from a result's own review, with the user's ticks — so a result that has
 * either is left out of the bulk write entirely and stays accepted for that
 * review, rather than being marked applied with them unwritten.
 */
export interface BulkPlan {
  items: { resultId: number; fields: IdentifyField[] }[];
  /** Fields the items fill, all told. */
  fields: number;
  /** Accepted results left for their own review. */
  leftForReview: number;
}

export function bulkPlan(
  accepted: readonly Pick<IdentifyQueueItem, "resultId" | "changes">[],
  /** Whether the file already has artwork, for results proposing some. */
  hasArtwork: (resultId: number) => boolean,
): BulkPlan {
  const plan: BulkPlan = { items: [], fields: 0, leftForReview: 0 };
  for (const item of accepted) {
    const wanted = item.changes.filter(changes);
    const fills = wanted.filter((change) =>
      change.field === "artwork" ? !hasArtwork(item.resultId)
        : change.field !== "subGenre" && change.current.trim() === "");
    if (fills.length < wanted.length) {
      plan.leftForReview += 1;
    } else if (fills.length > 0) {
      plan.items.push({ resultId: item.resultId, fields: fills.map((change) => change.field) });
      plan.fields += fills.length;
    }
  }
  return plan;
}

/** "Artist – Title", or whichever of the two the file has. */
export function trackName(item: Pick<IdentifyQueueItem, "artist" | "title" | "path">): string {
  const name = [item.artist, item.title].filter((part) => part.trim() !== "").join(" – ");
  return name || (item.path.split(/[\\/]/).at(-1) ?? item.path);
}
