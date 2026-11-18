; Arcade V1.0 one-click installer for Windows 10/11 x64: arcade-v1-windows-x86_64-setup.exe
;
; Double-click, and it installs for the current user (no administrator password), starts the
; arcade service now and at every login, adds Arcade V1.0 to the Start menu and the desktop, and
; opens it. Running a newer Setup.exe later updates in place; the wallet and chains are kept.
; It lists itself in Settings > Apps, where Uninstall removes the programs and keeps the data.
;
; All the installing is scripts\install.ps1 (--prebuilt): this only carries the release folder to
; it, so the two never drift apart. Built by the release workflow from the packaged folder:
;
;   makensis /NOCD /DVERSION=v0.1.77 /DPKG=arcade-v1-windows-x86_64 /DOUT=arcade-v1-windows-x86_64-setup.exe scripts\windows\setup.nsi
;
; (/NOCD: PKG and OUT are relative to where makensis runs. On Linux: apt install nsis, and -NOCD -D...)

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user
ManifestDPIAware true

!ifndef VERSION
  !define VERSION "dev"
!endif
!ifndef PKG
  !define PKG "arcade-v1-windows-x86_64"
!endif
!ifndef OUT
  !define OUT "arcade-v1-windows-x86_64-setup.exe"
!endif

!define NAME "Arcade V1.0"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ArcadeV1"

Name "${NAME}"
OutFile "${OUT}"
Icon "${PKG}\icon.ico"
UninstallIcon "${PKG}\icon.ico"
InstallDir "$LOCALAPPDATA\Programs\Arcade"
BrandingText "${NAME} ${VERSION}"
ShowInstDetails show
ShowUninstDetails show

!include "MUI2.nsh"
!include "x64.nsh"
!define MUI_ICON "${PKG}\icon.ico"
!define MUI_UNICON "${PKG}\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open Arcade V1.0"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenArcade
!define MUI_FINISHPAGE_TEXT "Arcade V1.0 is installed, and its service is running: it starts by itself every time you log in.$\r$\n$\r$\nOpen it from the Start menu or the desktop. The first time, make a wallet (write its 12 words down) or restore one."
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; makensis from the release image is the stock build, so LogSet is not available.
; Every silent failure appends a line here and then starts the node that was already installed.
!define INSTALL_LOG "$TEMP\arcade-v1-install.log"
Var FailReason
Var LogLine

Function AppendLog
  Push $R9
  ClearErrors
  FileOpen $R9 "${INSTALL_LOG}" a
  IfErrors append_done
  FileSeek $R9 0 END
  FileWrite $R9 "$LogLine$\r$\n"
  FileClose $R9
  append_done:
  ClearErrors
  Pop $R9
FunctionEnd

; install.ps1 never ran on the failed /S install, so the NSIS side has to bring the old node
; back itself. Enable covers a task Stop-Arcade already disabled. /Run is a no-op while the
; task is already running (MultipleInstances IgnoreNew).
Function RestartOldNode
  Push $R8
  ${If} ${RunningX64}
    ${DisableX64FSRedirection}
  ${EndIf}
  StrCpy $LogLine "Enabling and running the existing Arcade V1.0 login task."
  Call AppendLog
  ExecWait '"$WINDIR\System32\schtasks.exe" /Change /TN "Arcade V1.0" /ENABLE' $R8
  StrCpy $LogLine "schtasks /Change /ENABLE returned $R8"
  Call AppendLog
  ExecWait '"$WINDIR\System32\schtasks.exe" /Run /TN "Arcade V1.0"' $R8
  StrCpy $LogLine "schtasks /Run returned $R8"
  Call AppendLog
  ${If} ${RunningX64}
    ${EnableX64FSRedirection}
  ${EndIf}
  Pop $R8
FunctionEnd

Function .onInit
  StrCpy $FailReason "failed before the install section"
  StrCpy $LogLine "setup.exe started. A silent failure is written here and the existing node is started again."
  Call AppendLog
FunctionEnd

Function .onInstFailed
  StrCpy $LogLine "NSIS aborted the install: $FailReason"
  Call AppendLog
  Call RestartOldNode
FunctionEnd

VIProductVersion "0.0.0.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${NAME} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Open source"

Function OpenArcade
  ; The same launcher as the Start menu shortcut: it sets the data folder for the app.
  IfFileExists "$INSTDIR\arcade-desktop.vbs" 0 +2
    Exec '"$WINDIR\System32\wscript.exe" //B "$INSTDIR\arcade-desktop.vbs"'
FunctionEnd

Section "Arcade V1.0" SecMain
  SectionIn RO
  ; /S never shows the MUI pages, and those pages are what used to call InitPluginsDir.
  ; Without this, $PLUGINSDIR is empty and SetOutPath below lands on \pkg at the drive root.
  StrCpy $FailReason "could not create the plug-in directory"
  StrCpy $LogLine "Creating the plug-in directory before unpacking ${NAME} ${VERSION}."
  Call AppendLog
  InitPluginsDir

  DetailPrint "Unpacking ${NAME} ${VERSION}..."
  StrCpy $FailReason "failed while unpacking $PLUGINSDIR\pkg, before PowerShell was started"
  StrCpy $LogLine "Unpacking ${NAME} ${VERSION} into $PLUGINSDIR\pkg"
  Call AppendLog
  SetOutPath "$PLUGINSDIR\pkg"
  File /r "${PKG}\*"

  DetailPrint "Installing (this stops a running arcade first so it can finish writing)..."
  StrCpy $FailReason "PowerShell was not started, so install.ps1 did not run"
  StrCpy $LogLine "Unpacked. Starting the 64-bit PowerShell for install.ps1."
  Call AppendLog
  ; ARCADE_NO_OPEN: the finish page offers to open it instead, also on updates.
  ; This stub is 32-bit. Leaving redirection on makes System32\powershell.exe the
  ; SysWOW64 copy, and that copy never started an engine on the failed /S run.
  ${If} ${RunningX64}
    ${DisableX64FSRedirection}
  ${EndIf}
  System::Call 'kernel32::SetEnvironmentVariable(t "ARCADE_NO_OPEN", t "1")'
  nsExec::ExecToLog '"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\pkg\scripts\install.ps1" --prebuilt "$PLUGINSDIR\pkg" --log "${INSTALL_LOG}"'
  Pop $0
  ${If} ${RunningX64}
    ${EnableX64FSRedirection}
  ${EndIf}
  StrCmp $0 "0" installed
    StrCpy $FailReason "install.ps1 / nsExec returned $0"
    StrCpy $LogLine "Install failed (nsExec $0). The log is ${INSTALL_LOG}. Starting the previous node before the dialog."
    Call AppendLog
    Call RestartOldNode
    MessageBox MB_OK|MB_ICONSTOP "The install did not finish (code $0). The log is$\r$\n${INSTALL_LOG}" /SD IDOK
    SetOutPath "$TEMP"
    Abort "Install failed."
  installed:

  SetOutPath "$TEMP"
  IfFileExists "$INSTDIR\arcade-desktop.vbs" 0 +2
    CreateShortcut "$DESKTOP\Arcade V1.0.lnk" "$WINDIR\System32\wscript.exe" '//B "$INSTDIR\arcade-desktop.vbs"' "$INSTDIR\arcade-v1.ico" 0 SW_SHOWMINIMIZED "" "Arcade V1.0"

  WriteUninstaller "$INSTDIR\Uninstall Arcade V1.0.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "ARCADE"
  WriteRegStr HKCU "${UNINST_KEY}" "URLInfoAbout" "https://github.com/martinseeger2002/arcade-v1"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\arcade-v1.ico"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\Uninstall Arcade V1.0.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall Arcade V1.0.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  ; Programs, login task, shortcuts and PATH entry go; the data folder (wallet, 12 words,
  ; chains) stays, so installing again picks it up.
  SetOutPath "$PLUGINSDIR"
  File "/oname=$PLUGINSDIR\uninstall.ps1" "${PKG}\scripts\uninstall.ps1"
  SetOutPath "$TEMP"
  ${If} ${RunningX64}
    ${DisableX64FSRedirection}
  ${EndIf}
  nsExec::ExecToLog '"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$PLUGINSDIR\uninstall.ps1" --keep-data'
  Pop $0
  ${If} ${RunningX64}
    ${EnableX64FSRedirection}
  ${EndIf}
  Delete "$DESKTOP\Arcade V1.0.lnk"
  Delete "$INSTDIR\Uninstall Arcade V1.0.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${UNINST_KEY}"
  IfSilent +2
    MessageBox MB_ICONINFORMATION "Arcade V1.0 is removed. Your wallet, its 12 words and your chains are kept in $LOCALAPPDATA\arcade (or the folder you chose): installing again picks them up. Delete that folder yourself only once your 12 words are written down."
SectionEnd
