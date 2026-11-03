/**
 * Track identification's settings (fork-only; `docs/identify/IDENTIFY_SPEC.md`):
 * the AcoustID key, the Discogs token and the MusicBrainz user agent the
 * services need, how Discogs styles are proposed for Genre, and auto-apply.
 *
 * Keys are secrets. The backend reports only where each comes from, never
 * its value; a new one is typed into an empty password box and sent once,
 * and nothing here keeps or shows it afterwards.
 *
 * The backend reads a key from the environment first, then the data
 * folder's `.env.identify` (which this pane writes), then a development
 * checkout's. So an environment variable cannot be replaced or cleared from
 * here at all, and a development-file key can be replaced (the saved one
 * wins) but not cleared.
 *
 * Every save sends only what changed: an absent member is kept, a null key
 * is cleared.
 */
import { useEffect, useId, useState } from "react";

import { getBackend } from "@/ipc/client";
import type {
  IdentifyGenrePolicy, IdentifyKeySource, IdentifySettings, IdentifySettingsUpdate,
} from "@/ipc/types";
import { useTranslation } from "@/i18n";
import { errorMessage } from "@/lib/errorMessage";
import { Button, Section } from "./controls";
import layout from "./PaneLayout.module.css";
import controls from "./Preferences.module.css";
import styles from "./IdentifyPane.module.css";

type Translate = ReturnType<typeof useTranslation>;

/** The auto-apply threshold's bounds, as the backend enforces them. */
export const THRESHOLD_MIN = 0.92;
export const THRESHOLD_MAX = 1;

/** Which part of the pane a save came from, so its error shows beside it. */
type Area = "acoustidKey" | "discogsToken" | "mbUserAgent" | "genrePolicy" | "autoApply";

function sourceText(source: IdentifyKeySource, t: Translate): string {
  switch (source) {
    case "settings": return t("Set (from settings)");
    case "environment": return t("Set (from environment)");
    case "devFile": return t("Set (from development file)");
    case null: return t("Not set");
  }
}

/** The development file's name: a file name, never translated. */
const DEV_FILE = ".env.identify";

/**
 * Why a value cannot be changed or cleared here, or null when it can. The
 * variable and file names go in as values so no translation touches them.
 */
function lockedReason(source: IdentifyKeySource, variable: string, t: Translate): string | null {
  switch (source) {
    case "environment":
      return t("The environment variable {variable} sets this and takes priority over anything saved here. Change or remove it outside rbxport.", { variable });
    case "devFile":
      return t("This comes from the development file {file}, which can't be changed here. A value saved here takes priority over it.", { file: DEV_FILE });
    default: return null;
  }
}

/** An address shown as text that opens in the browser. */
function Link({ url }: { url: string }) {
  const open = () => { void getBackend().then((backend) => backend.openUrl(`https://${url}`)).catch(() => {}); };
  return <button type="button" role="link" className={styles.link} onClick={open}>{url}</button>;
}

function KeyCard({ name, variable, source, getAt, busy, error, onSave }: {
  name: string;
  /** The environment variable that would set it. */
  variable: string;
  source: IdentifyKeySource;
  /** Where to get one, without the scheme. */
  getAt: string;
  busy: boolean;
  error: string | undefined;
  /** A new value, or null to clear. */
  onSave: (value: string | null) => Promise<boolean>;
}) {
  const t = useTranslation();
  const id = useId();
  const [editing, setEditing] = useState(false);
  // Only ever what the user is typing now; never filled from the backend.
  const [value, setValue] = useState("");
  const locked = lockedReason(source, variable, t);
  const close = () => {
    setEditing(false);
    setValue("");
  };
  const save = async () => {
    if (value.trim() === "") return;
    if (await onSave(value.trim())) close();
  };
  return (
    <section className={`${layout.card} ${styles.card}`} aria-label={name}>
      <strong id={`${id}-name`}>{name}</strong>
      <p className={styles.status} data-set={source !== null || undefined} role="status">{sourceText(source, t)}</p>
      {editing && source !== "environment" ? (
        <form className={styles.actions} onSubmit={(event) => { event.preventDefault(); void save(); }}>
          <input
            type="password"
            className={styles.input}
            aria-label={t("New {name}", { name })}
            autoComplete="off"
            spellCheck={false}
            value={value}
            // Asked for: typing goes here.
            autoFocus
            onChange={(event) => setValue(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.stopPropagation();
                close();
              }
            }}
          />
          <button type="submit" className={controls.button} disabled={busy || value.trim() === ""}>{t("Save")}</button>
          <Button onClick={close}>{t("Cancel")}</Button>
        </form>
      ) : (
        <div className={styles.actions}>
          {/* Saving under an environment variable would change nothing: the variable wins. */}
          {source !== "environment" ? (
            <button type="button" className={controls.button} aria-describedby={`${id}-name`} disabled={busy}
              onClick={() => setEditing(true)}>
              {source === null ? t("Add…") : t("Replace…")}
            </button>
          ) : null}
          {source === "settings" ? (
            <button type="button" className={controls.button} aria-label={t("Remove {name}", { name })} disabled={busy}
              onClick={() => void onSave(null)}>
              {t("Remove")}
            </button>
          ) : null}
        </div>
      )}
      {locked !== null ? <p className={layout.help}>{locked}</p> : null}
      <p className={layout.help}>{t("Get one at")} <Link url={getAt} /></p>
      {error !== undefined ? <p className={styles.error} role="alert">{error}</p> : null}
    </section>
  );
}

export function IdentifyPane() {
  const t = useTranslation();
  const id = useId();
  const [settings, setSettings] = useState<IdentifySettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [errors, setErrors] = useState<Partial<Record<Area, string>>>({});
  const setError = (area: Area, message: string | null) => setErrors((current) => {
    const next = { ...current };
    if (message === null) delete next[area];
    else next[area] = message;
    return next;
  });
  const [busy, setBusy] = useState(false);
  const [agent, setAgent] = useState("");
  const [threshold, setThreshold] = useState("");

  useEffect(() => {
    let live = true;
    void getBackend().then((backend) => backend.identify.settings()).then((found) => {
      if (!live) return;
      setSettings(found);
      setAgent(found.mbUserAgent);
      setThreshold(found.autoApplyThreshold.toFixed(2));
    }).catch((e: unknown) => { if (live) setLoadError(errorMessage(e)); });
    return () => { live = false; };
  }, []);

  /** Sends one change; true when it was saved. */
  const save = async (area: Area, update: IdentifySettingsUpdate): Promise<boolean> => {
    setBusy(true);
    setError(area, null);
    try {
      const saved = await (await getBackend()).identify.saveSettings(update);
      setSettings(saved);
      if (area === "mbUserAgent") setAgent(saved.mbUserAgent);
      if (area === "autoApply") setThreshold(saved.autoApplyThreshold.toFixed(2));
      return true;
    } catch (e) {
      setError(area, errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  if (settings === null) {
    return (
      <Section title={t("Identify")}>
        {loadError !== null
          ? <p className={styles.error} role="alert">{loadError}</p>
          : <p className={layout.help}>{t("Loading…")}</p>}
      </Section>
    );
  }

  const commitThreshold = () => {
    const value = Number(threshold);
    if (threshold.trim() === "" || !Number.isFinite(value) || value < THRESHOLD_MIN || value > THRESHOLD_MAX) {
      setError("autoApply", t("Choose a threshold from {min} to {max}.", { min: THRESHOLD_MIN.toFixed(2), max: THRESHOLD_MAX.toFixed(2) }));
      return;
    }
    const rounded = Math.round(value * 100) / 100;
    setThreshold(rounded.toFixed(2));
    if (rounded === settings.autoApplyThreshold) {
      setError("autoApply", null);
      return;
    }
    void save("autoApply", { autoApplyThreshold: rounded });
  };

  const policies: readonly { value: IdentifyGenrePolicy; label: string; help: string }[] = [
    { value: "fillEmpty", label: t("Fill an empty Genre (default)"),
      help: t("Propose the Discogs styles for Genre only when the file has none.") },
    { value: "replace", label: t("Replace Genre"),
      help: t("Propose the Discogs styles for Genre even when the file has one.") },
    { value: "never", label: t("Never propose Genre"),
      help: t("Leave Genre alone.") },
  ];

  const agentFromEnvironment = settings.mbUserAgentSource === "environment";
  const agentLocked = lockedReason(settings.mbUserAgentSource, "MB_USER_AGENT", t);

  return (
    <Section title={t("Identify")}>
      <p className={`${layout.help} ${styles.intro}`}>
        {t("Identify tracks with AcoustID, MusicBrainz and Discogs. Only the tags in the audio files are written; the rekordbox library is not changed.")}
      </p>

      <KeyCard
        name={t("AcoustID API key")}
        variable="ACOUSTID_API_KEY"
        source={settings.acoustidKey}
        getAt="acoustid.org/new-application"
        busy={busy}
        error={errors.acoustidKey}
        onSave={(value) => save("acoustidKey", { acoustidKey: value })}
      />
      <KeyCard
        name={t("Discogs personal token")}
        variable="DISCOGS_TOKEN"
        source={settings.discogsToken}
        getAt="discogs.com/settings/developers"
        busy={busy}
        error={errors.discogsToken}
        onSave={(value) => save("discogsToken", { discogsToken: value })}
      />

      <section className={`${layout.card} ${styles.card}`} aria-label={t("MusicBrainz user agent")}>
        <label htmlFor={`${id}-agent`}><strong>{t("MusicBrainz user agent")}</strong></label>
        <p className={styles.status} data-set={settings.mbUserAgentSource !== null || undefined}>
          {sourceText(settings.mbUserAgentSource, t)}
        </p>
        <form className={styles.actions} onSubmit={(event) => {
          event.preventDefault();
          void save("mbUserAgent", { mbUserAgent: agent.trim() });
        }}>
          <input
            id={`${id}-agent`}
            type="text"
            className={styles.input}
            spellCheck={false}
            placeholder="rbxport/1.2 ( you@example.com )"
            value={agent}
            disabled={agentFromEnvironment}
            onChange={(event) => setAgent(event.target.value)}
          />
          <button type="submit" className={controls.button}
            disabled={busy || agentFromEnvironment || agent.trim() === settings.mbUserAgent}>
            {t("Save")}
          </button>
        </form>
        <p className={layout.help}>
          {t("MusicBrainz asks every app to name itself and a contact, in the form App/version ( contact ).")}
        </p>
        {agentLocked !== null ? <p className={layout.help}>{agentLocked}</p> : null}
        {errors.mbUserAgent !== undefined ? <p className={styles.error} role="alert">{errors.mbUserAgent}</p> : null}
      </section>

      <section className={`${layout.card} ${styles.card}`} aria-label={t("Genre")}>
        <strong>{t("Genre")}</strong>
        <div role="radiogroup" aria-label={t("Genre")}>
          {policies.map((policy) => (
            <label key={policy.value} className={styles.choice}>
              <input
                type="radio"
                className={controls.radio}
                name={`${id}-genre`}
                value={policy.value}
                checked={settings.genrePolicy === policy.value}
                disabled={busy}
                onChange={() => void save("genrePolicy", { genrePolicy: policy.value })}
              />
              <span>
                {policy.label}
                <small>{policy.help}</small>
              </span>
            </label>
          ))}
        </div>
        <p className={layout.help}>
          {t("Whatever is chosen here, the Discogs styles (the sub-genre) are always proposed for Comment, for review.")}
        </p>
        {errors.genrePolicy !== undefined ? <p className={styles.error} role="alert">{errors.genrePolicy}</p> : null}
      </section>

      <section className={`${layout.card} ${styles.card}`} aria-label={t("Auto-apply")}>
        <label className={styles.switch}>
          <strong id={`${id}-auto`}>{t("Apply the best matches automatically")}</strong>
          <input
            type="checkbox"
            role="switch"
            className={controls.toggle}
            aria-labelledby={`${id}-auto`}
            aria-describedby={`${id}-auto-help`}
            checked={settings.autoApply}
            disabled={busy}
            onChange={(event) => void save("autoApply", { autoApply: event.target.checked })}
          />
        </label>
        <p id={`${id}-auto-help`} className={layout.help}>
          {t("When on, a fingerprint match scoring at least the minimum below that only fills empty fields is written to the file's tags as soon as it is identified. It appears under {applied}; if the write fails, it stays under {needsReview}. Everything else waits for review. Off by default.", {
            // The review queue's own filter names, in the same words.
            applied: t("Applied (undoable)"), needsReview: t("Needs review"),
          })}
        </p>
        <div className={styles.actions}>
          <label htmlFor={`${id}-threshold`}>{t("Minimum score")}</label>
          <input
            id={`${id}-threshold`}
            type="number"
            className={`${styles.input} ${styles.number}`}
            min={THRESHOLD_MIN}
            max={THRESHOLD_MAX}
            step={0.01}
            value={threshold}
            disabled={busy || !settings.autoApply}
            onChange={(event) => setThreshold(event.target.value)}
            onBlur={commitThreshold}
            onKeyDown={(event) => { if (event.key === "Enter") commitThreshold(); }}
          />
          <span className={layout.help}>{`${THRESHOLD_MIN.toFixed(2)}–${THRESHOLD_MAX.toFixed(2)}`}</span>
        </div>
        {errors.autoApply !== undefined ? <p className={styles.error} role="alert">{errors.autoApply}</p> : null}
      </section>

      <section className={`${layout.card} ${styles.card}`} aria-label={t("Settings file")}>
        <strong>{t("Settings file")}</strong>
        <p className={styles.path}>{settings.settingsFile}</p>
      </section>
    </Section>
  );
}
