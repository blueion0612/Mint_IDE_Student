/**
 * Complete code edit history.
 * Every insert/delete is logged with:
 *   - timestamp
 *   - file path
 *   - operation type (ins/del)
 *   - source: "type" (keyboard), "paste" (clipboard), "auto" (bracket-close, indent)
 *   - position and text
 */

export interface EditEntry {
  /** ms since epoch */
  t: number;
  /** file path */
  f: string;
  /** "ins" | "del" */
  op: "ins" | "del";
  /** "type" | "paste" | "auto" | "undo" | "redo" */
  src: string;
  /** character offset */
  pos: number;
  /** the text inserted or description of deletion */
  text: string;
  /** length of text (for quick stats) */
  len: number;
}

const history: EditEntry[] = [];
let currentFile = "";
let lastInputSource: string = "type";

export function setCurrentFile(path: string): void {
  currentFile = path;
}

/**
 * Called from DOM input handlers BEFORE the CodeMirror transaction fires.
 * Sets the source label for the next transaction(s).
 */
export function markNextInputSource(source: "type" | "paste"): void {
  lastInputSource = source;
}

/**
 * Called from CodeMirror's updateListener for every document-changing transaction.
 * The `userEvent` annotation from CodeMirror tells us undo/redo/etc.
 */
export function recordTransaction(
  changes: { from: number; to: number; inserted: string }[],
  userEvent?: string,
): void {
  const now = Date.now();

  // Determine source from userEvent annotation or our manual marker
  let src = lastInputSource;
  if (userEvent) {
    if (userEvent.startsWith("undo")) src = "undo";
    else if (userEvent.startsWith("redo")) src = "redo";
    else if (userEvent === "input.complete" || userEvent === "input.indent") src = "auto";
  }

  // Heuristic: single char = typing, multi-char with no prior paste marker = auto (bracket close, etc.)
  // But if lastInputSource was "paste", trust that.

  for (const c of changes) {
    const deletedLen = c.to - c.from;

    if (deletedLen > 0) {
      history.push({
        t: now, f: currentFile, op: "del", src,
        pos: c.from,
        text: `[${deletedLen} chars deleted]`,
        len: deletedLen,
      });
    }

    if (c.inserted.length > 0) {
      // Refine source: if more than 1 char inserted and not marked as paste,
      // it's likely auto-complete/bracket-close
      let effectiveSrc = src;
      if (src === "type" && c.inserted.length > 1 && c.inserted !== "\n") {
        // Could be bracket auto-close (e.g. typing "(" inserts "()")
        // Keep as "type" if <= 2 chars (bracket pair), otherwise "auto"
        if (c.inserted.length > 2) effectiveSrc = "auto";
      }

      history.push({
        t: now, f: currentFile, op: "ins", src: effectiveSrc,
        pos: c.from,
        text: c.inserted,
        len: c.inserted.length,
      });
    }
  }

  // Reset source back to "type" after consuming
  lastInputSource = "type";
}

export function getEditHistory(): EditEntry[] {
  return history;
}

export function getEditHistoryJSON(): string {
  return JSON.stringify(history);
}
