!macro NSIS_HOOK_PREINSTALL
  ReadRegStr $R9 SHCTX "${UNINSTKEY}" "UninstallString"
  ${If} $R9 != ""
    DetailPrint "Removing previous installation..."
    ExecWait '$R9 /S'
    Sleep 2000
  ${EndIf}
  ; Do NOT delete setup_config.json on update — that would wipe the student's
  ; custom_venv_path (set when a non-ASCII %LOCALAPPDATA% forced the
  ; ProgramData fallback). Wiping it makes the Korean-username PC retry the
  ; broken venv every update. The wizard's first-launch check (setup_done)
  ; gates re-entry; we don't need to nuke the file.
!macroend
