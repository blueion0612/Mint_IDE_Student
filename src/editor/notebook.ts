import { EditorView } from "@codemirror/view";
import { createEditor, type SupportedLanguage } from "./setup";
import { invoke } from "@tauri-apps/api/core";
import { recordTransaction, setCurrentFile, markNextInputSource } from "../monitor/edithistory";
import { handleEditorInput } from "../monitor/keystroke";

interface CellState {
  type: "code" | "markdown";
  editor: EditorView | null;
  element: HTMLElement;
  output: string;
  running: boolean;
}

let cells: CellState[] = [];
let notebookPath = "";
let onModified: (() => void) | null = null;
const CELL_MARKER = "__MINT_CELL_";

let nbRunning = false;
export function isNotebookRunning(): boolean { return nbRunning; }
export function isNotebookActive(): boolean { return cells.length > 0; }
export function clearNotebook(): void { cells = []; notebookPath = ""; onModified = null; }

export function getNotebookJSON(): string {
  const nbCells = cells.map((c) => {
    const source = c.editor ? c.editor.state.doc.toString() : "";
    return {
      cell_type: c.type,
      source: source.split("\n").map((line, i, arr) => i < arr.length - 1 ? line + "\n" : line),
      metadata: {},
      outputs: c.type === "code" && c.output ? [{ output_type: "stream", name: "stdout", text: [c.output] }] : [],
      execution_count: null,
    };
  });
  return JSON.stringify({
    cells: nbCells,
    metadata: { kernelspec: { display_name: "Python 3", language: "python", name: "python3" }, language_info: { name: "python" } },
    nbformat: 4, nbformat_minor: 5,
  }, null, 1);
}

export function mountNotebook(container: HTMLElement, content: string, filePath: string, onMod: () => void): void {
  cells = [];
  notebookPath = filePath;
  onModified = onMod;
  container.innerHTML = "";

  const wrapper = document.createElement("div");
  wrapper.className = "notebook-container";

  const toolbar = document.createElement("div");
  toolbar.className = "nb-toolbar";
  toolbar.innerHTML = `
    <button class="btn btn-run nb-btn" id="nb-run-all">&#9654; Run All</button>
    <button class="btn nb-btn" id="nb-add-code">+ Code</button>
    <button class="btn nb-btn" id="nb-add-md">+ Markdown</button>
  `;
  wrapper.appendChild(toolbar);

  let nb: any;
  try { nb = JSON.parse(content); } catch { nb = { cells: [{ cell_type: "code", source: [""], metadata: {}, outputs: [] }] }; }

  const cellsContainer = document.createElement("div");
  cellsContainer.className = "nb-cells";
  cellsContainer.id = "nb-cells";

  for (const cell of (nb.cells || [])) {
    if (cell.cell_type === "code" || cell.cell_type === "markdown") {
      const src = Array.isArray(cell.source) ? cell.source.join("") : String(cell.source || "");
      addCellToDOM(cellsContainer, cell.cell_type, src, "");
    }
  }

  wrapper.appendChild(cellsContainer);
  container.appendChild(wrapper);

  document.getElementById("nb-run-all")!.addEventListener("click", runAllCells);
  document.getElementById("nb-add-code")!.addEventListener("click", () => { addCellToDOM(document.getElementById("nb-cells")!, "code", "", ""); onModified?.(); });
  document.getElementById("nb-add-md")!.addEventListener("click", () => { addCellToDOM(document.getElementById("nb-cells")!, "markdown", "", ""); onModified?.(); });
}

function addCellToDOM(container: HTMLElement, type: "code" | "markdown", source: string, output: string): void {
  const idx = cells.length;
  const el = document.createElement("div");
  el.className = `nb-cell nb-cell-${type}`;

  el.innerHTML = `
    <div class="nb-cell-header">
      <span class="nb-cell-badge">${type === "code" ? "Code" : "Md"}</span>
      <span class="nb-cell-number">[${idx + 1}]</span>
      <div class="nb-cell-actions">
        ${type === "code" ? '<button class="nb-cell-btn nb-run-cell" title="Run Cell">&#9654;</button>' : ""}
        <button class="nb-cell-btn nb-del-cell" title="Delete">&#10005;</button>
      </div>
    </div>
    <div class="nb-cell-editor"></div>
    <div class="nb-cell-output"></div>
  `;

  container.appendChild(el);

  const editor = createEditor(
    el.querySelector(".nb-cell-editor") as HTMLElement, "python" as SupportedLanguage, source,
    (event) => { if (event.inputType === "insertFromPaste") markNextInputSource("paste"); handleEditorInput(event); onModified?.(); },
    (changes, userEvent) => { setCurrentFile(`${notebookPath}#cell${idx}`); recordTransaction(changes, userEvent); },
  );

  cells.push({ type, editor, element: el, output, running: false });

  el.querySelector(".nb-run-cell")?.addEventListener("click", () => runSingleCell(idx));
  el.querySelector(".nb-del-cell")!.addEventListener("click", () => {
    const i = cells.indexOf(cells.find(c => c.element === el)!);
    if (i >= 0) { cells.splice(i, 1); el.remove(); renumberCells(); onModified?.(); }
  });
}

function renumberCells(): void {
  document.getElementById("nb-cells")?.querySelectorAll(".nb-cell").forEach((el, i) => {
    const num = el.querySelector(".nb-cell-number");
    if (num) num.textContent = `[${i + 1}]`;
  });
}

// ===== Execution (blocking, no event conflicts) =====

async function runAllCells(): Promise<void> {
  const codeCells: CellState[] = cells.filter(c => c.type === "code");
  if (codeCells.length === 0 || nbRunning) return;
  nbRunning = true;

  for (const cell of codeCells) {
    const out = cell.element.querySelector(".nb-cell-output") as HTMLElement;
    out.textContent = "Running...";
    out.className = "nb-cell-output running";
  }

  let script = "";
  codeCells.forEach((cell, i) => {
    script += `print("${CELL_MARKER}${i}__", flush=True)\n`;
    script += (cell.editor?.state.doc.toString() || "") + "\n";
  });
  script += `print("${CELL_MARKER}END__", flush=True)\n`;

  try {
    const result = await invoke<[string, string, number | null]>("run_code_sync", {
      language: "python", code: script, filename: "_notebook_run.py", pythonPath: (window as any).getSelectedPythonPath?.() ?? null,
    });
    const stdout = result[0];
    const stderr = result[1];

    const parts = stdout.split(new RegExp(`${CELL_MARKER}(\\d+|END)__\\n?`));
    for (let i = 1; i < parts.length; i += 2) {
      if (parts[i] === "END") break;
      const ci = parseInt(parts[i]);
      const text = (parts[i + 1] || "").trim();
      if (ci >= 0 && ci < codeCells.length) {
        const outEl = codeCells[ci].element.querySelector(".nb-cell-output") as HTMLElement;
        codeCells[ci].output = text;
        outEl.innerHTML = esc(text || "(no output)") + detectImages(text);
        outEl.className = "nb-cell-output success";
      }
    }

    if (stderr.trim()) {
      // Find which cell caused the error by parsing "line N" from stderr
      let errorCellIdx = codeCells.length - 1; // default: last cell
      const lineMatch = stderr.match(/line (\d+)/);
      if (lineMatch) {
        const errLine = parseInt(lineMatch[1]);
        // Map script line back to cell index using cumulative line counts
        let cumLines = 1; // marker line
        for (let ci = 0; ci < codeCells.length; ci++) {
          const cellLines = (codeCells[ci].editor?.state.doc.toString() || "").split("\n").length + 1; // +1 for marker
          if (errLine <= cumLines + cellLines) { errorCellIdx = ci; break; }
          cumLines += cellLines;
        }
      }
      const errCell = codeCells[errorCellIdx].element.querySelector(".nb-cell-output") as HTMLElement;
      errCell.innerHTML += `<div class="nb-stderr">${esc(stderr)}</div>`;
      // Keep stdout part as success, only add stderr indicator
    }
  } catch (e) {
    const last = codeCells[codeCells.length - 1].element.querySelector(".nb-cell-output") as HTMLElement;
    last.textContent = String(e);
    last.className = "nb-cell-output error";
  }

  nbRunning = false;
}

async function runSingleCell(targetIdx: number): Promise<void> {
  if (nbRunning || cells[targetIdx]?.type !== "code") return;
  nbRunning = true;

  const outEl = cells[targetIdx].element.querySelector(".nb-cell-output") as HTMLElement;
  outEl.textContent = "Running...";
  outEl.className = "nb-cell-output running";

  let script = "";
  for (let i = 0; i <= targetIdx; i++) {
    if (cells[i].type !== "code") continue;
    if (i === targetIdx) script += `print("${CELL_MARKER}TARGET__", flush=True)\n`;
    script += (cells[i].editor?.state.doc.toString() || "") + "\n";
  }

  try {
    const result = await invoke<[string, string, number | null]>("run_code_sync", {
      language: "python", code: script, filename: "_notebook_cell.py", pythonPath: (window as any).getSelectedPythonPath?.() ?? null,
    });
    const stdout = result[0];
    const stderr = result[1];

    const pos = stdout.indexOf(`${CELL_MARKER}TARGET__`);
    const text = pos >= 0 ? stdout.substring(pos + `${CELL_MARKER}TARGET__`.length).trim() : stdout.trim();

    cells[targetIdx].output = text;
    outEl.innerHTML = esc(text || "(no output)") + detectImages(text);
    outEl.className = "nb-cell-output success";
    if (stderr.trim()) outEl.innerHTML += `<div class="nb-stderr">${esc(stderr)}</div>`;
  } catch (e) {
    outEl.textContent = String(e);
    outEl.className = "nb-cell-output error";
  }

  nbRunning = false;
}

function detectImages(text: string): string {
  const matches = text.match(/[\w/\\.-]+\.png/gi);
  if (!matches) return "";
  let html = "";
  for (const img of matches) {
    const name = img.split(/[/\\]/).pop() || img;
    html += `<div class="nb-cell-image"><img data-path="${name}" class="nb-img-pending" /><div class="nb-img-label">${name}</div></div>`;
  }
  setTimeout(async () => {
    for (const el of document.querySelectorAll(".nb-img-pending")) {
      try {
        const b64 = await invoke<string>("ws_read_file_base64", { path: (el as HTMLElement).dataset.path });
        (el as HTMLImageElement).src = `data:image/png;base64,${b64}`;
        el.classList.remove("nb-img-pending");
      } catch {}
    }
  }, 100);
  return html;
}

function esc(t: string): string { const d = document.createElement("div"); d.textContent = t; return d.innerHTML; }
