import { invoke } from "@tauri-apps/api/core";

interface InputEvent {
  inputType: string;
  text: string;
  from: number;
  to: number;
}

const PASTE_THRESHOLD = 100; // chars — above this, flag as paste_large
// Burst tuned for "watch-and-type from external source" detection.
// Normal fast typing rarely sustains 15 chars in 200ms (= 75 cps), but
// transcribing from an external screen often hits this rate.
const BURST_WINDOW_MS = 200;
const BURST_CHAR_THRESHOLD = 15;

let lastInputTime = 0;
let burstBuffer: { time: number; chars: number }[] = [];

// Buffer the last clipboard event seen by the frontend so paste events can
// reference where the clipboard content most likely came from.
interface ClipboardSnapshot {
  source: string;          // process name (or "self")
  windowTitle: string;     // foreground window title at copy time, if any
  isExternal: boolean;
  epochMs: number;
}
let lastClipboard: ClipboardSnapshot | null = null;

export function noteClipboardEvent(snapshot: ClipboardSnapshot): void {
  lastClipboard = snapshot;
}

export function handleEditorInput(event: InputEvent): void {
  const now = performance.now();
  const timeDelta = lastInputTime > 0 ? now - lastInputTime : 0;
  lastInputTime = now;

  // Copy event
  if (event.inputType === "copy") {
    const preview = event.text.length > 100
      ? event.text.substring(0, 100).replace(/\n/g, "\\n") + "..."
      : event.text.replace(/\n/g, "\\n");
    invoke("log_editor_event", {
      eventType: "copy",
      detail: `Copied ${event.text.length} chars: ${preview}`,
      charCount: event.text.length,
      timeDeltaMs: timeDelta,
    });
    return;
  }

  // Cut event
  if (event.inputType === "cut") {
    const preview = event.text.length > 100
      ? event.text.substring(0, 100).replace(/\n/g, "\\n") + "..."
      : event.text.replace(/\n/g, "\\n");
    invoke("log_editor_event", {
      eventType: "cut",
      detail: `Cut ${event.text.length} chars: ${preview}`,
      charCount: event.text.length,
      timeDeltaMs: timeDelta,
    });
    return;
  }

  // Paste event
  if (event.inputType === "insertFromPaste") {
    const charCount = event.text.length;
    const eventType = charCount > PASTE_THRESHOLD ? "paste_large" : "paste";
    const preview = event.text.length > 100
      ? event.text.substring(0, 100).replace(/\n/g, "\\n") + "..."
      : event.text.replace(/\n/g, "\\n");

    // Attach source-of-clipboard hint if it was set within the last 5s.
    let sourceHint = "";
    if (lastClipboard && Date.now() - lastClipboard.epochMs < 5000) {
      const tag = lastClipboard.isExternal ? "external" : "self";
      const titlePart = lastClipboard.windowTitle ? ` (${lastClipboard.windowTitle})` : "";
      sourceHint = ` [from ${tag}: ${lastClipboard.source}${titlePart}]`;
    }

    invoke("log_editor_event", {
      eventType,
      detail: `Pasted ${charCount} chars${sourceHint}: ${preview}`,
      charCount,
      timeDeltaMs: timeDelta,
    });
    return;
  }

  // Burst detection: track rapid character input
  burstBuffer.push({ time: now, chars: event.text.length });
  burstBuffer = burstBuffer.filter((b) => now - b.time < BURST_WINDOW_MS);

  const totalBurstChars = burstBuffer.reduce((sum, b) => sum + b.chars, 0);

  if (totalBurstChars > BURST_CHAR_THRESHOLD) {
    invoke("log_editor_event", {
      eventType: "input_burst",
      detail: `Rapid input detected: ${totalBurstChars} chars in ${BURST_WINDOW_MS}ms`,
      charCount: totalBurstChars,
      timeDeltaMs: timeDelta,
    });
    burstBuffer = [];
  }

  // Track typing for periodic typing_summary (every 30s — see flushTypingSummary below).
  if (event.inputType === "insertText" && event.text.length > 0) {
    trackTyping(event.text.length);
  }
}

// Periodic summary of typing activity
let typedChars = 0;
let typingStartTime = 0;

export function trackTyping(charCount: number): void {
  if (typedChars === 0) {
    typingStartTime = Date.now();
  }
  typedChars += charCount;
}

export function flushTypingSummary(): void {
  if (typedChars > 0) {
    const duration = (Date.now() - typingStartTime) / 1000;
    const wpm = duration > 0 ? Math.round((typedChars / 5) / (duration / 60)) : 0;
    invoke("log_editor_event", {
      eventType: "typing_summary",
      detail: `Typed ${typedChars} chars in ${duration.toFixed(1)}s (~${wpm} WPM)`,
      charCount: typedChars,
      timeDeltaMs: duration * 1000,
    });
    typedChars = 0;
  }
}

// Flush typing summary every 30 seconds
setInterval(flushTypingSummary, 30000);
