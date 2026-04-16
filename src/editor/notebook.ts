import { EditorView } from "@codemirror/view";
import { createEditor, type SupportedLanguage } from "./setup";
import { invoke } from "@tauri-apps/api/core";
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

  // Toolbar
  const toolbar = document.createElement("div");
  toolbar.className = "nb-toolbar";
  toolbar.innerHTML = `
    <button class="btn btn-run nb-btn" id="nb-run-all">&#9654; Run All</button>
    <button class="btn nb-btn" id="nb-add-code">+ Code</button>
    <button class="btn nb-btn" id="nb-add-md">+ Markdown</button>
  `;
  wrapper.appendChild(toolbar);

  // Parse notebook
  let nb: NotebookJSON;
  try {
    nb = JSON.parse(content);
  } catch {
    nb = { cells: [{ cell_type: "code", source: [""], metadata: {}, outputs: [] }], metadata: {}, nbformat: 4, nbformat_minor: 5 };
  }

  // Render cells
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

  // Events
  document.getElementById("nb-run-all")!.addEventListener("click", runAllCells);
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
        output_type: "stream",
        name: "stdout",
        text: c.output.split("\n").map((l, i, a) => i < a.length - 1 ? l + "\n" : l),
      }] : [],
      execution_count: null,
    };
  });

  const nb: NotebookJSON = {
    cells: nbCells,
    metadata: {
      kernelspec: { display_name: "Python 3", language: "python", name: "python3" },
      language_info: { name: "python", version: "3.12" },
    },
    nbformat: 4,
    nbformat_minor: 5,
  };

  return JSON.stringify(nb, null, 1);
}

export function isNotebookActive(): boolean {
  return cells.length > 0;
}

export function clearNotebook(): void {
  cells = [];
  notebookPath = "";
  onModified = null;
}

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

  // Mount CodeMirror in cell
  const editorContainer = el.querySelector(".nb-cell-editor") as HTMLElement;
  const lang: SupportedLanguage = type === "code" ? "python" : "python"; // markdown uses plain text but python highlighting is ok for now

  const editor = createEditor(
    editorContainer, lang, source,
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

  // Run cell button
  const runBtn = el.querySelector(".nb-run-cell");
  if (runBtn) {
    runBtn.addEventListener("click", () => runCell(idx));
  }

  // Delete cell button
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
  const cellEls = container.querySelectorAll(".nb-cell");
  cellEls.forEach((el, i) => {
    const num = el.querySelector(".nb-cell-number");
    if (num) num.textContent = `[${i + 1}]`;
    (el as HTMLElement).dataset.idx = String(i);
  });
}

// ===== Execution =====

async function runCell(idx: number): Promise<void> {
  const cell = cells[idx];
  if (!cell || cell.type !== "code" || cell.running) return;

  const code = cell.editor?.state.doc.toString() || "";
  if (!code.trim()) return;

  cell.running = true;
  const outputEl = cell.element.querySelector(".nb-cell-output") as HTMLElement;
  outputEl.textContent = "Running...";
  outputEl.className = "nb-cell-output running";

  try {
    const result = await invoke<{ stdout: string; stderr: string; exit_code: number | null }>("run_code", {
      language: "python",
      code,
      filename: notebookPath.replace(".ipynb", "_cell.py"),
      pythonPath: null,
    });

    let out = "";
    if (result.stdout) out += result.stdout;
    if (result.stderr) out += result.stderr;
    cell.output = out;
    outputEl.textContent = out || "(no output)";
    outputEl.className = `nb-cell-output ${result.exit_code === 0 ? "success" : "error"}`;
  } catch (e) {
    cell.output = String(e);
    outputEl.textContent = String(e);
    outputEl.className = "nb-cell-output error";
  }

  cell.running = false;
}

async function runAllCells(): Promise<void> {
  // Run cells sequentially — each cell can depend on previous state
  // We concatenate all code cells and run as one script, then attribute output
  const codeCells = cells.filter((c) => c.type === "code");
  if (codeCells.length === 0) return;

  // Simple approach: run all code cells as one script
  const allCode = codeCells.map((c) => c.editor?.state.doc.toString() || "").join("\n\n");

  // Show running state
  for (const c of codeCells) {
    const out = c.element.querySelector(".nb-cell-output") as HTMLElement;
    out.textContent = "Running...";
    out.className = "nb-cell-output running";
  }

  try {
    const result = await invoke<{ stdout: string; stderr: string; exit_code: number | null }>("run_code", {
      language: "python",
      code: allCode,
      filename: notebookPath.replace(".ipynb", "_all.py"),
      pythonPath: null,
    });

    let out = "";
    if (result.stdout) out += result.stdout;
    if (result.stderr) out += result.stderr;

    // Show output on the last code cell
    for (const c of codeCells) {
      const outEl = c.element.querySelector(".nb-cell-output") as HTMLElement;
      outEl.textContent = "";
      outEl.className = "nb-cell-output";
    }

    const lastCell = codeCells[codeCells.length - 1];
    const lastOut = lastCell.element.querySelector(".nb-cell-output") as HTMLElement;
    lastOut.textContent = out || "(no output)";
    lastOut.className = `nb-cell-output ${result.exit_code === 0 ? "success" : "error"}`;
    lastCell.output = out;
  } catch (e) {
    const lastCell = codeCells[codeCells.length - 1];
    const lastOut = lastCell.element.querySelector(".nb-cell-output") as HTMLElement;
    lastOut.textContent = String(e);
    lastOut.className = "nb-cell-output error";
  }
}

// ===== Helpers =====

function formatOutputs(outputs: any[]): string {
  if (!outputs || outputs.length === 0) return "";
  return outputs.map((o) => {
    if (o.text) return Array.isArray(o.text) ? o.text.join("") : String(o.text);
    if (o.data && o.data["text/plain"]) {
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
