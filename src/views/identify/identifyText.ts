/**
 * Words and small rules the identify review screen shares: what a field,
 * a state and a candidate's source are called, which fields start ticked,
 * and which states can still be written.
 */
import type {
  IdentifyCandidate, IdentifyDecision, IdentifyField, IdentifyFieldChange, IdentifyFilter, IdentifyQueueItem,
  IdentifyReviewState,
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

export const FILTERS: readonly IdentifyFilter[] = ["needsReview", "unidentified", "applied", "all"];

export function filterLabel(filter: IdentifyFilter, t: Translate): string {
  switch (filter) {
    case "needsReview": return t("Needs review");
    case "unidentified": return t("Unidentified");
    case "applied": return t("Applied (undoable)");
    case "all": return t("All");
  }
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

/** "Artist – Title", or whichever of the two the file has. */
export function trackName(item: Pick<IdentifyQueueItem, "artist" | "title" | "path">): string {
  const name = [item.artist, item.title].filter((part) => part.trim() !== "").join(" – ");
  return name || (item.path.split(/[\\/]/).at(-1) ?? item.path);
}
