import { useEffect, useRef, useState } from "react";
import styles from "./NewLibraryDialog.module.css";

/**
 * The first start's one question, before rbxport has a library of its own:
 * make one, or quit. Nothing else in the window works without a library, so
 * there is no other way out; Escape does nothing. The library is rbxport's,
 * in its own folder; rekordbox's is never opened from here.
 */
export function NewLibraryDialog({ masterDb, onCreate, onQuit }: {
  masterDb: string;
  onCreate: () => Promise<void>;
  onQuit: () => void;
}) {
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState("");
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const element = dialog.current;
    element?.showModal();
    return () => element?.close();
  }, []);

  const create = () => {
    setCreating(true);
    setError("");
    onCreate().catch((e: unknown) => {
      setCreating(false);
      setError(errorText(e));
    });
  };

  return (
    <dialog ref={dialog} className={styles.dialog} aria-labelledby="new-library-title"
      aria-describedby="new-library-text"
      onCancel={event => event.preventDefault()}
      onKeyDown={event => event.stopPropagation()}>
      <form onSubmit={event => { event.preventDefault(); if (!creating) create(); }}>
        <h2 id="new-library-title" className={styles.title}>Welcome to rbxport</h2>
        <p id="new-library-text" className={styles.text}>
          rbxport keeps its own library, separate from rekordbox. Create a new,
          empty library to start. It will be made here:
        </p>
        <p className={styles.path} title={masterDb}>{masterDb}</p>
        {error ? <p className={styles.error} role="alert">{error}</p> : null}
        <div className={styles.buttons}>
          <button type="submit" disabled={creating} autoFocus>{creating ? "Creating…" : "Create a New Library"}</button>
          <button type="button" onClick={onQuit} disabled={creating}>Quit</button>
        </div>
      </form>
    </dialog>
  );
}

/** The backend's message when it sent one, which says what went wrong. */
function errorText(e: unknown): string {
  if (typeof e === "object" && e !== null && "message" in e && typeof e.message === "string") return e.message;
  if (typeof e === "string") return e;
  return "Could not create the library.";
}
