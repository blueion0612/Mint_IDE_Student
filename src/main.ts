import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { createEditor, setLanguage, markErrorLines, clearErrors, type SupportedLanguage } from "./editor/setup";
import { handleEditorInput, flushTypingSummary, noteClipboardEvent } from "./monitor/keystroke";
import { recordTransaction, setCurrentFile, markNextInputSource, getEditHistoryJSON } from "./monitor/edithistory";
import { mountNotebook, getNotebookJSON, isNotebookActive, clearNotebook, isNotebookRunning, stopNotebook } from "./editor/notebook";
import { showSetupWizard, showSettingsModal, loadConfig, type SetupConfig } from "./setup_wizard";

// ===== Types =====
interface FileNode {
  name: string;
  path: string;
  is_dir: boolean;
  children: FileNode[];
}

interface ActivityEvent {
  timestamp: string;
  epoch_ms: number;
  event_type: string;
  detail: string;
  char_count: number | null;
  time_delta_ms: number | null;
  severity: string;
}

interface OpenFile {
  path: string;
  name: string;
  language: SupportedLanguage;
  content: string;
  modified: boolean;
  // Oversized file opened as an info panel only (never mounted into
  // CodeMirror / the table parser — a 100MB doc froze low-spec machines).
  tooLarge?: boolean;
  sizeBytes?: number;
}

// Above this size a file opens as an info panel instead of an editor/table.
const MAX_PREVIEW_BYTES = 20 * 1024 * 1024;
// Rows rendered (and parsed) by the CSV/TSV table viewer.
const MAX_TABLE_ROWS = 500;

// ===== State =====
let openFiles: OpenFile[] = [];
let activeFilePath: string | null = null;
let editorView: ReturnType<typeof createEditor> | null = null;
let warningCount = 0;
let isRunning = false;
// Tracks whether any "run-output" stream chunk arrived during the current
// Run. Used by the run-done handler to decide if the fallback collected-
// output dump is needed. Reset to false at the start of every Run.
let streamingOutputReceived = false;
let isRecording = false;
let workspaceRoot = "";
let studentId = "";
let selectedPythonPath: string | null = null; // null = system default
let setupConfig: SetupConfig = {
  setup_done: false,
  package_profile: "basic",
  custom_packages: [],
  recording_enabled: true,
  include_sample_code: true,
  config_version: 1,
};
// Expose for notebook.ts
(window as any).getSelectedPythonPath = () => selectedPythonPath;
// Expose for setup_wizard.ts (settings modal sample-create button)
(window as any).__mintCreateSampleFiles = async () => { await createSampleFiles(); };
(window as any).__mintRefreshFileTree = async () => { await refreshFileTree(); };

// ===== Initialization =====
document.addEventListener("DOMContentLoaded", async () => {
  // Show student ID prompt first — blocks everything until entered
  showStudentIdModal();
});

function showStudentIdModal(): void {
  const overlay = document.createElement("div");
  overlay.id = "student-id-overlay";
  overlay.innerHTML = `
    <div class="modal">
      <div class="modal-logo">MINT Exam IDE</div>
      <div class="modal-title">학번을 입력하세요</div>
      <input type="text" id="student-id-input" class="modal-input" placeholder="예: 20240001" autocomplete="off" spellcheck="false" />
      <div class="modal-error" id="student-id-error"></div>
      <button class="btn btn-accent modal-btn" id="student-id-submit">Test Start (Screen Recording)</button>
    </div>
  `;
  document.body.appendChild(overlay);

  const input = document.getElementById("student-id-input") as HTMLInputElement;
  const btn = document.getElementById("student-id-submit")!;
  const error = document.getElementById("student-id-error")!;

  input.focus();

  const doSubmit = async () => {
    const val = input.value.trim();
    if (!val) {
      error.textContent = "학번을 입력해 주세요.";
      input.focus();
      return;
    }
    // ASCII alphanumeric, 4~20 chars. Prevents the student-id from breaking
    // path concatenation (Desktop folder, recording filenames, manifest).
    if (!/^[A-Za-z0-9]{4,20}$/.test(val)) {
      error.textContent = "학번은 영문/숫자 4~20자만 사용 가능합니다.";
      input.focus();
      return;
    }
    studentId = val;
    overlay.remove();

    setupConfig = await loadConfig();
    // Run the wizard when setup was never completed OR the exam venv is gone
    // (fresh/repaved machine with a stale setup_done=true config) — skipping it
    // then would silently hand the student a package-less environment.
    let venvReady = false;
    try { venvReady = await invoke<boolean>("exam_venv_ready"); } catch { /* treat as not ready */ }
    if (!setupConfig.setup_done || !venvReady) {
      // Prepare venv first so wizard installs packages into it (not system Python)
      try {
        selectedPythonPath = await invoke<string>("setup_exam_python");
      } catch (e) {
        console.warn("Exam venv prep failed:", e);
      }
      setupConfig = await showSetupWizard(selectedPythonPath);
    }

    await initializeApp();
  };

  btn.addEventListener("click", doSubmit);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") doSubmit();
  });
}

let appVersion = "";

async function initializeApp(): Promise<void> {
  // Version in the toolbar + OS title bar: instant visual check that the
  // LATEST build is installed (previously indistinguishable from an old one).
  try { appVersion = await invoke<string>("get_app_version"); } catch { /* keep blank */ }
  if (appVersion) {
    try {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      await getCurrentWindow().setTitle(`MINT Exam IDE v${appVersion}`);
    } catch { /* title stays default */ }
  }
  buildToolbar();
  buildStatusBar();
  setupLogPanel();
  setupOutputPanel();
  setupSidebarResize();
  listenForBackendEvents();

  document.addEventListener("click", closeContextMenu);

  // Suppress the WebView's NATIVE context menu everywhere. Its "새로 고침 /
  // Reload" item (WebView2 on Windows, WKWebView on macOS) reloads the page
  // and wipes the whole session (학번, open buffers, edit history) — the same
  // reload the F5/Ctrl+R blocks below exist to prevent. Our own file-tree
  // context menu is built by element-level handlers and is unaffected.
  document.addEventListener("contextmenu", (e) => e.preventDefault());

  // Keyboard shortcuts
  document.addEventListener("keydown", (e) => {
    // Block reload shortcuts — F5 / Ctrl+R / Ctrl+Shift+R / Ctrl+F5 — which
    // would wipe the student's session state (학번, 작업 파일, 로그 등).
    if (e.key === "F5") { e.preventDefault(); return; }

    if ((e.ctrlKey || e.metaKey) && (e.key === "s" || e.key === "S")) {
      e.preventDefault();
      if (!e.shiftKey) saveCurrentFile();
      return;
    }
    // Ctrl+R: run. Ctrl+Shift+R: blocked (would otherwise reload the WebView).
    if ((e.ctrlKey || e.metaKey) && (e.key === "r" || e.key === "R")) {
      e.preventDefault();
      if (!e.shiftKey) runCurrentFile();
      return;
    }
    // Ctrl+Shift+C — emergency stop (bypasses event-flooded UI)
    if ((e.ctrlKey || e.metaKey) && e.shiftKey && (e.key === "C" || e.key === "c")) {
      e.preventDefault();
      if (isRunning) stopCurrentRun();
      if (isNotebookRunning()) stopNotebook();
      return;
    }
  });

  document.getElementById("btn-sidebar-new-file")!.addEventListener("click", () => promptNewFile(""));
  document.getElementById("btn-sidebar-new-folder")!.addEventListener("click", () => promptNewFolder(""));
  document.getElementById("btn-sidebar-new-notebook")!.addEventListener("click", () => promptNewNotebook(""));
  document.getElementById("btn-sidebar-import")!.addEventListener("click", () => importExternalFile(""));

  const session = `${studentId}_${new Date().toISOString().slice(0, 19).replace(/[:-]/g, "")}`;
  try {
    workspaceRoot = await invoke<string>("init_workspace", { sessionName: session });

    if (setupConfig.include_sample_code) {
      await createSampleFiles();
    } else {
      await invoke("ws_write_file", { path: "main.py", content: EMPTY_MAIN_PY });
    }

    await refreshFileTree();
    openFileByPath("main.py");
  } catch (e) {
    // A failed workspace init leaves EVERY ws_* command broken — nothing
    // saves, submit cannot work. Silently logging to a console the student
    // can't see meant they'd discover it only at submit time.
    console.error("Workspace init failed:", e);
    alert(
      `[치명적] 작업 폴더를 만들지 못했습니다.\n\n${e}\n\n` +
      `이 상태로는 파일 저장과 제출이 동작하지 않습니다. ` +
      `감독관에게 즉시 알리고 IDE를 재시작하세요.`
    );
  }

  invoke("log_editor_event", {
    eventType: "session_start",
    detail: `Session started. Student: ${studentId}, Workspace: ${workspaceRoot}`,
    charCount: null,
    timeDeltaMs: null,
  });

  if (setupConfig.recording_enabled) {
    startAutoRecording();
  } else {
    const indicator = document.getElementById("rec-indicator");
    if (indicator) {
      indicator.textContent = "REC: OFF";
      indicator.classList.add("rec-disabled");
      indicator.title = "Recording disabled in settings";
    }
  }

  if (selectedPythonPath) {
    const pyEl = document.getElementById("status-python");
    if (pyEl) pyEl.textContent = "Python: Exam Env";
  } else {
    setupExamPython();
  }

  registerCloseGuard();
}

// Confirm + best-effort flush before the window closes (X / Alt+F4). Without
// this, an accidental close mid-exam silently discards unsaved buffers AND the
// entire in-memory edit-history (a grading artifact only persisted on submit).
let closeInProgress = false;
async function registerCloseGuard(): Promise<void> {
  try {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const win = getCurrentWindow();
    await win.onCloseRequested(async (event) => {
      if (closeInProgress) return;         // already flushing → let it proceed
      event.preventDefault();              // we control the actual close
      if (isSubmitting) return;            // submit runs its own exit(0); block manual close meanwhile
      const ok = confirm(
        "시험을 종료하시겠습니까?\n\n" +
        "제출하지 않고 종료하면 이번 세션의 편집 기록이 저장되지 않습니다.\n" +
        "제출하려면 [취소]를 누르고 Submit 버튼을 사용하세요."
      );
      if (!ok) return;                     // stay open
      closeInProgress = true;
      // Stop live children so nothing is orphaned past the exit (the backend
      // Exit hook is the guarantee; this keeps their final writes out of the
      // buffer flush below).
      if (isRunning) stopCurrentRun();
      if (isNotebookRunning()) { try { await stopNotebook(); } catch { /* best effort */ } }
      try {
        await syncCurrentEditor();
        for (const f of openFiles) {
          if (f.path === activeFilePath && editorView) f.content = editorView.state.doc.toString();
          if (f.modified || (f.path === activeFilePath && editorView)) {
            await invoke("ws_write_file", { path: f.path, content: f.content });
            f.modified = false;
          }
        }
        await invoke("save_code_history", { historyJson: getEditHistoryJSON() });
      } catch { /* best effort — still close below */ }
      // Authorize the exit with the backend gate (which otherwise vetoes
      // ExitRequested — that veto is what blocks macOS Cmd+Q from bypassing
      // this guard entirely).
      try { await invoke("allow_exit"); } catch { /* proceed anyway */ }
      const { exit } = await import("@tauri-apps/plugin-process");
      await exit(0);
    });
  } catch (e) {
    console.warn("close guard registration failed:", e);
  }
}

async function createSampleFiles(): Promise<void> {
  await invoke("ws_write_file", { path: "main.py", content: DEFAULT_MAIN_PY });
  await invoke("ws_write_file", { path: "test_all.py", content: DEFAULT_TEST_PY });
  await invoke("ws_create_dir", { path: "utils" });
  await invoke("ws_write_file", { path: "utils/__init__.py", content: "from .math_helper import add, multiply\nfrom .text_helper import greet\n" });
  await invoke("ws_write_file", { path: "utils/math_helper.py", content: DEFAULT_MATH_HELPER });
  await invoke("ws_write_file", { path: "utils/text_helper.py", content: DEFAULT_TEXT_HELPER });
  await invoke("ws_write_file", { path: "test_import.py", content: DEFAULT_IMPORT_TEST });
  await invoke("ws_write_file", { path: "test_popup.py", content: DEFAULT_POPUP_TEST });
  await invoke("ws_write_file", { path: "test_notebook.ipynb", content: DEFAULT_NOTEBOOK });
  await refreshFileTree();
}

// ===== Toolbar =====
function buildToolbar(): void {
  const toolbar = document.getElementById("toolbar")!;
  toolbar.innerHTML = `
    <span class="toolbar-title">MINT Exam IDE${appVersion ? ` <span class="toolbar-version">v${escapeHtml(appVersion)}</span>` : ""}</span>
    <div class="toolbar-group">
      <select id="lang-selector" class="lang-select">
        <option value="python">Python</option>
        <option value="javascript">JavaScript</option>
        <option value="typescript">TypeScript</option>
        <option value="java">Java</option>
        <option value="c">C</option>
        <option value="cpp">C++</option>
      </select>
    </div>
    <div class="toolbar-separator"></div>
    <div class="toolbar-group">
      <button class="btn btn-run" id="btn-run">&#9654; Run</button>
      <button class="btn" id="btn-save">Save</button>
    </div>
    <div class="toolbar-separator"></div>
    <div class="toolbar-group">
      <span class="rec-indicator" id="rec-indicator">&#9679; REC</span>
    </div>
    <div class="toolbar-separator"></div>
    <div class="toolbar-group">
      <button class="btn btn-settings" id="btn-settings" title="설정 (Settings)">&#9881;</button>
      <button class="btn btn-submit" id="btn-submit">Submit</button>
    </div>
  `;

  document.getElementById("lang-selector")!.addEventListener("change", (e) => {
    const lang = (e.target as HTMLSelectElement).value as SupportedLanguage;
    const file = openFiles.find((f) => f.path === activeFilePath);
    if (file) {
      file.language = lang;
      if (editorView) setLanguage(editorView, lang);
    }
  });

  document.getElementById("btn-run")!.onclick = () => runCurrentFile();
  document.getElementById("btn-save")!.addEventListener("click", saveCurrentFile);
  document.getElementById("btn-submit")!.addEventListener("click", submitExam);
  document.getElementById("btn-settings")!.addEventListener("click", openSettingsModal);
}

async function openSettingsModal(): Promise<void> {
  const updated = await showSettingsModal(selectedPythonPath);
  if (!updated) return;
  const prevRecording = setupConfig.recording_enabled;
  setupConfig = updated;

  if (updated.recording_enabled && !prevRecording && !isRecording) {
    await startAutoRecording();
  } else if (!updated.recording_enabled && isRecording) {
    try {
      await invoke<string>("stop_recording");
      isRecording = false;
      const ind = document.getElementById("rec-indicator");
      if (ind) {
        ind.textContent = "REC: OFF";
        ind.classList.remove("recording");
        ind.classList.add("rec-disabled");
      }
    } catch (e) {
      console.warn("Stop recording failed:", e);
    }
  }
}

// ===== Workspace / File Tree =====
// Most actions trigger 2~3 refreshes (e.g. deleteItem → refreshFileTree, and
// openFileByPath → refreshFileTree). Each one is a full backend directory walk
// + JSON round-trip + complete DOM rebuild, which is visible on a low-spec
// machine once the workspace holds an imported dataset. Coalesce: at most one
// refresh in flight plus one queued. An awaited call still resolves only after
// a refresh that STARTED at or after the call, so callers that immediately
// query the freshly-rendered DOM (startRenameInSidebar) stay correct.
let treeRefreshInFlight: Promise<void> | null = null;
let treeRefreshQueued: Promise<void> | null = null;

function refreshFileTree(): Promise<void> {
  if (treeRefreshInFlight === null) {
    treeRefreshInFlight = doRefreshFileTree().finally(() => {
      treeRefreshInFlight = null;
    });
    return treeRefreshInFlight;
  }
  if (treeRefreshQueued === null) {
    treeRefreshQueued = treeRefreshInFlight.then(() => {
      treeRefreshQueued = null;
      return refreshFileTree();
    });
  }
  return treeRefreshQueued;
}

async function doRefreshFileTree(): Promise<void> {
  try {
    dropTargets.length = 0; // clear old drop targets before re-render
    const tree = await invoke<FileNode[]>("ws_list_tree");
    renderFileTree(tree, "");
  } catch (e) {
    console.error("Failed to refresh file tree:", e);
  }
}

function renderFileTree(nodes: FileNode[], parentPath: string): void {
  const container = parentPath === ""
    ? document.getElementById("file-tree")!
    : document.querySelector(`.tree-children[data-path="${CSS.escape(parentPath)}"]`);

  if (!container) return;
  if (parentPath === "") {
    container.innerHTML = "";
    // Root is a drop target for moving items to top level
    makeDropTarget(container as HTMLElement, "");
  }

  for (const node of nodes) {
    const item = document.createElement("div");
    item.className = "file-tree-item";
    item.dataset.path = node.path;

    if (node.is_dir) {
      item.innerHTML = `
        <div class="file-item dir${isExpanded(node.path) ? " expanded" : ""}" data-path="${escapeAttr(node.path)}">
          <span class="file-icon dir-arrow">${isExpanded(node.path) ? "&#9660;" : "&#9654;"}</span>
          <span class="file-name">${escapeHtml(node.name)}</span>
        </div>
        <div class="tree-children${isExpanded(node.path) ? "" : " hidden"}" data-path="${escapeAttr(node.path)}"></div>
      `;

      const dirRow = item.querySelector(".file-item.dir") as HTMLElement;
      dirRow.addEventListener("click", () => {
        if (dragState !== null) return;
        toggleDir(node.path);
      });
      dirRow.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        showContextMenu((e as MouseEvent).clientX, (e as MouseEvent).clientY, node.path, true);
      });
      makeDraggable(dirRow, node.path);
      makeDropTarget(dirRow, node.path);

      container.appendChild(item);

      if (isExpanded(node.path) && node.children.length > 0) {
        const childContainer = item.querySelector(".tree-children") as HTMLElement;
        makeDropTarget(childContainer, node.path);
        for (const child of node.children) {
          renderNode(child, childContainer as HTMLElement);
        }
      }
    } else {
      renderNode(node, container as HTMLElement);
    }
  }
}

function renderNode(node: FileNode, container: HTMLElement): void {
  if (node.is_dir) {
    const wrapper = document.createElement("div");
    wrapper.className = "file-tree-item";
    wrapper.innerHTML = `
      <div class="file-item dir${isExpanded(node.path) ? " expanded" : ""}" data-path="${escapeAttr(node.path)}">
        <span class="file-icon dir-arrow">${isExpanded(node.path) ? "&#9660;" : "&#9654;"}</span>
        <span class="file-name">${escapeHtml(node.name)}</span>
      </div>
      <div class="tree-children${isExpanded(node.path) ? "" : " hidden"}" data-path="${escapeAttr(node.path)}"></div>
    `;
    const dirRow = wrapper.querySelector(".file-item.dir") as HTMLElement;
    dirRow.addEventListener("click", () => {
      // Don't toggle folder if we just finished a drag
      if (dragState !== null) return;
      toggleDir(node.path);
    });
    dirRow.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      showContextMenu((e as MouseEvent).clientX, (e as MouseEvent).clientY, node.path, true);
    });
    makeDraggable(dirRow, node.path);
    makeDropTarget(dirRow, node.path);
    container.appendChild(wrapper);

    if (isExpanded(node.path)) {
      const childEl = wrapper.querySelector(".tree-children") as HTMLElement;
      makeDropTarget(childEl, node.path);
      for (const child of node.children) {
        renderNode(child, childEl);
      }
    }
  } else {
    const isModified = openFiles.find((f) => f.path === node.path)?.modified ?? false;
    const el = document.createElement("div");
    el.className = `file-item file${node.path === activeFilePath ? " active" : ""}`;
    el.dataset.path = node.path;
    el.innerHTML = `
      <span class="file-icon">${iconForExt(node.name)}</span>
      <span class="file-name">${escapeHtml(node.name)}</span>
      ${isModified ? '<span class="file-modified-dot"></span>' : ""}
    `;
    el.addEventListener("click", () => openFileByPath(node.path));
    el.addEventListener("dblclick", (e) => {
      e.preventDefault();
      startRenameInSidebar(node.path);
    });
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      showContextMenu((e as MouseEvent).clientX, (e as MouseEvent).clientY, node.path, false);
    });
    // Drag: files are draggable
    makeDraggable(el, node.path);
    container.appendChild(el);
  }
}

const expandedDirs = new Set<string>();

function isExpanded(path: string): boolean {
  return expandedDirs.has(path);
}

async function toggleDir(path: string): Promise<void> {
  if (expandedDirs.has(path)) {
    expandedDirs.delete(path);
  } else {
    expandedDirs.add(path);
  }
  await refreshFileTree();
}

function iconForExt(name: string): string {
  const ext = name.split(".").pop()?.toLowerCase() || "";
  const map: Record<string, string> = {
    py: "Py", js: "JS", ts: "TS", java: "Jv", c: "C", cpp: "C+", h: "H",
    hpp: "H+", json: "{}", txt: "Tx", md: "Md", ipynb: "Nb",
    png: "Ig", jpg: "Ig", jpeg: "Ig", gif: "Ig", svg: "Ig", webp: "Ig", bmp: "Ig",
    csv: "Cs", xml: "Xm", html: "Ht", css: "Ss",
  };
  return map[ext] || "??";
}

// ===== Open / Save Files =====
async function openFileByPath(path: string): Promise<void> {
  // Save current editor content
  await syncCurrentEditor();

  const ext = path.split(".").pop()?.toLowerCase() || "";
  const imageExts = ["png", "jpg", "jpeg", "gif", "bmp", "svg", "webp"];
  const tableExts = ["csv", "tsv"];
  const excelExts = ["xlsx", "xls"];
  const binaryExts = ["docx", "pdf", "zip", "tar", "gz", "exe", "dll", "so", "pyc"];

  // CSV/TSV: render as table
  if (tableExts.includes(ext)) {
    activeFilePath = path;
    const name = path.split("/").pop() || path;
    let file = openFiles.find((f) => f.path === path);
    if (!file) {
      try {
        const size = await invoke<number>("ws_file_size", { path }).catch(() => 0);
        if (size > MAX_PREVIEW_BYTES) {
          file = { path, name, language: "python" as SupportedLanguage, content: "", modified: false, tooLarge: true, sizeBytes: size };
        } else {
          const content = await invoke<string>("ws_read_file", { path });
          file = { path, name, language: "python" as SupportedLanguage, content, modified: false };
        }
        openFiles.push(file);
      } catch (e) {
        // Most often a non-UTF-8 file (cp949 Korean .csv is common). Clicking
        // it used to do NOTHING AT ALL — the student had no idea whether the
        // click registered. Show why instead.
        mountUnreadableViewer(name, e);
        activeFilePath = null;
        return;
      }
    }
    if (file.tooLarge) {
      mountTooLargeViewer(file);
    } else {
      mountTableViewer(file);
    }
    renderTabs();
    refreshFileTree();
    return;
  }

  // Excel: convert to CSV via Python, then show as table
  if (excelExts.includes(ext)) {
    activeFilePath = path;
    const name = path.split("/").pop() || path;
    let file = openFiles.find((f) => f.path === path);
    if (!file) {
      // Convert xlsx to CSV text using pandas — run it with the EXAM VENV
      // python (that's where pandas/openpyxl are installed; the base python
      // the backend would otherwise fall back to has neither).
      try {
        const size = await invoke<number>("ws_file_size", { path }).catch(() => 0);
        if (size > MAX_PREVIEW_BYTES) {
          file = { path, name, language: "python" as SupportedLanguage, content: "", modified: false, tooLarge: true, sizeBytes: size };
          openFiles.push(file);
        } else {
          const csvContent = await invoke<string>("ws_xlsx_to_csv", { path, pythonPath: selectedPythonPath });
          file = { path, name, language: "python" as SupportedLanguage, content: csvContent, modified: false };
          openFiles.push(file);
        }
      } catch {
        file = { path, name, language: "python" as SupportedLanguage, content: "", modified: false };
        openFiles.push(file);
      }
    }
    if (file.tooLarge) {
      mountTooLargeViewer(file);
    } else if (file.content) {
      mountTableViewer(file);
    } else {
      const container = document.getElementById("editor-container")!;
      container.innerHTML = `<div class="binary-viewer"><div class="binary-icon">&#128196;</div><div class="binary-name">${escapeHtml(name)}</div><div class="binary-info">Could not read Excel file</div></div>`;
    }
    renderTabs();
    refreshFileTree();
    return;
  }

  // Binary files: show info only
  if (binaryExts.includes(ext)) {
    activeFilePath = path;
    const name = path.split("/").pop() || path;
    let file = openFiles.find((f) => f.path === path);
    if (!file) {
      file = { path, name, language: "python" as SupportedLanguage, content: "", modified: false };
      openFiles.push(file);
    }
    const container = document.getElementById("editor-container")!;
    container.innerHTML = `<div class="binary-viewer">
      <div class="binary-icon">&#128196;</div>
      <div class="binary-name">${escapeHtml(name)}</div>
      <div class="binary-info">${ext.toUpperCase()} file — binary format, cannot preview in editor</div>
    </div>`;
    editorView = null;
    clearNotebook();
    renderTabs();
    refreshFileTree();
    return;
  }

  // Images: don't read as text, go straight to viewer
  if (imageExts.includes(ext)) {
    activeFilePath = path;
    const name = path.split("/").pop() || path;
    let file = openFiles.find((f) => f.path === path);
    if (!file) {
      const size = await invoke<number>("ws_file_size", { path }).catch(() => 0);
      file = { path, name, language: "python" as SupportedLanguage, content: "", modified: false,
               tooLarge: size > MAX_PREVIEW_BYTES, sizeBytes: size };
      openFiles.push(file);
    }
    if (file.tooLarge) {
      mountTooLargeViewer(file);
      renderTabs();
      refreshFileTree();
      return;
    }
    mountImageViewer(file);
    renderTabs();
    refreshFileTree();
    return;
  }

  let file = openFiles.find((f) => f.path === path);
  if (!file) {
    try {
      const name = path.split("/").pop() || path;
      const lang = langFromExtension(name) || "python";
      const size = await invoke<number>("ws_file_size", { path }).catch(() => 0);
      if (size > MAX_PREVIEW_BYTES) {
        // Never mount a giant doc into CodeMirror — it froze the whole UI.
        file = { path, name, language: lang, content: "", modified: false, tooLarge: true, sizeBytes: size };
      } else {
        const content = await invoke<string>("ws_read_file", { path });
        file = { path, name, language: lang, content, modified: false };
      }
      openFiles.push(file);
    } catch (e) {
      // Non-UTF-8 / locked / unreadable. Silently returning left the student
      // clicking a file with zero feedback.
      console.error("Failed to open file:", e);
      mountUnreadableViewer(path.split("/").pop() || path, e);
      activeFilePath = null;
      renderTabs();
      return;
    }
  }

  if (activeFilePath !== path) {
    invoke("log_editor_event", {
      eventType: "tab_switch",
      detail: `Switched to ${path} (lang: ${file.language})`,
      charCount: null,
      timeDeltaMs: null,
    });
  }
  activeFilePath = path;
  setCurrentFile(path);
  const selector = document.getElementById("lang-selector") as HTMLSelectElement;
  if (selector) selector.value = file.language;

  if (file.tooLarge) {
    mountTooLargeViewer(file);
  } else if (path.endsWith(".ipynb")) {
    mountNotebookView(file);
  } else {
    clearNotebook();
    mountEditor(file);
  }
  renderTabs();
  refreshFileTree();
}

function mountUnreadableViewer(name: string, err: unknown): void {
  const container = document.getElementById("editor-container")!;
  editorView = null;
  clearNotebook();
  container.innerHTML = `<div class="binary-viewer">
    <div class="binary-icon">&#128196;</div>
    <div class="binary-name">${escapeHtml(name)}</div>
    <div class="binary-info">이 파일은 편집기에서 열 수 없습니다 (UTF-8이 아니거나 읽기 실패).<br/>코드에서 인코딩을 지정해 읽어보세요 — 예: open(path, encoding='cp949')<br/><br/>${escapeHtml(String(err))}</div>
  </div>`;
}

function mountTooLargeViewer(file: OpenFile): void {
  const container = document.getElementById("editor-container")!;
  editorView = null;
  clearNotebook();
  const mb = ((file.sizeBytes ?? 0) / (1024 * 1024)).toFixed(1);
  container.innerHTML = `<div class="binary-viewer">
    <div class="binary-icon">&#128196;</div>
    <div class="binary-name">${escapeHtml(file.name)}</div>
    <div class="binary-info">${mb} MB — 파일이 너무 커서 미리보기를 열 수 없습니다.<br/>코드에서 직접 읽어 사용하세요 (예: open() / pd.read_csv).</div>
  </div>`;
}

async function saveCurrentFile(): Promise<void> {
  await syncCurrentEditor();
  const file = openFiles.find((f) => f.path === activeFilePath);
  if (!file) return;

  try {
    await invoke("ws_write_file", { path: file.path, content: file.content });
    file.modified = false;
    renderTabs();
    refreshFileTree();
  } catch (e) {
    appendOutput(`Failed to save: ${e}\n`, "error");
  }
}

async function syncCurrentEditor(): Promise<void> {
  if (!activeFilePath) return;
  const file = openFiles.find((f) => f.path === activeFilePath);
  if (!file) return;

  if (isNotebookActive() && activeFilePath.endsWith(".ipynb")) {
    file.content = getNotebookJSON();
  } else if (editorView) {
    file.content = editorView.state.doc.toString();
  }
}

// ===== Tabs =====
function renderTabs(): void {
  const tabBar = document.getElementById("tab-bar")!;
  tabBar.innerHTML = "";

  for (const file of openFiles) {
    const el = document.createElement("div");
    el.className = `tab${file.path === activeFilePath ? " active" : ""}`;
    el.innerHTML = `
      ${file.modified ? '<span class="tab-modified"></span>' : ""}
      <span class="tab-name">${escapeHtml(file.name)}</span>
      <span class="tab-close">&times;</span>
    `;
    el.querySelector(".tab-name")!.addEventListener("click", () => openFileByPath(file.path));
    el.querySelector(".tab-close")!.addEventListener("click", (e) => {
      e.stopPropagation();
      void closeFile(file.path);
    });
    tabBar.appendChild(el);
  }
}

async function closeFile(path: string): Promise<void> {
  const idx = openFiles.findIndex((f) => f.path === path);
  if (idx < 0) return;

  const file = openFiles[idx];
  // Capture the live buffer before removal (covers active notebook where editorView is null)
  if (path === activeFilePath) {
    if (isNotebookActive() && path.endsWith(".ipynb")) {
      file.content = getNotebookJSON();
    } else if (editorView) {
      file.content = editorView.state.doc.toString();
    }
  }
  // Auto-save unsaved changes so closing never loses work
  if (file.modified) {
    await invoke("ws_write_file", { path: file.path, content: file.content });
    file.modified = false;
  }

  openFiles.splice(idx, 1);

  if (openFiles.length === 0) {
    activeFilePath = null;
    document.getElementById("editor-container")!.innerHTML = '<div class="editor-placeholder">No file open</div>';
    renderTabs();
    return;
  }

  if (activeFilePath === path) {
    const newIdx = Math.min(idx, openFiles.length - 1);
    openFileByPath(openFiles[newIdx].path);
  } else {
    renderTabs();
  }
}

// ===== Editor =====
function mountEditor(file: OpenFile): void {
  const container = document.getElementById("editor-container")!;
  container.innerHTML = "";
  setCurrentFile(file.path);

  editorView = createEditor(
    container, file.language, file.content,
    (event) => {
      // Mark paste source BEFORE the transaction fires
      if (event.inputType === "insertFromPaste") {
        markNextInputSource("paste");
      }
      handleEditorInput(event);
      // NOTE: modified is set in the transaction callback below, not here —
      // onInput only fires for beforeinput events carrying data, so it MISSES
      // backspace/delete, Enter, drops, and undo/redo. Setting it on every
      // docChanged transaction ensures those edits are saved on close/submit.
      renderTabs();
    },
    (changes, userEvent) => {
      // Every document change (incl. delete / Enter / undo / redo) marks the
      // file dirty so closeFile and submitExam actually write it to disk.
      file.modified = true;
      renderTabs();
      recordTransaction(changes, userEvent);
    },
  );
}

function mountTableViewer(file: OpenFile): void {
  const container = document.getElementById("editor-container")!;
  container.innerHTML = "";
  editorView = null;
  clearNotebook();

  const wrapper = document.createElement("div");
  wrapper.className = "table-viewer";

  const sep = file.path.endsWith(".tsv") ? "\t" : ",";
  const lines = file.content.split("\n").filter(l => l.trim());
  if (lines.length === 0) {
    wrapper.innerHTML = '<div class="binary-info">Empty file</div>';
    container.appendChild(wrapper);
    return;
  }

  // Parse CSV (handle quoted fields)
  const parseRow = (line: string): string[] => {
    const result: string[] = [];
    let current = "";
    let inQuotes = false;
    for (const ch of line) {
      if (ch === '"') { inQuotes = !inQuotes; }
      else if (ch === sep && !inQuotes) { result.push(current.trim()); current = ""; }
      else { current += ch; }
    }
    result.push(current.trim());
    return result;
  };

  const headers = parseRow(lines[0]);
  // Parse ONLY the rows we render. Mapping parseRow over every line meant a
  // 15MB CSV (well under the open-size cap) parsed ~500k rows character by
  // character on the UI thread just to display 500 of them — a multi-second
  // freeze on a low-spec laptop.
  const totalRows = lines.length - 1;
  const rows = lines.slice(1, 1 + MAX_TABLE_ROWS).map(parseRow);

  // Info bar
  const info = document.createElement("div");
  info.className = "table-info";
  info.textContent = `${file.name} — ${totalRows} rows, ${headers.length} columns`;
  wrapper.appendChild(info);

  // Table
  const tableWrap = document.createElement("div");
  tableWrap.className = "table-scroll";

  const table = document.createElement("table");
  table.className = "data-table";

  // Header
  const thead = document.createElement("thead");
  const headerRow = document.createElement("tr");
  // Row number column
  const thNum = document.createElement("th");
  thNum.className = "row-num";
  thNum.textContent = "#";
  headerRow.appendChild(thNum);
  for (const h of headers) {
    const th = document.createElement("th");
    th.textContent = h;
    headerRow.appendChild(th);
  }
  thead.appendChild(headerRow);
  table.appendChild(thead);

  // Body (max MAX_TABLE_ROWS rows)
  const tbody = document.createElement("tbody");
  const maxRows = rows.length;
  for (let i = 0; i < maxRows; i++) {
    const tr = document.createElement("tr");
    const tdNum = document.createElement("td");
    tdNum.className = "row-num";
    tdNum.textContent = String(i + 1);
    tr.appendChild(tdNum);
    for (let j = 0; j < headers.length; j++) {
      const td = document.createElement("td");
      td.textContent = rows[i]?.[j] ?? "";
      // Right-align numbers
      if (rows[i]?.[j] && !isNaN(Number(rows[i][j]))) {
        td.className = "num-cell";
      }
      tr.appendChild(td);
    }
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);
  tableWrap.appendChild(table);
  wrapper.appendChild(tableWrap);

  if (totalRows > rows.length) {
    const more = document.createElement("div");
    more.className = "table-info";
    more.textContent = `Showing ${rows.length} of ${totalRows} rows`;
    wrapper.appendChild(more);
  }

  container.appendChild(wrapper);
}

function mountImageViewer(file: OpenFile): void {
  const container = document.getElementById("editor-container")!;
  container.innerHTML = "";
  editorView = null;
  clearNotebook();

  const wrapper = document.createElement("div");
  wrapper.className = "image-viewer";

  // Load image as base64 from workspace
  invoke<string>("ws_read_file_base64", { path: file.path }).then((base64) => {
    const ext = file.path.split(".").pop()?.toLowerCase() || "png";
    const mime = ext === "svg" ? "image/svg+xml" : `image/${ext === "jpg" ? "jpeg" : ext}`;
    wrapper.innerHTML = `
      <div class="image-viewer-label">${escapeHtml(file.name)}</div>
      <img src="data:${mime};base64,${base64}" class="image-preview" />
    `;
  }).catch(() => {
    wrapper.innerHTML = `<div class="image-viewer-label">Cannot display image</div>`;
  });

  container.appendChild(wrapper);
}

function mountNotebookView(file: OpenFile): void {
  const container = document.getElementById("editor-container")!;
  container.innerHTML = "";
  editorView = null;

  mountNotebook(container, file.content, file.path, () => {
    file.modified = true;
    renderTabs();
  });
}

// ===== Context Menu =====
function showContextMenu(x: number, y: number, path: string, isDir: boolean): void {
  closeContextMenu();
  const menu = document.createElement("div");
  menu.className = "context-menu";
  menu.id = "context-menu";
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;

  type MenuItem = { type: "action"; label: string; action: () => void; danger?: boolean }
    | { type: "separator" };

  const items: MenuItem[] = isDir
    ? [
        { type: "action", label: "New File", action: () => promptNewFile(path) },
        { type: "action", label: "New Notebook", action: () => promptNewNotebook(path) },
        { type: "action", label: "New Folder", action: () => promptNewFolder(path) },
        { type: "action", label: "Add File...", action: () => importExternalFile(path) },
        { type: "separator" },
        { type: "action", label: "Rename", action: () => startRenameInSidebar(path) },
        { type: "action", label: "Delete", action: () => deleteItem(path), danger: true },
      ]
    : [
        { type: "action", label: "Rename", action: () => startRenameInSidebar(path) },
        { type: "action", label: "Run", action: () => { openFileByPath(path).then(runCurrentFile); } },
        { type: "separator" },
        { type: "action", label: "Delete", action: () => deleteItem(path), danger: true },
      ];

  for (const item of items) {
    if (item.type === "separator") {
      const sep = document.createElement("div");
      sep.className = "context-menu-separator";
      menu.appendChild(sep);
    } else {
      const el = document.createElement("div");
      el.className = `context-menu-item${item.danger ? " danger" : ""}`;
      el.textContent = item.label;
      el.addEventListener("click", (e) => { e.stopPropagation(); closeContextMenu(); item.action(); });
      menu.appendChild(el);
    }
  }

  document.body.appendChild(menu);
  const rect = menu.getBoundingClientRect();
  if (rect.right > window.innerWidth) menu.style.left = `${window.innerWidth - rect.width - 4}px`;
  if (rect.bottom > window.innerHeight) menu.style.top = `${window.innerHeight - rect.height - 4}px`;
}

function closeContextMenu(): void {
  document.getElementById("context-menu")?.remove();
}

// ===== New File / Folder / Rename / Delete =====
async function promptNewFile(parentDir: string): Promise<void> {
  const lang = (document.getElementById("lang-selector") as HTMLSelectElement).value as SupportedLanguage;
  const ext = extForLanguage(lang);
  const name = await findUniqueName(parentDir, "untitled", ext);
  const path = parentDir ? `${parentDir}/${name}` : name;
  try {
    await invoke("ws_write_file", { path, content: "" });
    if (parentDir) expandedDirs.add(parentDir);
    await refreshFileTree();
    await openFileByPath(path);
    // Auto-enter rename mode
    setTimeout(() => startRenameInSidebar(path), 50);
  } catch (e) {
    alert(`Failed to create file: ${e}`);
  }
}

async function promptNewFolder(parentDir: string): Promise<void> {
  const name = await findUniqueName(parentDir, "folder", "");
  const path = parentDir ? `${parentDir}/${name}` : name;
  try {
    await invoke("ws_create_dir", { path });
    expandedDirs.add(path);
    if (parentDir) expandedDirs.add(parentDir);
    await refreshFileTree();
    setTimeout(() => startRenameInSidebar(path), 50);
  } catch (e) {
    alert(`Failed to create folder: ${e}`);
  }
}

async function promptNewNotebook(parentDir: string): Promise<void> {
  const name = await findUniqueName(parentDir, "notebook", ".ipynb");
  const path = parentDir ? `${parentDir}/${name}` : name;
  const emptyNotebook = JSON.stringify({
    cells: [{ cell_type: "code", source: [""], metadata: {}, outputs: [], execution_count: null }],
    metadata: { kernelspec: { display_name: "Python 3", language: "python", name: "python3" }, language_info: { name: "python" } },
    nbformat: 4, nbformat_minor: 5,
  }, null, 1);
  try {
    await invoke("ws_write_file", { path, content: emptyNotebook });
    if (parentDir) expandedDirs.add(parentDir);
    await refreshFileTree();
    await openFileByPath(path);
    setTimeout(() => startRenameInSidebar(path), 50);
  } catch (e) {
    alert(`Failed to create notebook: ${e}`);
  }
}

async function findUniqueName(parentDir: string, base: string, ext: string): Promise<string> {
  // Get existing names in the directory
  let existingNames: string[] = [];
  try {
    const tree = await invoke<FileNode[]>("ws_list_tree");
    const findChildren = (nodes: FileNode[], dir: string): string[] => {
      if (dir === "") return nodes.map((n) => n.name);
      for (const n of nodes) {
        if (n.is_dir && n.path === dir) return n.children.map((c) => c.name);
        if (n.is_dir) {
          const found = findChildren(n.children, dir);
          if (found.length > 0) return found;
        }
      }
      return [];
    };
    existingNames = findChildren(tree, parentDir);
  } catch { /* ignore */ }

  // Find unique name: untitled.py, untitled2.py, untitled3.py...
  const firstName = ext ? `${base}${ext}` : base;
  if (!existingNames.includes(firstName)) return firstName;

  for (let i = 2; ; i++) {
    const candidate = ext ? `${base}${i}${ext}` : `${base}${i}`;
    if (!existingNames.includes(candidate)) return candidate;
  }
}

async function importExternalFile(destDir: string): Promise<void> {
  try {
    const selected = await open({
      multiple: true,
      title: "Select files to import",
    });
    if (!selected) return;

    const raw = Array.isArray(selected) ? selected : [selected];
    const paths: string[] = raw.map((p: any) => typeof p === "string" ? p : p.path ?? String(p));
    for (const pathStr of paths) {
      const result = await invoke<{ dest_path: string; original_path: string; size_bytes: number }>(
        "ws_import_file",
        { sourcePath: pathStr, destDir }
      );
      if (destDir) expandedDirs.add(destDir);
      // Auto-open imported text files
      const ext = result.dest_path.split(".").pop()?.toLowerCase() || "";
      const textExts = ["py", "js", "ts", "java", "c", "cpp", "h", "hpp", "txt", "json", "md", "csv", "xml", "html", "css"];
      if (textExts.includes(ext)) {
        await openFileByPath(result.dest_path);
      }
    }
    await refreshFileTree();
    renderTabs();
  } catch (e) {
    alert(`Import failed: ${e}`);
  }
}

function extForLanguage(lang: SupportedLanguage): string {
  const map: Record<SupportedLanguage, string> = {
    python: ".py", javascript: ".js", typescript: ".ts",
    java: ".java", c: ".c", cpp: ".cpp",
  };
  return map[lang] || ".txt";
}

function startRenameInSidebar(path: string): void {
  const el = document.querySelector(`.file-item[data-path="${CSS.escape(path)}"]`);
  if (!el) return;

  const nameSpan = el.querySelector(".file-name") as HTMLElement;
  const currentName = path.split("/").pop() || path;

  const input = document.createElement("input");
  input.type = "text";
  input.className = "file-rename-input";
  input.value = currentName;
  nameSpan.replaceWith(input);
  input.focus();

  const dotIdx = currentName.lastIndexOf(".");
  input.setSelectionRange(0, dotIdx > 0 ? dotIdx : currentName.length);

  const commit = async () => {
    const newName = input.value.trim();
    if (newName && newName !== currentName) {
      const parentDir = path.includes("/") ? path.substring(0, path.lastIndexOf("/")) : "";
      const newPath = parentDir ? `${parentDir}/${newName}` : newName;
      try {
        await invoke("ws_rename", { oldPath: path, newPath });
        // Update open files that reference old path
        for (const f of openFiles) {
          if (f.path === path || f.path.startsWith(path + "/")) {
            f.path = f.path.replace(path, newPath);
            f.name = f.path.split("/").pop() || f.path;
            f.language = langFromExtension(f.name) || f.language;
          }
        }
        // Re-point activeFilePath for BOTH a direct rename of the active file
        // AND a rename of a PARENT directory containing it. Without the
        // directory case, activeFilePath stays stale and Ctrl+S / Run / sync
        // (which look up openFiles by activeFilePath) silently no-op and the
        // next tab switch discards unsaved edits.
        if (activeFilePath === path) {
          activeFilePath = newPath;
          setCurrentFile(newPath);
        } else if (activeFilePath && activeFilePath.startsWith(path + "/")) {
          activeFilePath = activeFilePath.replace(path, newPath);
          setCurrentFile(activeFilePath);
        }
      } catch (e) {
        alert(`Rename failed: ${e}`);
      }
    }
    await refreshFileTree();
    renderTabs();
  };

  input.addEventListener("blur", commit);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); input.blur(); }
    if (e.key === "Escape") { input.value = currentName; input.blur(); }
  });
}

async function deleteItem(path: string): Promise<void> {
  const name = path.split("/").pop() || path;
  if (!confirm(`Delete "${name}"?`)) return;
  try {
    await invoke("ws_delete", { path });
    // Close if open
    openFiles = openFiles.filter((f) => f.path !== path && !f.path.startsWith(path + "/"));
    if (activeFilePath === path || activeFilePath?.startsWith(path + "/")) {
      if (openFiles.length > 0) {
        openFileByPath(openFiles[0].path);
      } else {
        activeFilePath = null;
        document.getElementById("editor-container")!.innerHTML = '<div class="editor-placeholder">No file open</div>';
        renderTabs();
      }
    }
    await refreshFileTree();
  } catch (e) {
    alert(`Delete failed: ${e}`);
  }
}

// ===== Run Code =====
async function runCurrentFile(): Promise<void> {
  if (isRunning || !activeFilePath) return;

  // CRITICAL: claim the run BEFORE any await. Without this a fast
  // double-click of Run sneaks past the guard while syncCurrentEditor /
  // saveCurrentFile are awaiting, causing the same file to run twice —
  // the student sees their print() output duplicated.
  isRunning = true;
  streamingOutputReceived = false;

  // If notebook is active, delegate to notebook's Run All. Release the
  // main isRunning lock since notebook tracks its own running state.
  if (isNotebookActive() && activeFilePath.endsWith(".ipynb")) {
    isRunning = false;
    const runAllBtn = document.getElementById("nb-run-all");
    if (runAllBtn) runAllBtn.click();
    return;
  }

  const file = openFiles.find((f) => f.path === activeFilePath);
  if (!file) { isRunning = false; return; }

  await syncCurrentEditor();
  await saveCurrentFile();

  if (!file.content.trim()) {
    appendOutput("No code to run.\n", "system");
    isRunning = false;
    return;
  }

  const btn = document.getElementById("btn-run") as HTMLButtonElement;
  btn.innerHTML = "&#9632; Stop";
  btn.classList.remove("btn-run");
  btn.classList.add("btn-danger");
  // Change click to stop
  btn.onclick = stopCurrentRun;

  const panel = document.getElementById("output-panel")!;
  panel.classList.remove("collapsed");
  panel.classList.add("expanded");

  document.getElementById("output-content")!.textContent = "";
  // Clear previous error highlights
  pendingErrorLines.length = 0;
  if (editorView) clearErrors(editorView);
  appendOutput(`$ Running ${file.path} (${file.language})\n`, "system");

  try {
    await invoke("run_code", {
      language: file.language,
      code: file.content,
      filename: file.path,
      pythonPath: selectedPythonPath,
    });
    // Output comes via "run-output" events, completion via "run-done"
  } catch (e) {
    appendOutput(`Error: ${e}\n`, "error");
    resetRunButton();
  }
}

function stopCurrentRun(): void {
  if (!isRunning) return;
  // Reset UI immediately — backend taskkill /F /T can take 100ms+, and we
  // don't want the user to wonder if the click registered. Fire-and-forget
  // the actual kill; "run-done" event will arrive shortly after.
  appendOutput("\n[stopping...]\n", "system");
  resetRunButton();
  invoke<boolean>("stop_code").catch(() => { /* ignore */ });
}

function resetRunButton(): void {
  isRunning = false;
  const btn = document.getElementById("btn-run") as HTMLButtonElement;
  btn.innerHTML = "&#9654; Run";
  btn.classList.remove("btn-danger");
  btn.classList.add("btn-run");
  btn.onclick = () => runCurrentFile();
  // Refresh file tree to show newly created files (png, csv, etc.)
  refreshFileTree();
}

// Error line highlighting — parse Python traceback "line N"
// Parse error lines from stderr (Python, GCC, Java tracebacks)
// Patterns: 'File "x.py", line 5' / 'main.c:5:' / 'Main.java:5:'
const pendingErrorLines: number[] = [];

function highlightErrorLine(text: string): void {
  if (!editorView) return;

  let lineNum: number | null = null;

  // Python: File "xxx", line N
  const pyMatch = text.match(/line (\d+)/);
  if (pyMatch) lineNum = parseInt(pyMatch[1]);

  // C/C++/Java: filename:N: or filename:N:N:
  if (!lineNum) {
    const cMatch = text.match(/:\s*(\d+)\s*:/);
    if (cMatch) lineNum = parseInt(cMatch[1]);
  }

  if (lineNum && lineNum >= 1 && lineNum <= editorView.state.doc.lines) {
    if (!pendingErrorLines.includes(lineNum)) {
      pendingErrorLines.push(lineNum);
      markErrorLines(editorView, [...pendingErrorLines]);
    }
  }
}

// ===== Screen Recording (auto-start) =====
async function setupExamPython(): Promise<void> {
  try {
    const examPyPath = await invoke<string>("setup_exam_python");
    selectedPythonPath = examPyPath;
    const pyEl = document.getElementById("status-python");
    if (pyEl) pyEl.textContent = "Python: Exam Env";
    appendOutput(`Exam Python ready: ${examPyPath}\n`, "system");
  } catch (e) {
    appendOutput(`Exam Python setup failed: ${e}\nUsing system Python instead.\n`, "system");
    const pyEl = document.getElementById("status-python");
    if (pyEl) pyEl.textContent = "Python: System (no exam env)";
  }
}

let recRetryTimer: number | null = null;
let recRetryCount = 0;
let recFailAlerted = false;
const REC_RETRY_INTERVAL_MS = 15000;
const REC_RETRY_MAX = 40; // ~10 minutes of retries

async function startAutoRecording(): Promise<void> {
  const indicator = document.getElementById("rec-indicator")!;
  indicator.classList.remove("rec-disabled", "rec-error");
  indicator.textContent = "● REC";
  try {
    const path = await invoke<string>("start_recording", { outputDir: null });
    isRecording = true;
    indicator.classList.add("recording");
    appendOutput(`Screen recording started: ${path}\n`, "system");
    if (recRetryTimer !== null) { clearInterval(recRetryTimer); recRetryTimer = null; }
    recRetryCount = 0;
  } catch (e) {
    indicator.classList.add("rec-error");
    indicator.title = `Recording failed: ${e}`;
    appendOutput(`Recording failed: ${e}\n`, "error");
    console.warn("Auto-recording failed:", e);
    // First failure: loud modal (typically the macOS Screen Recording
    // permission on first launch). Then keep retrying quietly — once the
    // student grants the permission, a later attempt succeeds without an
    // app restart on most macOS versions.
    if (!recFailAlerted) {
      recFailAlerted = true;
      alert(`[녹화 시작 실패]\n\n${e}\n\n권한을 허용하면 15초 내에 자동으로 다시 시작됩니다. 계속 실패하면 IDE를 재시작하세요.`);
    }
    if (recRetryTimer === null && setupConfig.recording_enabled) {
      recRetryTimer = window.setInterval(() => {
        if (isRecording || !setupConfig.recording_enabled) {
          if (recRetryTimer !== null) { clearInterval(recRetryTimer); recRetryTimer = null; }
          return;
        }
        if (recRetryCount >= REC_RETRY_MAX) {
          if (recRetryTimer !== null) { clearInterval(recRetryTimer); recRetryTimer = null; }
          // Giving up SILENTLY meant the student sat through the rest of the
          // exam with no recording and no further notice.
          alert(
            "[녹화 실패] 약 10분간 재시도했지만 화면 녹화를 시작하지 못했습니다.\n\n" +
            "이 시험은 녹화 없이 진행되고 있습니다 — 감독관에게 즉시 알리세요."
          );
          return;
        }
        recRetryCount++;
        startAutoRecording();
      }, REC_RETRY_INTERVAL_MS);
    }
  }
}

/// A recording_health_fail can mean the capture process DIED (recoverable —
/// start a fresh segment) or that a live process stopped producing frames
/// (restarting would just fail with "already in progress"). Ask the backend
/// which it is instead of guessing. Without this, a mid-exam ffmpeg crash left
/// the rest of the exam unrecorded even though restarting would have worked.
async function maybeRestartRecording(): Promise<void> {
  if (!setupConfig.recording_enabled) return;
  try {
    const live = await invoke<boolean>("is_recording");
    if (live) return; // stalled but alive — a restart attempt would be rejected
  } catch {
    return;
  }
  isRecording = false;
  recFailAlerted = true;   // don't stack a second modal on top of the health alert
  recRetryCount = 0;       // fresh retry budget for this new failure
  await startAutoRecording();
}

// ===== Submit Exam =====
let isSubmitting = false;
async function submitExam(): Promise<void> {
  if (isSubmitting) return;  // double-click guard
  if (!confirm("제출하시겠습니까?\n제출 후 프로그램이 종료됩니다.")) {
    return;
  }
  isSubmitting = true;
  const btn = document.getElementById("btn-submit") as HTMLButtonElement;
  btn.disabled = true;
  btn.textContent = "제출 중...";

  try {
    // Stop anything still running BEFORE zipping: a live python child keeps
    // rewriting its output files while the backend zips them (torn members),
    // and it would simply be ORPHANED by the exit below and keep running
    // after the exam.
    if (isRunning) stopCurrentRun();
    if (isNotebookRunning()) { try { await stopNotebook(); } catch { /* best effort */ } }

    // Save all open files. This is INSIDE the try so a write failure (a
    // running script holding the file open, AV/indexer lock, disk error)
    // re-enables the button and surfaces the error instead of leaving Submit
    // permanently stuck at "제출 중..." with no message.
    await syncCurrentEditor(); // capture live buffer (covers active notebook where editorView is null)
    for (const file of openFiles) {
      if (file.path === activeFilePath && editorView) {
        file.content = editorView.state.doc.toString();
      }
      // Always write the ACTIVE file (its live buffer was just captured) even
      // if `modified` was somehow not set; write others only when dirty.
      if (file.modified || (file.path === activeFilePath && editorView)) {
        await invoke("ws_write_file", { path: file.path, content: file.content });
        file.modified = false;
      }
    }
    flushTypingSummary();
    // Save code edit history before submit
    await invoke("save_code_history", { historyJson: getEditHistoryJSON() });

    const result = await invoke<{ folder_path: string; code_zip: string; video_zip: string }>(
      "submit_exam",
      { studentId }
    );

    alert(`제출 완료!\n\n저장 위치:\n${result.folder_path}\n\n프로그램을 종료합니다.`);

    // Exit the application (authorize with the backend exit gate first)
    try { await invoke("allow_exit"); } catch { /* proceed anyway */ }
    const { exit } = await import("@tauri-apps/plugin-process");
    await exit(0);
  } catch (e) {
    appendOutput(`Submit failed: ${e}\n`, "error");
    alert(`제출에 실패했습니다:\n\n${e}\n\n원본 파일은 보존되어 있습니다. 문제를 해결한 뒤 다시 제출하세요.`);
    btn.disabled = false;
    btn.textContent = "Submit";
    isSubmitting = false;
    // submit_exam already STOPPED the recording before it failed. The student
    // is now continuing the exam — restart capture instead of silently
    // recording nothing for the remainder.
    if (setupConfig.recording_enabled) {
      isRecording = false;
      recFailAlerted = true; // failure alert above is enough; no extra modal
      void startAutoRecording();
    }
  }
}

// ===== Drag and Drop (custom mouse-based, no HTML5 DnD API) =====
let dragState: {
  srcPath: string;
  srcEl: HTMLElement;
  ghost: HTMLElement;
  startX: number;
  startY: number;
  isDragging: boolean;
} | null = null;

// All drop targets registered during render
const dropTargets: { el: HTMLElement; dirPath: string }[] = [];

function makeDraggable(el: HTMLElement, path: string): void {
  el.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return; // left click only
    e.stopPropagation();

    const startX = e.clientX;
    const startY = e.clientY;
    let moved = false;

    const onMove = (me: MouseEvent) => {
      const dx = me.clientX - startX;
      const dy = me.clientY - startY;

      // Start drag after 5px threshold
      if (!moved && Math.abs(dx) + Math.abs(dy) < 5) return;

      if (!moved) {
        moved = true;
        // Create ghost
        const ghost = document.createElement("div");
        ghost.className = "drag-ghost";
        ghost.textContent = path.split("/").pop() || path;
        document.body.appendChild(ghost);
        el.classList.add("dragging");

        dragState = { srcPath: path, srcEl: el, ghost, startX, startY, isDragging: true };
      }

      if (dragState) {
        dragState.ghost.style.left = `${me.clientX + 12}px`;
        dragState.ghost.style.top = `${me.clientY - 8}px`;

        // Highlight drop target under cursor
        updateDropHighlight(me.clientX, me.clientY);
      }
    };

    const onUp = async (me: MouseEvent) => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);

      if (!moved || !dragState) {
        dragState = null;
        return;
      }

      // Find drop target
      const target = findDropTarget(me.clientX, me.clientY);

      // Cleanup
      dragState.ghost.remove();
      dragState.srcEl.classList.remove("dragging");
      clearDropHighlights();

      const srcPath = dragState.srcPath;
      dragState = null;

      if (target === null || target === undefined) return;
      const destDir = target;

      // Validate
      if (srcPath === destDir) return;
      if (destDir.startsWith(srcPath + "/")) return;
      const srcParent = srcPath.includes("/") ? srcPath.substring(0, srcPath.lastIndexOf("/")) : "";
      if (srcParent === destDir) return;

      try {
        const newPath = await invoke<string>("ws_move", { srcPath, destDir });
        for (const f of openFiles) {
          if (f.path === srcPath) {
            f.path = newPath;
            f.name = newPath.split("/").pop() || newPath;
          } else if (f.path.startsWith(srcPath + "/")) {
            f.path = f.path.replace(srcPath, newPath);
            f.name = f.path.split("/").pop() || f.path;
          }
        }
        if (activeFilePath === srcPath) activeFilePath = newPath;
        else if (activeFilePath?.startsWith(srcPath + "/")) {
          activeFilePath = activeFilePath.replace(srcPath, newPath);
        }
        if (destDir) expandedDirs.add(destDir);
        await refreshFileTree();
        renderTabs();
      } catch (err) {
        console.error("Move failed:", err);
      }
    };

    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
  });
}

function makeDropTarget(el: HTMLElement, destDir: string): void {
  dropTargets.push({ el, dirPath: destDir });
  el.dataset.dropDir = destDir;
}

function findDropTarget(x: number, y: number): string | null {
  // Find the most specific (deepest nested) drop target under cursor
  let best: { el: HTMLElement; dirPath: string } | null = null;

  for (const t of dropTargets) {
    const rect = t.el.getBoundingClientRect();
    if (x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom) {
      // Prefer more specific (smaller area) targets
      if (!best || rect.width * rect.height < best.el.getBoundingClientRect().width * best.el.getBoundingClientRect().height) {
        // Don't allow dropping on self
        if (dragState && t.dirPath !== dragState.srcPath) {
          best = t;
        }
      }
    }
  }

  return best ? best.dirPath : null;
}

function updateDropHighlight(x: number, y: number): void {
  clearDropHighlights();
  for (const t of dropTargets) {
    const rect = t.el.getBoundingClientRect();
    if (x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom) {
      if (dragState && t.dirPath !== dragState.srcPath) {
        t.el.classList.add("drag-over-highlight");
      }
    }
  }
}

function clearDropHighlights(): void {
  document.querySelectorAll(".drag-over-highlight").forEach((d) => d.classList.remove("drag-over-highlight"));
}

// ===== Output Panel =====
function setupOutputPanel(): void {
  document.getElementById("output-toggle")!.addEventListener("click", () => {
    const panel = document.getElementById("output-panel")!;
    panel.classList.toggle("collapsed");
    panel.classList.toggle("expanded");
  });
  document.getElementById("output-clear")!.addEventListener("click", () => {
    document.getElementById("output-content")!.textContent = "";
    // Reset rAF batch so a buffered chunk doesn't re-appear on the next frame
    pendingOutput = [];
    if (pendingFlushHandle !== null) {
      cancelAnimationFrame(pendingFlushHandle);
      pendingFlushHandle = null;
    }
  });

  // Resize handle — drag to resize output panel height
  const handle = document.getElementById("output-resize-handle")!;
  const panel = document.getElementById("output-panel")!;

  handle.addEventListener("mousedown", (e) => {
    e.preventDefault();
    const startY = e.clientY;
    const startH = panel.offsetHeight;
    handle.classList.add("dragging");

    const onMove = (me: MouseEvent) => {
      // Dragging up = increasing height (startY - me.clientY is positive when moving up)
      const delta = startY - me.clientY;
      const newH = Math.max(60, Math.min(window.innerHeight * 0.8, startH + delta));
      panel.style.height = `${newH}px`;
    };

    const onUp = () => {
      handle.classList.remove("dragging");
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
    };

    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
  });
}

const MAX_OUTPUT_NODES = 5000;
let pendingOutput: { text: string; type: "stdout" | "error" | "system" }[] = [];
let pendingFlushHandle: number | null = null;

function appendOutput(text: string, type: "stdout" | "error" | "system"): void {
  pendingOutput.push({ text, type });
  if (pendingFlushHandle !== null) return;
  pendingFlushHandle = requestAnimationFrame(flushPendingOutput);
}

function flushPendingOutput(): void {
  pendingFlushHandle = null;
  if (pendingOutput.length === 0) return;

  const content = document.getElementById("output-content");
  if (!content) {
    pendingOutput = [];
    return;
  }

  const frag = document.createDocumentFragment();
  for (const { text, type } of pendingOutput) {
    const span = document.createElement("span");
    if (type === "error") span.className = "output-error";
    if (type === "system") span.className = "output-system";
    span.textContent = text;
    frag.appendChild(span);
  }
  pendingOutput = [];
  content.appendChild(frag);

  while (content.childElementCount > MAX_OUTPUT_NODES) {
    content.removeChild(content.firstChild!);
  }

  content.scrollTop = content.scrollHeight;
}

// ===== Sidebar Resize =====
function setupSidebarResize(): void {
  const handle = document.getElementById("sidebar-resize-handle")!;
  const sidebar = document.getElementById("sidebar")!;

  handle.addEventListener("mousedown", (e) => {
    const startX = e.clientX;
    const startWidth = sidebar.offsetWidth;
    handle.classList.add("dragging");

    const onMove = (e: MouseEvent) => {
      sidebar.style.width = `${Math.max(140, Math.min(400, startWidth + e.clientX - startX))}px`;
    };
    const onUp = () => {
      handle.classList.remove("dragging");
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
    };
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
  });
}

// ===== Log Panel =====
function setupLogPanel(): void {
  document.getElementById("log-panel-header")!.addEventListener("click", () => {
    const panel = document.getElementById("log-panel")!;
    panel.classList.toggle("collapsed");
    panel.classList.toggle("expanded");
  });
}

const MAX_LOG_ENTRIES = 3000;

function appendLogEntry(event: ActivityEvent): void {
  const logContent = document.getElementById("log-content")!;
  const entry = document.createElement("div");
  entry.className = `log-entry severity-${event.severity}`;
  const time = event.timestamp.split(" ")[1]?.substring(0, 8) || "";
  entry.innerHTML = `
    <span class="log-time">${time}</span>
    <span class="log-type">${formatEventType(event.event_type)}</span>
    <span class="log-detail">${escapeHtml(event.detail)}</span>
  `;
  logContent.appendChild(entry);
  // Bound the DOM (display only — the full log lives in the backend and is
  // exported at submit). A long run-heavy session otherwise grows this list
  // without limit and the whole UI gets sluggish.
  while (logContent.childElementCount > MAX_LOG_ENTRIES) {
    logContent.removeChild(logContent.firstChild!);
  }
  logContent.scrollTop = logContent.scrollHeight;

  if (event.severity === "warning" || event.severity === "alert") {
    warningCount++;
    updateLogBadge();
    updateStatusBar(event);
  }
}

function updateLogBadge(): void {
  const header = document.getElementById("log-panel-header")!;
  let badge = header.querySelector(".log-badge") as HTMLElement;
  if (!badge) {
    badge = document.createElement("span");
    badge.className = "log-badge";
    header.querySelector("span")!.appendChild(badge);
  }
  badge.textContent = String(warningCount);
}

// ===== Status Bar =====
function buildStatusBar(): void {
  document.getElementById("status-bar")!.innerHTML = `
    <div class="status-item"><span class="status-dot" id="monitor-dot"></span><span>Monitoring Active</span></div>
    <div class="status-item" id="status-focus">Focus: OK</div>
    <div class="status-item" id="status-clipboard">Clipboard: Idle</div>
    <div class="status-item" style="margin-left:auto" id="status-warnings">Warnings: 0</div>
    <div class="status-item status-python" id="status-python" title="Click to change Python interpreter">Python: System</div>
  `;
  document.getElementById("status-python")!.addEventListener("click", showPythonSelector);
  loadPythonList();
}

interface PythonInfo { path: string; version: string; label: string; }
let pythonList: PythonInfo[] = [];

async function loadPythonList(): Promise<void> {
  try {
    pythonList = await invoke<PythonInfo[]>("detect_pythons");
    // Only relabel to a detected interpreter when the IDE is NOT already bound
    // to the exam venv. detect_pythons resolves slowly (spawns --version), and
    // it was overwriting the "Python: Exam Env" label with the first detected
    // SYSTEM interpreter — misreporting which Python actually runs the code
    // (runs still use the venv) and tempting students to "fix" it.
    if (pythonList.length > 0 && !selectedPythonPath) {
      document.getElementById("status-python")!.textContent = `Python: ${pythonList[0].label}`;
    }
  } catch { /* ignore */ }
}

function showPythonSelector(): void {
  // Remove existing popup
  document.getElementById("python-selector")?.remove();

  const anchor = document.getElementById("status-python")!;
  const rect = anchor.getBoundingClientRect();

  const popup = document.createElement("div");
  popup.id = "python-selector";
  popup.className = "python-selector-popup";
  popup.style.left = `${rect.left}px`;
  popup.style.bottom = `${window.innerHeight - rect.top + 4}px`;

  // System default option
  const sysItem = document.createElement("div");
  sysItem.className = `py-option${selectedPythonPath === null ? " active" : ""}`;
  sysItem.textContent = "System Default";
  sysItem.addEventListener("click", () => {
    const oldPath = selectedPythonPath || "exam-env";
    selectedPythonPath = null;
    anchor.textContent = "Python: System";
    popup.remove();
    invoke("log_python_change", { fromEnv: oldPath, toEnv: "system" });
  });
  popup.appendChild(sysItem);

  // Detected interpreters
  for (const py of pythonList) {
    const item = document.createElement("div");
    item.className = `py-option${selectedPythonPath === py.path ? " active" : ""}`;
    item.innerHTML = `<span>${escapeHtml(py.label)}</span><span class="py-path">${escapeHtml(py.path)}</span>`;
    item.addEventListener("click", () => {
      const oldPath = selectedPythonPath || "exam-env";
      selectedPythonPath = py.path;
      anchor.textContent = `Python: ${py.label}`;
      popup.remove();
      invoke("log_python_change", { fromEnv: oldPath, toEnv: py.path });
    });
    popup.appendChild(item);
  }

  // Browse option
  const browseItem = document.createElement("div");
  browseItem.className = "py-option py-browse";
  browseItem.textContent = "Browse for venv...";
  browseItem.addEventListener("click", async () => {
    popup.remove();
    const path = await open({ directory: true, title: "Select Python venv folder" });
    if (!path) return;
    const dir = typeof path === "string" ? path : String(path);
    const pyExe = navigator.platform.includes("Win")
      ? `${dir}/Scripts/python.exe`
      : `${dir}/bin/python`;
    const oldPath = selectedPythonPath || "exam-env";
    selectedPythonPath = pyExe;
    const name = dir.split(/[/\\]/).pop() || dir;
    anchor.textContent = `Python: venv (${name})`;
    // Log the env switch like every other selector option — Browse was the
    // one path that changed the interpreter without an audit trail.
    invoke("log_python_change", { fromEnv: oldPath, toEnv: pyExe });
  });
  popup.appendChild(browseItem);

  document.body.appendChild(popup);

  // Close on outside click
  const close = (e: MouseEvent) => {
    if (!popup.contains(e.target as Node)) {
      popup.remove();
      document.removeEventListener("click", close);
    }
  };
  setTimeout(() => document.addEventListener("click", close), 0);
}

function updateStatusBar(event: ActivityEvent): void {
  document.getElementById("status-warnings")!.textContent = `Warnings: ${warningCount}`;
  const dot = document.getElementById("monitor-dot")!;
  if (warningCount > 5) dot.className = "status-dot alert";
  else if (warningCount > 0) dot.className = "status-dot warning";

  if (event.event_type === "focus_lost") document.getElementById("status-focus")!.textContent = "Focus: LOST";
  else if (event.event_type === "focus_returned") document.getElementById("status-focus")!.textContent = "Focus: OK";

  if (event.event_type.startsWith("clipboard")) {
    document.getElementById("status-clipboard")!.textContent = "Clipboard: Changed";
    setTimeout(() => { document.getElementById("status-clipboard")!.textContent = "Clipboard: Idle"; }, 3000);
  }
}

// ===== Backend Event Listener =====
async function listenForBackendEvents(): Promise<void> {
  await listen<ActivityEvent>("activity-event", (event) => {
    const ev = event.payload;
    appendLogEntry(ev);
    // Capture clipboard events so paste source can be attributed accurately.
    if (ev.event_type === "clipboard_internal" || ev.event_type === "clipboard_external") {
      const m = ev.detail.match(/^\[Source: ([^\]\s(]+)(?:\s*\(([^)]*)\))?\]/);
      const source = m?.[1] ?? "unknown";
      const windowTitle = m?.[2] ?? "";
      noteClipboardEvent({
        source,
        windowTitle,
        isExternal: ev.event_type === "clipboard_external",
        epochMs: Date.now(),
      });
    }
    // Recording health failure: backend detected a dead capture process or a
    // non-growing file — most commonly Screen Recording permission denied on
    // macOS. Alert the student loudly before they finish the exam thinking it
    // was recording.
    if (ev.event_type === "recording_health_fail") {
      alert(`[녹화 문제]\n\n${ev.detail}`);
      void maybeRestartRecording();
    }
    // Monitoring permission failure (macOS Automation denied): one-time
    // backend event — without the permission, focus/clipboard-source
    // monitoring silently records nothing all exam.
    if (ev.event_type === "monitor_health_fail") {
      alert(`[모니터링 권한 문제]\n\n${ev.detail}`);
    }
  });

  // Real-time code output
  await listen<{ stream: string; text: string }>("run-output", (event) => {
    if (!isRunning) return; // ignore straggler chunks after Stop/from a previous run
    // NOTE: no isNotebookRunning() guard — notebook runs use run_code_sync and
    // never emit run-output/run-done, so guarding on it only served to DROP a
    // streaming .py run's events when a notebook cell happened to run
    // concurrently, wedging the Run button at "Stop". isRunning already scopes
    // these handlers to streaming runs.
    const { stream, text } = event.payload;
    if (stream === "stderr") {
      streamingOutputReceived = true;
      appendOutput(text, "error");
      highlightErrorLine(text);
    } else if (stream === "system") {
      appendOutput(text, "system");
    } else {
      streamingOutputReceived = true;
      appendOutput(text, "stdout");
    }
  });

  // Code execution finished
  await listen<{ exit_code: number | null; duration_ms: number; stdout: string; stderr: string }>("run-done", (event) => {
    if (!isRunning) return; // streaming-run scope only (notebook uses run_code_sync)

    const { exit_code, duration_ms, stdout, stderr } = event.payload;

    // Fallback ONLY if no run-output stream events arrived. The previous
    // heuristic (read outputEl.textContent and check for non-header lines)
    // raced with appendOutput's rAF batching: a fast program would emit
    // stdout via run-output, the DOM flush hadn't happened yet, run-done
    // saw an "empty" output panel and re-appended the collected stdout —
    // result: "Hello, MINT" printed twice for a single Run.
    if (!streamingOutputReceived) {
      if (stdout) appendOutput(stdout, "stdout");
      if (stderr) {
        appendOutput(stderr, "error");
        for (const line of stderr.split("\n")) {
          highlightErrorLine(line);
        }
      }
    }
    streamingOutputReceived = false;  // reset for next run

    const status = exit_code === 0 ? "OK" : `exit code ${exit_code}`;
    appendOutput(`--- Finished (${status}, ${duration_ms}ms) ---\n\n`, "system");

    // Log terminal output for audit trail
    const truncStdout = stdout.length > 2000 ? stdout.substring(0, 2000) + "...(truncated)" : stdout;
    const truncStderr = stderr.length > 1000 ? stderr.substring(0, 1000) + "...(truncated)" : stderr;
    if (stdout) {
      invoke("log_editor_event", { eventType: "terminal_stdout", detail: truncStdout, charCount: stdout.length, timeDeltaMs: duration_ms });
    }
    if (stderr) {
      invoke("log_editor_event", { eventType: "terminal_stderr", detail: truncStderr, charCount: stderr.length, timeDeltaMs: duration_ms });
    }

    resetRunButton();
  });
}

// ===== Export =====
// ===== Helpers =====
function formatEventType(type: string): string {
  const map: Record<string, string> = {
    session_start: "SESSION", clipboard_internal: "CLIP-INT", clipboard_external: "CLIP-EXT",
    focus_lost: "FOCUS-LOST", focus_returned: "FOCUS-BACK", paste: "PASTE",
    paste_large: "PASTE-LRG", input_burst: "BURST", typing_summary: "TYPING",
    code_run: "RUN", code_run_result: "RUN-RESULT",
    recording_start: "REC-START", recording_stop: "REC-STOP",
    recording_health_fail: "REC-FAIL", monitor_health_fail: "MON-FAIL",
    file_import: "IMPORT",
    copy: "COPY",
    cut: "CUT",
    terminal_stdout: "STDOUT",
    terminal_stderr: "STDERR",
    tamper_detected: "TAMPER",
    tamper_new_file: "TAMPER-NEW",
    tamper_deleted: "TAMPER-DEL",
  };
  return map[type] || type.toUpperCase();
}

function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

function escapeAttr(text: string): string {
  return text.replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

// ===== Default Test Files =====
const EMPTY_MAIN_PY = `# MINT Exam IDE
# Ctrl+S to save, Ctrl+R to run
`;

const DEFAULT_MAIN_PY = `# MINT Exam IDE — Write your code here
print("Hello, MINT!")

# Try: Ctrl+S to save, Ctrl+R to run
# Run test_all.py to verify all libraries
`;

const DEFAULT_TEST_PY = `"""MINT Exam IDE — Library Test (skips packages not installed)"""
import sys
print(f"Python: {sys.executable}")
print(f"Version: {sys.version}")

import csv, json, os, math, random, statistics
import collections, itertools, re, datetime
print("Built-in modules: OK")

import numpy as np
import pandas as pd
print(f"NumPy {np.__version__}: mean([1..5]) = {np.mean([1,2,3,4,5])}")

df = pd.DataFrame({"Name": ["A","B","C"], "Score": [95, 88, 72]})
print(f"Pandas {pd.__version__}:")
print(df)

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

fig, ax = plt.subplots(1, 2, figsize=(10, 4))
ax[0].hist(np.random.randn(300), bins=20, color='#89b4fa')
ax[0].set_title('Histogram')
try:
    import seaborn as sns
    sns.barplot(data=df, x="Name", y="Score", ax=ax[1])
    ax[1].set_title('Scores (seaborn)')
    print("Seaborn: OK")
except ImportError:
    ax[1].bar(df["Name"], df["Score"], color='#a6e3a1')
    ax[1].set_title('Scores (matplotlib fallback)')
    print("Seaborn: not installed (optional)")
plt.tight_layout()
plt.savefig("test_chart.png", dpi=100)
plt.close()
print("Matplotlib: OK (test_chart.png saved)")

try:
    from sklearn.linear_model import LinearRegression
    X = np.array([[1],[2],[3],[4],[5]])
    model = LinearRegression().fit(X, [2, 4, 5, 4, 5])
    print(f"sklearn: predict(6) = {model.predict([[6]])[0]:.2f}")
except ImportError:
    print("sklearn: not installed (optional)")

try:
    from scipy import optimize
    r = optimize.minimize(lambda x: (x-3)**2, x0=0)
    print(f"SciPy: min of (x-3)^2 at x = {r.x[0]:.2f}")
except ImportError:
    print("SciPy: not installed (optional)")

try:
    import sympy as sp
    x = sp.Symbol('x')
    print(f"SymPy: integral(x^2) = {sp.integrate(x**2, x)}")
except ImportError:
    print("SymPy: not installed (optional)")

from PIL import Image
img = Image.new('RGB', (50, 50), color='blue')
img.save('test_img.png')
print("Pillow: OK (test_img.png saved)")

try:
    import cv2
    gray = cv2.imread('test_img.png', cv2.IMREAD_GRAYSCALE)
    print(f"OpenCV: shape={gray.shape}")
except ImportError:
    print("OpenCV: not installed (optional)")

try:
    import openpyxl
    df.to_excel("test.xlsx", index=False)
    print("openpyxl: OK (test.xlsx saved)")
except ImportError:
    print("openpyxl: not installed (optional)")

df.to_csv("test.csv", index=False)
with open("test.json", "w") as f:
    json.dump({"result": "pass"}, f, indent=2)
print("test.csv, test.json saved")

try:
    import torch
    t = torch.tensor([1.0, 2.0, 3.0])
    print(f"PyTorch {torch.__version__}: mean = {t.mean():.2f}")
except ImportError:
    print("PyTorch: not installed (optional)")

try:
    os.environ['TF_CPP_MIN_LOG_LEVEL'] = '3'
    import tensorflow as tf
    print(f"TensorFlow {tf.__version__}: 1+2 = {tf.add(1, 2).numpy()}")
except ImportError:
    print("TensorFlow: not installed (optional)")

try:
    import requests
    r = requests.get("https://httpbin.org/get", timeout=3)
    print(f"Requests: status {r.status_code}")
except Exception:
    print("Requests: network unavailable or not installed")

print("\\n=== TESTS COMPLETE ===")
`;

const DEFAULT_MATH_HELPER = `def add(a, b):
    return a + b

def multiply(a, b):
    return a * b

def factorial(n):
    if n <= 1:
        return 1
    return n * factorial(n - 1)
`;

const DEFAULT_TEXT_HELPER = `def greet(name):
    return f"Hello, {name}!"

def reverse(text):
    return text[::-1]
`;

const DEFAULT_IMPORT_TEST = `"""Test: folder import (utils package)"""
from utils import add, multiply, greet
from utils.math_helper import factorial
from utils.text_helper import reverse

print(f"add(3, 5) = {add(3, 5)}")
print(f"multiply(4, 7) = {multiply(4, 7)}")
print(f"factorial(6) = {factorial(6)}")
print(f"greet('MINT') = {greet('MINT')}")
print(f"reverse('hello') = {reverse('hello')}")
print("\\nFolder import test passed!")
`;

const DEFAULT_POPUP_TEST = `"""Test: matplotlib popup window"""
import matplotlib.pyplot as plt

plt.figure(figsize=(6, 4))
plt.plot([1, 4, 9, 16, 25], 'ro-', label='squares')
plt.title('Popup Test — close this window')
plt.legend()
plt.show()
print("Popup closed successfully!")
`;

const DEFAULT_NOTEBOOK = JSON.stringify({
  cells: [
    { cell_type: "markdown", source: ["# MINT Exam IDE — Notebook Test\n", "Run each cell to verify."], metadata: {}, outputs: [] },
    { cell_type: "code", source: ["import numpy as np\n", "print(f'NumPy: {np.mean([1,2,3,4,5])}')"], metadata: {}, outputs: [], execution_count: null },
    { cell_type: "code", source: ["import pandas as pd\n", "df = pd.DataFrame({'A': [1,2,3], 'B': [4,5,6]})\n", "print(df)"], metadata: {}, outputs: [], execution_count: null },
    { cell_type: "code", source: ["import matplotlib\n", "matplotlib.use('Agg')\n", "import matplotlib.pyplot as plt\n", "plt.plot([1,4,9,16], 'ro-')\n", "plt.savefig('nb_plot.png')\n", "plt.close()\n", "print('nb_plot.png saved')"], metadata: {}, outputs: [], execution_count: null },
    { cell_type: "code", source: ["# Intentional error test\n", "print('before error')\n", "print(1/0)"], metadata: {}, outputs: [], execution_count: null },
  ],
  metadata: { kernelspec: { display_name: "Python 3", language: "python", name: "python3" }, language_info: { name: "python" } },
  nbformat: 4, nbformat_minor: 5,
}, null, 1);

function langFromExtension(name: string): SupportedLanguage | null {
  const ext = name.split(".").pop()?.toLowerCase();
  const map: Record<string, SupportedLanguage> = {
    py: "python", js: "javascript", ts: "typescript", java: "java",
    c: "c", cpp: "cpp", cc: "cpp", cxx: "cpp", h: "c", hpp: "cpp",
    ipynb: "python",
  };
  return ext ? map[ext] ?? null : null;
}
