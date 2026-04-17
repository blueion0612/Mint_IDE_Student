import { EditorView } from "@codemirror/view";
import { createEditor, type SupportedLanguage } from "./setup";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { recordTransaction, setCurrentFile, markNextInputSource } from "../monitor/edithistory";
import { handleEditorInput } from "../monitor/keystroke";

// ===== Types =====
interface NotebookCell {
  cell_type: "code" | "markdown" | "raw";
  source: string[];
  metadata: Record<string, any>;
  outputs?: any[];
  execution_count?: number | null;
}

interface NotebookJSON {
  cells: NotebookCell[];
  metadata: Record<string, any>;
  nbformat: number;
  nbformat_minor: number;
}

interface CellState {
  type: "code" | "markdown";
  editor: EditorView | null;
  element: HTMLElement;
  output: string;
  running: boolean;
}

// ===== State =====
let cells: CellState[] = [];
let notebookPath = "";
let onModified: (() => void) | null = null;
const CELL_MARKER = "__MINT_CELL_";

// ===== Public API =====

export function mountNotebook(
  container: HTMLElement,
  content: string,
  filePath: string,
  onMod: () => void,
): void {
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

  let nb: NotebookJSON;
  try {
    nb = JSON.parse(content);
  } catch {
    nb = { cells: [{ cell_type: "code", source: [""], metadata: {}, outputs: [] }], metadata: {}, nbformat: 4, nbformat_minor: 5 };
  }

  const cellsContainer = document.createElement("div");
  cellsContainer.className = "nb-cells";
  cellsContainer.id = "nb-cells";

  for (const cell of nb.cells) {
    if (cell.cell_type === "code" || cell.cell_type === "markdown") {
      const source = Array.isArray(cell.source) ? cell.source.join("") : String(cell.source);
      const outputs = cell.outputs || [];
      addCellToDOM(cellsContainer, cell.cell_type, source, formatOutputs(outputs));
    }
  }

  wrapper.appendChild(cellsContainer);
  container.appendChild(wrapper);

  document.getElementById("nb-run-all")!.addEventListener("click", runAllCellsSequential);
  document.getElementById("nb-add-code")!.addEventListener("click", () => {
    addCellToDOM(document.getElementById("nb-cells")!, "code", "", "");
    onModified?.();
  });
  document.getElementById("nb-add-md")!.addEventListener("click", () => {
    addCellToDOM(document.getElementById("nb-cells")!, "markdown", "", "");
    onModified?.();
  });
}

export function getNotebookJSON(): string {
  const nbCells: NotebookCell[] = cells.map((c) => {
    const source = c.editor ? c.editor.state.doc.toString() : "";
    return {
      cell_type: c.type,
      source: source.split("\n").map((line, i, arr) => i < arr.length - 1 ? line + "\n" : line),
      metadata: {},
      outputs: c.type === "code" && c.output ? [{
        output_type: "stream", name: "stdout",
        text: c.output.split("\n").map((l, i, a) => i < a.length - 1 ? l + "\n" : l),
      }] : [],
      execution_count: null,
    };
  });

  return JSON.stringify({
    cells: nbCells,
    metadata: { kernelspec: { display_name: "Python 3", language: "python", name: "python3" }, language_info: { name: "python" } },
    nbformat: 4, nbformat_minor: 5,
  }, null, 1);
}

export function isNotebookActive(): boolean { return cells.length > 0; }
export function clearNotebook(): void { cells = []; notebookPath = ""; onModified = null; }

// ===== Cell Management =====

function addCellToDOM(container: HTMLElement, type: "code" | "markdown", source: string, output: string): void {
  const idx = cells.length;
  const el = document.createElement("div");
  el.className = `nb-cell nb-cell-${type}`;
  el.dataset.idx = String(idx);

  el.innerHTML = `
    <div class="nb-cell-header">
      <span class="nb-cell-badge">${type === "code" ? "Code" : "Md"}</span>
      <span class="nb-cell-number">[${idx + 1}]</span>
      <div class="nb-cell-actions">
        ${type === "code" ? '<button class="nb-cell-btn nb-run-cell" title="Run Cell">&#9654;</button>' : ""}
        <button class="nb-cell-btn nb-del-cell" title="Delete Cell">&#10005;</button>
      </div>
    </div>
    <div class="nb-cell-editor"></div>
    <div class="nb-cell-output">${output ? escHtml(output) : ""}</div>
  `;

  container.appendChild(el);

  const editorContainer = el.querySelector(".nb-cell-editor") as HTMLElement;
  const editor = createEditor(
    editorContainer, "python" as SupportedLanguage, source,
    (event) => {
      if (event.inputType === "insertFromPaste") markNextInputSource("paste");
      handleEditorInput(event);
      onModified?.();
    },
    (changes, userEvent) => {
      setCurrentFile(`${notebookPath}#cell${idx}`);
      recordTransaction(changes, userEvent);
    },
  );

  const cellState: CellState = { type, editor, element: el, output, running: false };
  cells.push(cellState);

  const runBtn = el.querySelector(".nb-run-cell");
  if (runBtn) runBtn.addEventListener("click", () => runSingleCell(idx));

  el.querySelector(".nb-del-cell")!.addEventListener("click", () => {
    cells.splice(idx, 1);
    el.remove();
    renumberCells();
    onModified?.();
  });
}

function renumberCells(): void {
  const container = document.getElementById("nb-cells");
  if (!container) return;
  container.querySelectorAll(".nb-cell").forEach((el, i) => {
    const num = el.querySelector(".nb-cell-number");
    if (num) num.textContent = `[${i + 1}]`;
    (el as HTMLElement).dataset.idx = String(i);
  });
}

// ===== Colab-style Execution =====

/**
 * Run all code cells sequentially using cell markers.
 * Concatenates all code with print("__MINT_CELL_N__") between cells,
 * runs as one script, then splits output by markers.
 */
async function runAllCellsSequential(): Promise<void> {
  const codeCells: { idx: number; cell: CellState }[] = [];
  cells.forEach((c, i) => { if (c.type === "code") codeCells.push({ idx: i, cell: c }); });
  if (codeCells.length === 0) return;

  // Show running state
  for (const { cell } of codeCells) {
    const out = cell.element.querySelector(".nb-cell-output") as HTMLElement;
    out.textContent = "Running...";
    out.className = "nb-cell-output running";
    cell.output = "";
  }

  // Build combined script with markers
  let script = "";
  for (let i = 0; i < codeCells.length; i++) {
    script += `print("${CELL_MARKER}${i}__")\n`;
    script += (codeCells[i].cell.editor?.state.doc.toString() || "") + "\n";
  }

  // Collect all output
  let allOutput = "";
  let allStderr = "";

  const unlisten1 = await listen<{ stream: string; text: string }>("run-output", (event) => {
    if (event.payload.stream === "stderr") {
      allStderr += event.payload.text;
    } else {
      allOutput += event.payload.text;
    }
  });

  const done = new Promise<void>((resolve) => {
    listen<any>("run-done", () => {
      resolve();
    }).then(u => { setTimeout(() => u(), 100); });
  });

  await invoke("run_code", {
    language: "python",
    code: script,
    filename: notebookPath.replace(".ipynb", "_nb.py"),
    pythonPath: null,
  });

  await done;
  unlisten1();

  // Parse output by markers
  const chunks: string[] = [];
  const parts = allOutput.split(new RegExp(`${CELL_MARKER}(\\d+)__\\n?`));

  // parts: ["", "0", "output0\n", "1", "output1\n", ...]
  for (let i = 1; i < parts.length; i += 2) {
    const cellIdx = parseInt(parts[i]);
    const text = (parts[i + 1] || "").trim();
    chunks[cellIdx] = text;
  }

  // Assign output to cells
  for (let i = 0; i < codeCells.length; i++) {
    const { cell } = codeCells[i];
    const outEl = cell.element.querySelector(".nb-cell-output") as HTMLElement;
    const text = chunks[i] || "";

    // Check for images (matplotlib savefig)
    const imgHtml = detectImages(text);

    cell.output = text;
    if (imgHtml) {
      outEl.innerHTML = escHtml(text) + imgHtml;
    } else {
      outEl.textContent = text || "(no output)";
    }
    outEl.className = "nb-cell-output success";
  }

  // Show stderr on last cell if any
  if (allStderr.trim()) {
    const lastCell = codeCells[codeCells.length - 1].cell;
    const outEl = lastCell.element.querySelector(".nb-cell-output") as HTMLElement;
    outEl.innerHTML = (outEl.innerHTML || "") + `<div class="nb-stderr">${escHtml(allStderr)}</div>`;
    outEl.className = "nb-cell-output error";
  }
}

/** Run a single cell (runs all cells up to this one for context) */
async function runSingleCell(targetIdx: number): Promise<void> {
  // Run all code cells up to and including targetIdx
  const codeCells: { idx: number; cell: CellState }[] = [];
  cells.forEach((c, i) => {
    if (c.type === "code" && i <= targetIdx) codeCells.push({ idx: i, cell: c });
  });
  if (codeCells.length === 0) return;

  const targetCell = cells[targetIdx];
  const outEl = targetCell.element.querySelector(".nb-cell-output") as HTMLElement;
  outEl.textContent = "Running...";
  outEl.className = "nb-cell-output running";
  targetCell.output = "";

  // Build script: all previous cells + target cell with marker
  let script = "";
  for (let i = 0; i < codeCells.length; i++) {
    const code = codeCells[i].cell.editor?.state.doc.toString() || "";
    if (codeCells[i].idx === targetIdx) {
      script += `print("${CELL_MARKER}TARGET__")\n`;
    }
    script += code + "\n";
  }

  let allOutput = "";
  let allStderr = "";

  const unlisten1 = await listen<{ stream: string; text: string }>("run-output", (event) => {
    if (event.payload.stream === "stderr") allStderr += event.payload.text;
    else allOutput += event.payload.text;
  });

  const done = new Promise<void>((resolve) => {
    listen<any>("run-done", () => resolve()).then(u => { setTimeout(() => u(), 100); });
  });

  await invoke("run_code", {
    language: "python",
    code: script,
    filename: notebookPath.replace(".ipynb", "_cell.py"),
    pythonPath: null,
  });

  await done;
  unlisten1();

  // Extract output after TARGET marker
  const markerIdx = allOutput.indexOf(`${CELL_MARKER}TARGET__`);
  const text = markerIdx >= 0
    ? allOutput.substring(markerIdx + `${CELL_MARKER}TARGET__`.length).trim()
    : allOutput.trim();

  const imgHtml = detectImages(text);
  targetCell.output = text;
  if (imgHtml) {
    outEl.innerHTML = escHtml(text) + imgHtml;
  } else {
    outEl.textContent = text || "(no output)";
  }
  outEl.className = allStderr.trim() ? "nb-cell-output error" : "nb-cell-output success";

  if (allStderr.trim()) {
    outEl.innerHTML = (outEl.innerHTML || "") + `<div class="nb-stderr">${escHtml(allStderr)}</div>`;
  }
}

// ===== Image Detection =====

function detectImages(text: string): string {
  // Look for "Saved xxx.png" or file paths ending in .png
  const imgMatches = text.match(/[\w/\\.-]+\.png/gi);
  if (!imgMatches) return "";

  let html = "";
  for (const imgPath of imgMatches) {
    const name = imgPath.split(/[/\\]/).pop() || imgPath;
    html += `<div class="nb-cell-image"><img src="" data-path="${escAttr(name)}" class="nb-img-pending" /><div class="nb-img-label">${escHtml(name)}</div></div>`;
  }

  // Load images after DOM update
  setTimeout(async () => {
    const pending = document.querySelectorAll(".nb-img-pending");
    for (const img of pending) {
      const path = (img as HTMLElement).dataset.path || "";
      try {
        const base64 = await invoke<string>("ws_read_file_base64", { path });
        (img as HTMLImageElement).src = `data:image/png;base64,${base64}`;
        img.classList.remove("nb-img-pending");
      } catch { /* file not in workspace */ }
    }
  }, 100);

  return html;
}

// ===== Helpers =====

function formatOutputs(outputs: any[]): string {
  if (!outputs || outputs.length === 0) return "";
  return outputs.map((o) => {
    if (o.text) return Array.isArray(o.text) ? o.text.join("") : String(o.text);
    if (o.data?.["text/plain"]) {
      const d = o.data["text/plain"];
      return Array.isArray(d) ? d.join("") : String(d);
    }
    return "";
  }).join("\n");
}

function escHtml(text: string): string {
  const d = document.createElement("div");
  d.textContent = text;
  return d.innerHTML;
}

function escAttr(text: string): string {
  return text.replace(/"/g, "&quot;");
}
