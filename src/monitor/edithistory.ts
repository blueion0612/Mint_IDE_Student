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
  src: string;    // "type" | "paste" | "auto" | "undo" | "redo" | "indent" | "cut" | "drop"
  pos: number;    // starting character offset
  text: string;   // inserted text or "[N chars deleted]"
  len: number;    // total length
}

const history: EditEntry[] = [];
let currentFile = "";
let lastInputSource: string = "type";

// CJK ranges that signal IME composition output — if any of these appear,
// treat a multi-char insertion as a typed keystroke (one IME commit = one
// logical keypress) rather than autocomplete. Covers:
//   - CJK symbols / Hiragana / Katakana / CJK Unified Ideographs (BMP)
//   - Hangul Jamo (ㄱ-ㆎ) — pre-composition jamos
//   - Halfwidth Katakana (ｦ-ﾟ)
//   - CJK Extension A (㐀-䶿)
//   - Hangul Syllables (가-힣)
const CJK_RE = /[　-〿぀-ゟ゠-ヿㄱ-ㆎ㐀-䶿一-鿿ｦ-ﾟ가-힯]/;

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
    else if (userEvent === "input.complete") src = "auto";    // autocomplete acceptance (snippet/word)
    else if (userEvent === "input.indent") src = "indent";    // Tab — student-driven, NOT autocomplete
    else if (userEvent === "input.paste") src = "paste";      // explicit paste annotation
    else if (userEvent === "input.drop") src = "drop";        // drag-drop from external app
    else if (userEvent === "delete.cut") src = "cut";         // Ctrl+X — block move out
    // delete.backward / delete.forward / delete.selection fall through to
    // lastInputSource ("type"), which matches student intent for backspace.
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
      // Pure-whitespace inserts (auto-indent on Enter, multi-char Tab) are
      // never autocomplete. CodeMirror's `indentOnInput` fires WITHOUT an
      // `input.indent` userEvent, so without this branch a 4-space auto-indent
      // would trip the length heuristic and be mislabeled "auto".
      if (src === "type" && /^[\t ]+$/.test(c.inserted)) {
        effectiveSrc = "indent";
      }
      // Heuristic fallback for IDE-side autocompletions that don't tag input.complete.
      // Honest single-keystroke insertions: 1 char, or up to ~4 (Tab indent), or
      // bracket auto-close (length 2). Genuine autocomplete tends to drop 5+ chars
      // at once (function names, snippets). Stay loose to avoid false positives,
      // and explicitly skip IME composition commits (CJK input arrives in
      // multi-char bursts from compositionend — this is one keystroke from
      // the student's perspective, not autocomplete).
      else if (
        src === "type" &&
        c.inserted.length > 4 &&
        !c.inserted.includes("\n") &&
        !CJK_RE.test(c.inserted)
      ) {
        effectiveSrc = "auto";
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
