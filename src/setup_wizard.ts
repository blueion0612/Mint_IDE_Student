import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

export interface SetupConfig {
  setup_done: boolean;
  package_profile: string; // "basic" | "ds" | "dl" | "custom" | "none"
  custom_packages: string[];
  recording_enabled: boolean;
  include_sample_code: boolean;
  config_version: number;
  custom_venv_path?: string | null;
}

const PROFILE_LABELS: Record<string, string> = {
  basic: "기본 (numpy / pandas / matplotlib / Pillow / openpyxl / requests)",
  ds: "데이터사이언스 (+ seaborn / sklearn / scipy / sympy / opencv)",
  dl: "딥러닝 (+ torch / torchvision / tensorflow-cpu) — 용량 큼",
  none: "설치 안 함 (기존 환경 그대로 사용)",
};

const COMMON_CUSTOM_PACKAGES = [
  "numpy", "pandas", "matplotlib", "seaborn", "scikit-learn", "scipy", "sympy",
  "Pillow", "opencv-python-headless", "openpyxl", "requests",
  "torch", "torchvision", "tensorflow-cpu",
  "transformers", "jupyter", "plotly", "beautifulsoup4",
];

export async function loadConfig(): Promise<SetupConfig> {
  return await invoke<SetupConfig>("read_setup_config");
}

export async function saveConfig(config: SetupConfig): Promise<void> {
  await invoke("write_setup_config", { config });
}

export async function showSetupWizard(pythonPath?: string | null): Promise<SetupConfig> {
  const initial = await loadConfig();
  return await openModal({
    mode: "wizard",
    initial,
    title: "MINT IDE 초기 설정",
    submitLabel: "설치 및 시작",
    pythonPath: pythonPath ?? null,
  });
}

export async function showSettingsModal(currentPython: string | null): Promise<SetupConfig | null> {
  const initial = await loadConfig();
  let installed: string[] = [];
  try {
    installed = await invoke<string[]>("list_installed_packages", { pythonPath: currentPython });
  } catch { /* ignore */ }
  let defaultVenv = "";
  let currentVenv = "";
  try {
    defaultVenv = await invoke<string>("get_default_venv_path");
    currentVenv = await invoke<string>("get_current_venv_path");
  } catch { /* ignore */ }
  let buildInfo: { commit_sha: string; build_time: string; exe_hash: string } | null = null;
  try {
    buildInfo = await invoke("get_build_info");
  } catch { /* ignore */ }
  return await openModal({
    mode: "settings",
    initial,
    title: "MINT IDE 설정",
    submitLabel: "변경 사항 적용",
    installedPackages: installed,
    pythonPath: currentPython,
    defaultVenvPath: defaultVenv,
    currentVenvPath: currentVenv,
    buildInfo,
  });
}

interface ModalOptions {
  mode: "wizard" | "settings";
  initial: SetupConfig;
  title: string;
  submitLabel: string;
  installedPackages?: string[];
  pythonPath?: string | null;
  defaultVenvPath?: string;
  currentVenvPath?: string;
  buildInfo?: { commit_sha: string; build_time: string; exe_hash: string } | null;
}

function openModal(opts: ModalOptions): Promise<SetupConfig> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.id = "setup-wizard-overlay";
    overlay.className = "wizard-overlay";

    const customSet = new Set(opts.initial.custom_packages);
    let profile = opts.initial.package_profile || "basic";
    let recording = opts.initial.recording_enabled;
    let sampleCode = opts.initial.include_sample_code;
    let venvMode: "default" | "custom" = opts.initial.custom_venv_path ? "custom" : "default";
    let customVenvPath: string = opts.initial.custom_venv_path ?? "";

    overlay.innerHTML = `
      <div class="wizard-modal">
        <div class="wizard-header">
          <div class="wizard-title">${escapeHtml(opts.title)}</div>
          ${opts.mode === "settings" ? '<button class="wizard-close" id="wiz-close">&times;</button>' : ""}
        </div>

        <div class="wizard-body">
          <section class="wiz-section">
            <div class="wiz-section-title">1. Python 패키지 프로파일</div>
            <div class="wiz-section-hint">설치 용량에 큰 차이가 있으니 신중히 선택하세요.</div>
            <div id="wiz-profile-list">
              ${["basic", "ds", "dl", "none"].map(p => `
                <label class="wiz-radio">
                  <input type="radio" name="profile" value="${p}" ${profile === p ? "checked" : ""} />
                  <span class="wiz-radio-label">${escapeHtml(PROFILE_LABELS[p])}</span>
                </label>
              `).join("")}
              <label class="wiz-radio">
                <input type="radio" name="profile" value="custom" ${profile === "custom" ? "checked" : ""} />
                <span class="wiz-radio-label">사용자 지정 (아래에서 개별 선택)</span>
              </label>
            </div>
            <div id="wiz-custom-packages" class="wiz-custom-pkg-grid${profile === "custom" ? "" : " hidden"}">
              ${COMMON_CUSTOM_PACKAGES.map(pkg => `
                <label class="wiz-check">
                  <input type="checkbox" data-pkg="${escapeAttr(pkg)}" ${customSet.has(pkg) ? "checked" : ""} />
                  <span>${escapeHtml(pkg)}</span>
                </label>
              `).join("")}
            </div>
            ${opts.mode === "settings" && opts.installedPackages && opts.installedPackages.length > 0 ? `
              <details class="wiz-installed-details">
                <summary>현재 설치된 패키지 (${opts.installedPackages.length}개) — 클릭하여 펼치기</summary>
                <div class="wiz-installed-list">${opts.installedPackages.map(p => escapeHtml(p)).join(", ")}</div>
              </details>
            ` : ""}
          </section>

          <section class="wiz-section">
            <div class="wiz-section-title">2. 영상 녹화</div>
            <label class="wiz-check wiz-check-block">
              <input type="checkbox" id="wiz-recording" ${recording ? "checked" : ""} />
              <span>시험 중 화면 녹화 사용 (FFmpeg 필요)</span>
            </label>
            <div class="wiz-section-hint">제출 zip엔 포함되지 않으며, 별도 폴더에 저장됩니다.</div>
          </section>

          <section class="wiz-section">
            <div class="wiz-section-title">3. 샘플 / 테스트 코드</div>
            <label class="wiz-check wiz-check-block">
              <input type="checkbox" id="wiz-sample" ${sampleCode ? "checked" : ""} />
              <span>main.py / test_*.py / utils/ 등 예제 파일 생성</span>
            </label>
            <div class="wiz-section-hint">시험 환경에서는 끄는 것을 권장합니다. 끄면 빈 워크스페이스로 시작합니다.</div>
            ${opts.mode === "settings" ? `
              <div class="wiz-extra-actions">
                <button class="wiz-mini-btn" id="wiz-create-sample">현재 워크스페이스에 샘플 생성</button>
                <button class="wiz-mini-btn wiz-danger" id="wiz-delete-sample">현재 워크스페이스의 샘플 삭제</button>
              </div>
            ` : ""}
          </section>

          ${opts.mode === "settings" ? `
            <section class="wiz-section">
              <div class="wiz-section-title">4. Python 환경 (venv)</div>
              <div class="wiz-section-hint">현재: <code>${escapeHtml(opts.currentVenvPath ?? "")}</code></div>
              <label class="wiz-radio">
                <input type="radio" name="venv-mode" value="default" ${!opts.initial.custom_venv_path ? "checked" : ""} />
                <span class="wiz-radio-label">Default 경로 사용 (<code>${escapeHtml(opts.defaultVenvPath ?? "")}</code>)</span>
              </label>
              <label class="wiz-radio">
                <input type="radio" name="venv-mode" value="custom" ${opts.initial.custom_venv_path ? "checked" : ""} />
                <span class="wiz-radio-label">자체 지정 경로 사용</span>
              </label>
              <div id="venv-custom-row" class="wiz-venv-row${opts.initial.custom_venv_path ? "" : " hidden"}">
                <input type="text" id="venv-custom-path" class="wiz-path-input" readonly value="${escapeAttr(opts.initial.custom_venv_path ?? "")}" placeholder="폴더 선택 버튼을 누르세요" />
                <button class="wiz-mini-btn" id="venv-browse">폴더 선택...</button>
              </div>
              <div class="wiz-extra-actions">
                <button class="wiz-mini-btn wiz-danger" id="venv-recreate">venv 재설치 (기존 삭제 후 생성)</button>
              </div>
              <div class="wiz-section-hint">경로 변경은 [변경 사항 적용] 후 적용됩니다. 재설치는 즉시.</div>
            </section>

            <section class="wiz-section">
              <div class="wiz-section-title">5. 초기 설정</div>
              <button class="wiz-mini-btn" id="wiz-rerun-wizard">초기 설정 위자드 다시 열기</button>
            </section>

            ${opts.buildInfo ? `
              <section class="wiz-section">
                <div class="wiz-section-title">6. 빌드 정보</div>
                <div class="wiz-build-info">
                  <div><span class="wiz-build-label">commit</span> <code>${escapeHtml(opts.buildInfo.commit_sha)}</code></div>
                  <div><span class="wiz-build-label">build</span> <code>${escapeHtml(opts.buildInfo.build_time)}</code></div>
                  <div><span class="wiz-build-label">exe SHA-256</span> <code class="wiz-build-hash">${escapeHtml(opts.buildInfo.exe_hash)}</code></div>
                </div>
                <div class="wiz-section-hint">제출 시 manifest.json에 자동 기록되어 채점자가 IDE 무결성을 확인할 수 있습니다.</div>
              </section>
            ` : ""}
          ` : ""}

          <section class="wiz-progress hidden" id="wiz-progress-section">
            <div class="wiz-section-title">설치 진행 중...</div>
            <pre id="wiz-progress-log" class="wiz-progress-log"></pre>
          </section>
        </div>

        <div class="wizard-footer">
          ${opts.mode === "settings" ? '<button class="btn" id="wiz-cancel">취소</button>' : ""}
          <button class="btn btn-accent" id="wiz-submit">${escapeHtml(opts.submitLabel)}</button>
        </div>
      </div>
    `;

    document.body.appendChild(overlay);

    const profileRadios = overlay.querySelectorAll<HTMLInputElement>('input[name="profile"]');
    const customGrid = overlay.querySelector("#wiz-custom-packages") as HTMLElement;
    profileRadios.forEach(r => r.addEventListener("change", () => {
      profile = r.value;
      if (r.checked) {
        if (profile === "custom") customGrid.classList.remove("hidden");
        else customGrid.classList.add("hidden");
      }
    }));

    const recCheck = overlay.querySelector("#wiz-recording") as HTMLInputElement;
    recCheck.addEventListener("change", () => { recording = recCheck.checked; });

    const sampleCheck = overlay.querySelector("#wiz-sample") as HTMLInputElement;
    sampleCheck.addEventListener("change", () => { sampleCode = sampleCheck.checked; });

    const collectCustom = (): string[] => {
      const out: string[] = [];
      overlay.querySelectorAll<HTMLInputElement>('input[data-pkg]').forEach(c => {
        if (c.checked) out.push(c.dataset.pkg!);
      });
      return out;
    };

    const closeModal = () => overlay.remove();

    if (opts.mode === "settings") {
      const closeBtn = overlay.querySelector("#wiz-close")!;
      const cancelBtn = overlay.querySelector("#wiz-cancel")!;
      closeBtn.addEventListener("click", () => { closeModal(); resolve(opts.initial); });
      cancelBtn.addEventListener("click", () => { closeModal(); resolve(opts.initial); });

      overlay.querySelector("#wiz-create-sample")!.addEventListener("click", async () => {
        try {
          await (window as any).__mintCreateSampleFiles();
          alert("샘플 파일이 생성되었습니다.");
        } catch (e) {
          alert("실패: " + e);
        }
      });
      overlay.querySelector("#wiz-delete-sample")!.addEventListener("click", async () => {
        if (!confirm("현재 워크스페이스의 샘플 파일을 삭제하시겠습니까?")) return;
        try {
          const removed = await invoke<number>("delete_sample_files");
          alert(`${removed}개 항목 삭제됨`);
          await (window as any).__mintRefreshFileTree();
        } catch (e) {
          alert("실패: " + e);
        }
      });
      overlay.querySelector("#wiz-rerun-wizard")!.addEventListener("click", async () => {
        closeModal();
        const cfg = await showSetupWizard();
        resolve(cfg);
      });

      const venvRadios = overlay.querySelectorAll<HTMLInputElement>('input[name="venv-mode"]');
      const customRow = overlay.querySelector("#venv-custom-row") as HTMLElement;
      const customInput = overlay.querySelector("#venv-custom-path") as HTMLInputElement;
      venvRadios.forEach(r => r.addEventListener("change", () => {
        if (r.checked) {
          venvMode = r.value as "default" | "custom";
          if (venvMode === "custom") customRow.classList.remove("hidden");
          else customRow.classList.add("hidden");
        }
      }));

      overlay.querySelector("#venv-browse")!.addEventListener("click", async () => {
        const picked = await openDialog({ directory: true, title: "venv를 설치할 폴더를 선택하세요" });
        if (!picked) return;
        const dir = typeof picked === "string" ? picked : String(picked);
        const finalPath = dir.endsWith("exam-venv") || dir.endsWith("exam-venv/") || dir.endsWith("exam-venv\\")
          ? dir
          : `${dir.replace(/[\\/]+$/, "")}/exam-venv`;
        customVenvPath = finalPath;
        customInput.value = finalPath;
      });

      overlay.querySelector("#venv-recreate")!.addEventListener("click", async () => {
        if (!confirm("기존 venv를 삭제하고 새로 생성합니다. 기존 패키지는 모두 지워지며 재설치 필요. 진행하시겠습니까?")) return;
        const targetPath = venvMode === "custom"
          ? (customVenvPath || null)
          : null;
        if (venvMode === "custom" && !customVenvPath) {
          alert("자체 지정 모드인데 경로가 비어있습니다. 폴더를 먼저 선택해 주세요.");
          return;
        }
        const btn = overlay.querySelector("#venv-recreate") as HTMLButtonElement;
        btn.disabled = true;
        const originalText = btn.textContent;
        btn.textContent = "재설치 중...";
        try {
          const newPath = await invoke<string>("recreate_venv", { path: targetPath });
          alert(`venv 재설치 완료:\n${newPath}\n\n패키지는 [변경 사항 적용] 시 선택한 프로파일로 설치됩니다.`);
        } catch (e) {
          alert(`venv 재설치 실패: ${e}`);
        } finally {
          btn.disabled = false;
          btn.textContent = originalText;
        }
      });
    }

    overlay.querySelector("#wiz-submit")!.addEventListener("click", async () => {
      const newConfig: SetupConfig = {
        setup_done: true,
        package_profile: profile,
        custom_packages: profile === "custom" ? collectCustom() : opts.initial.custom_packages,
        recording_enabled: recording,
        include_sample_code: sampleCode,
        config_version: 2,
        custom_venv_path: venvMode === "custom" && customVenvPath ? customVenvPath : null,
      };

      const packages = await invoke<string[]>("package_list_for_profile", {
        profile,
        custom: newConfig.custom_packages,
      });

      const wantsInstall = packages.length > 0 && (
        opts.mode === "wizard" ||
        JSON.stringify(packages.sort()) !== JSON.stringify(
          (await invoke<string[]>("package_list_for_profile", {
            profile: opts.initial.package_profile,
            custom: opts.initial.custom_packages,
          })).sort()
        )
      );

      const submitBtn = overlay.querySelector("#wiz-submit") as HTMLButtonElement;
      submitBtn.disabled = true;
      submitBtn.textContent = "처리 중...";

      try {
        await saveConfig(newConfig);

        if (wantsInstall) {
          await runInstall(overlay, packages, opts.pythonPath ?? null);
        }
      } catch (e) {
        alert("설정 저장 실패: " + e);
        submitBtn.disabled = false;
        submitBtn.textContent = opts.submitLabel;
        return;
      }

      closeModal();
      resolve(newConfig);
    });
  });
}

async function runInstall(
  overlay: HTMLElement,
  packages: string[],
  pythonPath: string | null,
): Promise<void> {
  const progressSection = overlay.querySelector("#wiz-progress-section") as HTMLElement;
  const log = overlay.querySelector("#wiz-progress-log") as HTMLElement;
  progressSection.classList.remove("hidden");
  log.textContent = `Installing ${packages.length} packages...\n`;

  return new Promise<void>((resolve) => {
    let unlisten: UnlistenFn | null = null;
    listen<{ stream: string; text: string }>("run-output", (event) => {
      const { stream, text } = event.payload;
      log.textContent += text;
      log.scrollTop = log.scrollHeight;
      if (stream === "system" && text.startsWith("[INSTALL_DONE")) {
        if (unlisten) unlisten();
        resolve();
      }
    }).then(fn => { unlisten = fn; });

    invoke("install_packages_smart", {
      packages,
      pythonPath,
    }).catch(e => {
      log.textContent += `\nError: ${e}\n`;
      if (unlisten) unlisten();
      resolve();
    });
  });
}

function escapeHtml(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

function escapeAttr(text: string): string {
  return text.replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
