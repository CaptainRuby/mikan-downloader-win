!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "MikanRssDownloader"
    nsExec::ExecToLog 'schtasks.exe /Delete /TN "MikanRssDownloader" /F'

    SetShellVarContext current
    RMDir /r "$APPDATA\${BUNDLEID}"
    RMDir /r "$LOCALAPPDATA\${BUNDLEID}"

    DeleteRegKey HKCU "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty HKCU "${MANUKEY}"
    DeleteRegKey HKLM "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty HKLM "${MANUKEY}"

    RMDir /r "$INSTDIR\data"
    RMDir "$INSTDIR"
  ${EndIf}
!macroend
