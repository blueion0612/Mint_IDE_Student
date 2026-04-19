!macro NSIS_HOOK_PREINSTALL
  ReadRegStr $R9 SHCTX "${UNINSTKEY}" "UninstallString"
  ${If} $R9 != ""
    DetailPrint "Removing previous installation..."
    ExecWait '$R9 /S'
    Sleep 2000
  ${EndIf}
  Delete "$LOCALAPPDATA\MINT_Exam_IDE\setup_config.json"
!macroend
