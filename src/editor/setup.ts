import { EditorState, Transaction, StateEffect, StateField, RangeSet } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLineGutter, highlightActiveLine, drawSelection, rectangularSelection, crosshairCursor, highlightSpecialChars, Decoration, type DecorationSet } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { syntaxHighlighting, indentOnInput, bracketMatching, foldGutter, foldKeymap, defaultHighlightStyle, HighlightStyle } from "@codemirror/language";
import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { searchKeymap, highlightSelectionMatches } from "@codemirror/search";
import { javascript } from "@codemirror/lang-javascript";
import { python } from "@codemirror/lang-python";
import { java } from "@codemirror/lang-java";
import { cpp } from "@codemirror/lang-cpp";
import { tags } from "@lezer/highlight";

export type SupportedLanguage = "javascript" | "typescript" | "python" | "java" | "c" | "cpp";

const languageExtensions: Record<SupportedLanguage, () => any> = {
  javascript: () => javascript(),
  typescript: () => javascript({ typescript: true }),
  python: () => python(),
  java: () => java(),
  c: () => cpp(),
  cpp: () => cpp(),
};

// Catppuccin Mocha-inspired theme
const mintTheme = EditorView.theme({
  "&": {
    backgroundColor: "#1e1e2e",
    color: "#cdd6f4",
  },
  ".cm-content": {
    caretColor: "#89b4fa",
  },
  ".cm-cursor, .cm-dropCursor": {
    borderLeftColor: "#89b4fa",
  },
  "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection": {
    backgroundColor: "rgba(137, 180, 250, 0.2)",
  },
  ".cm-gutters": {
    backgroundColor: "#181825",
    color: "#6c7086",
    border: "none",
  },
  ".cm-activeLineGutter": {
    backgroundColor: "#313244",
    color: "#cdd6f4",
  },
  ".cm-activeLine": {
    backgroundColor: "rgba(69, 71, 90, 0.3)",
  },
  ".cm-foldPlaceholder": {
    backgroundColor: "#313244",
    color: "#a6adc8",
    border: "none",
  },
  ".cm-selectionMatch": {
    backgroundColor: "rgba(137, 180, 250, 0.15)",
  },
  ".cm-searchMatch": {
    backgroundColor: "rgba(249, 226, 175, 0.3)",
    outline: "1px solid rgba(249, 226, 175, 0.5)",
  },
  ".cm-searchMatch.cm-searchMatch-selected": {
    backgroundColor: "rgba(249, 226, 175, 0.5)",
  },
}, { dark: true });

const mintHighlightStyle = HighlightStyle.define([
  { tag: tags.keyword, color: "#cba6f7" },
  { tag: tags.operator, color: "#89dceb" },
  { tag: tags.special(tags.variableName), color: "#f38ba8" },
  { tag: tags.typeName, color: "#f9e2af" },
  { tag: tags.atom, color: "#fab387" },
  { tag: tags.number, color: "#fab387" },
  { tag: tags.definition(tags.variableName), color: "#89b4fa" },
  { tag: tags.string, color: "#a6e3a1" },
  { tag: tags.special(tags.string), color: "#a6e3a1" },
  { tag: tags.comment, color: "#6c7086", fontStyle: "italic" },
  { tag: tags.variableName, color: "#cdd6f4" },
  { tag: tags.bracket, color: "#a6adc8" },
  { tag: tags.tagName, color: "#89b4fa" },
  { tag: tags.attributeName, color: "#f9e2af" },
  { tag: tags.propertyName, color: "#89b4fa" },
  { tag: tags.className, color: "#f9e2af" },
  { tag: tags.function(tags.variableName), color: "#89b4fa" },
  { tag: tags.bool, color: "#fab387" },
  { tag: tags.null, color: "#fab387" },
  { tag: tags.regexp, color: "#f5c2e7" },
]);

// ===== Error Line Highlighting =====
const setErrorLines = StateEffect.define<number[]>();
const clearErrorLines = StateEffect.define<null>();

const errorLineDeco = Decoration.line({ class: "cm-error-line" });

const errorLineField = StateField.define<DecorationSet>({
  create() { return RangeSet.empty; },
  update(decos, tr) {
    for (const effect of tr.effects) {
      if (effect.is(clearErrorLines)) {
        return RangeSet.empty;
      }
      if (effect.is(setErrorLines)) {
        const builder: any[] = [];
        for (const lineNum of effect.value) {
          if (lineNum >= 1 && lineNum <= tr.state.doc.lines) {
            const line = tr.state.doc.line(lineNum);
            builder.push(errorLineDeco.range(line.from));
          }
        }
        return RangeSet.of(builder);
      }
    }
    return decos;
  },
  provide: (f) => EditorView.decorations.from(f),
});

/** Highlight specific line numbers as errors (1-indexed) */
export function markErrorLines(view: EditorView, lineNumbers: number[]): void {
  view.dispatch({ effects: setErrorLines.of(lineNumbers) });
}

/** Clear all error line highlights */
export function clearErrors(view: EditorView): void {
  view.dispatch({ effects: clearErrorLines.of(null) });
}

export function createEditor(
  parent: HTMLElement,
  language: SupportedLanguage = "python",
  initialDoc: string = "",
  onInput?: (event: { inputType: string; text: string; from: number; to: number }) => void,
  onTransaction?: (changes: { from: number; to: number; inserted: string }[], userEvent?: string) => void,
): EditorView {
  const langExtension = languageExtensions[language]?.() ?? python();

  // Transaction listener — captures every insert/delete for edit history
  const txListener = EditorView.updateListener.of((update) => {
    if (!update.docChanged || !onTransaction) return;
    const changes: { from: number; to: number; inserted: string }[] = [];
    update.changes.iterChanges((fromA, toA, _fromB, _toB, inserted) => {
      changes.push({ from: fromA, to: toA, inserted: inserted.toString() });
    });
    // Extract userEvent annotation from the transaction
    let userEvent: string | undefined;
    for (const tr of update.transactions) {
      const ann = tr.annotation(Transaction.userEvent);
      if (ann) { userEvent = ann; break; }
    }
    if (changes.length > 0) onTransaction(changes, userEvent);
  });

  const inputListener = EditorView.domEventHandlers({
    beforeinput(event: InputEvent, view: EditorView) {
      if (onInput && event.data) {
        onInput({
          inputType: event.inputType || "unknown",
          text: event.data,
          from: view.state.selection.main.from,
          to: view.state.selection.main.to,
        });
      }
    },
    paste(event: ClipboardEvent, _view: EditorView) {
      if (onInput) {
        const text = event.clipboardData?.getData("text/plain") || "";
        onInput({
          inputType: "insertFromPaste",
          text,
          from: 0,
          to: 0,
        });
      }
    },
  });

  const state = EditorState.create({
    doc: initialDoc,
    extensions: [
      lineNumbers(),
      highlightActiveLineGutter(),
      highlightSpecialChars(),
      history(),
      foldGutter(),
      drawSelection(),
      rectangularSelection(),
      crosshairCursor(),
      highlightActiveLine(),
      highlightSelectionMatches(),
      indentOnInput(),
      bracketMatching(),
      closeBrackets(),
      langExtension,
      mintTheme,
      syntaxHighlighting(mintHighlightStyle),
      syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
      inputListener,
      txListener,
      errorLineField,
      keymap.of([
        ...defaultKeymap,
        ...historyKeymap,
        ...foldKeymap,
        ...searchKeymap,
        ...closeBracketsKeymap,
        indentWithTab,
      ]),
      EditorView.lineWrapping,
    ],
  });

  return new EditorView({ state, parent });
}

export function setLanguage(view: EditorView, language: SupportedLanguage): void {
  const langExtension = languageExtensions[language]?.() ?? python();

  // Reconfigure by creating a new state with the current doc
  const newState = EditorState.create({
    doc: view.state.doc.toString(),
    extensions: [
      lineNumbers(),
      highlightActiveLineGutter(),
      highlightSpecialChars(),
      history(),
      foldGutter(),
      drawSelection(),
      rectangularSelection(),
      crosshairCursor(),
      highlightActiveLine(),
      highlightSelectionMatches(),
      indentOnInput(),
      bracketMatching(),
      closeBrackets(),
      langExtension,
      mintTheme,
      syntaxHighlighting(mintHighlightStyle),
      syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
      errorLineField,
      keymap.of([
        ...defaultKeymap,
        ...historyKeymap,
        ...foldKeymap,
        ...searchKeymap,
        ...closeBracketsKeymap,
        indentWithTab,
      ]),
      EditorView.lineWrapping,
    ],
  });

  view.setState(newState);
}
