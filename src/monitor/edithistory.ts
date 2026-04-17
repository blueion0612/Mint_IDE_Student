/**
 * Complete code edit history with consecutive typing compression.
 *
 * Consecutive single-char typing within 300ms is merged into one entry.
 * e.g. typing "hello" = 1 entry instead of 5.
 * Paste, undo, redo, auto-complete are never merged.
 */

export interface EditEntry {
  t: number;      // ms since epoch (start of the batch)
  f: string;      // file path
  op: "ins" | "del";
  src: string;    // "type" | "paste" | "auto" | "undo" | "redo"
  pos: number;    // starting character offset
  text: string;   // inserted text or "[N chars deleted]"
  len: number;    // total length
}

const history: EditEntry[] = [];
let currentFile = "";
let lastInputSource: string = "type";

// Merge window: consecutive typed chars within this ms are batched
const MERGE_WINDOW_MS = 300;

export function setCurrentFile(path: string): void {
  currentFile = path;
}

export function markNextInputSource(source: "type" | "paste"): void {
  lastInputSource = source;
}

export function recordTransaction(
  changes: { from: number; to: number; inserted: string }[],
  userEvent?: string,
): void {
  const now = Date.now();

  let src = lastInputSource;
  if (userEvent) {
    if (userEvent.startsWith("undo")) src = "undo";
    else if (userEvent.startsWith("redo")) src = "redo";
    else if (userEvent === "input.complete" || userEvent === "input.indent") src = "auto";
  }

  for (const c of changes) {
    const deletedLen = c.to - c.from;

    if (deletedLen > 0) {
      // Deletions: try merge consecutive backspaces
      const last = history.length > 0 ? history[history.length - 1] : null;
      if (
        last &&
        last.op === "del" &&
        last.src === src &&
        last.f === currentFile &&
        src === "type" &&
        now - last.t < MERGE_WINDOW_MS &&
        deletedLen === 1
      ) {
        last.len += 1;
        last.text = `[${last.len} chars deleted]`;
        last.t = now;
      } else {
        history.push({
          t: now, f: currentFile, op: "del", src,
          pos: c.from,
          text: `[${deletedLen} chars deleted]`,
          len: deletedLen,
        });
      }
    }

    if (c.inserted.length > 0) {
      let effectiveSrc = src;
      if (src === "type" && c.inserted.length > 1 && c.inserted !== "\n") {
        if (c.inserted.length > 2) effectiveSrc = "auto";
      }

      // Try merge consecutive typing
      const last = history.length > 0 ? history[history.length - 1] : null;
      if (
        last &&
        last.op === "ins" &&
        last.src === "type" &&
        effectiveSrc === "type" &&
        last.f === currentFile &&
        now - last.t < MERGE_WINDOW_MS &&
        c.inserted.length === 1 &&
        c.from === last.pos + last.len  // consecutive position
      ) {
        // Merge into previous entry
        last.text += c.inserted;
        last.len += 1;
        last.t = now; // update timestamp to keep the window sliding
      } else {
        history.push({
          t: now, f: currentFile, op: "ins", src: effectiveSrc,
          pos: c.from,
          text: c.inserted,
          len: c.inserted.length,
        });
      }
    }
  }

  lastInputSource = "type";
}

export function getEditHistory(): EditEntry[] {
  return history;
}

export function getEditHistoryJSON(): string {
  return JSON.stringify(history);
}
