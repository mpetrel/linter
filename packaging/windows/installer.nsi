; NSIS script for the linter Windows installer.
; Built by packaging/windows/build-installer.sh, which defines:
;   VERSION  the version from Cargo.toml (e.g. 0.1.0)
;   STAGE    a folder holding linter.exe, README.md and examples\
;   OUTFILE  the installer to write
;   ICON     the .ico file for the installer and uninstaller

Unicode True
; No copyright holder is named in version info.
!pragma warning disable 9100
SetCompressor /SOLID lzma

!macro Require name
  !ifndef ${name}
    !error "${name} isn't defined: run packaging/windows/build-installer.sh"
  !endif
!macroend
!insertmacro Require VERSION
!insertmacro Require STAGE
!insertmacro Require OUTFILE
!insertmacro Require ICON

!define APP "linter"
!define EXE "linter.exe"
!define PROGID "linter.md"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP}"
!define APPPATH_KEY "Software\Microsoft\Windows\CurrentVersion\App Paths\${EXE}"

!include "MUI2.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

Name "${APP} ${VERSION}"
OutFile "${OUTFILE}"
; An earlier install's folder is read in .onInit.
InstallDir "$PROGRAMFILES64\${APP}"
RequestExecutionLevel admin
ManifestDPIAware true
BrandingText "${APP} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APP} Markdown editor setup"
VIAddVersionKey "FileVersion" "${VERSION}"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_SMALLDESC

!define MUI_WELCOMEPAGE_TEXT "This will install ${APP} ${VERSION}, a Markdown editor that shows your document rendered and turns a block back into Markdown when you click it.$\r$\n$\r$\nClick Next to continue."
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP}"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApp
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "${APP} needs 64-bit Windows 10 or later."
    Abort
  ${EndIf}
  SetRegView 64
  ; Update an earlier install in its own folder. InstallDirRegKey can't find
  ; it: SetRegView doesn't apply to it, so this 32-bit installer would look in
  ; the 32-bit registry view. A folder given with /D is kept.
  ReadRegStr $0 HKLM "Software\${APP}" "InstallDir"
  ${If} $0 != ""
  ${AndIf} $INSTDIR == "$PROGRAMFILES64\${APP}"
    StrCpy $INSTDIR $0
  ${EndIf}
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

; A running linter.exe can't be replaced or deleted, and it can't be opened
; for writing either, so ask until it's closed. A silent run gives up.
!macro WaitUntilClosed
  ${Do}
    ${IfNot} ${FileExists} "$INSTDIR\${EXE}"
      ${Break}
    ${EndIf}
    ClearErrors
    FileOpen $0 "$INSTDIR\${EXE}" a
    ${IfNot} ${Errors}
      FileClose $0
      ${Break}
    ${EndIf}
    ${If} ${Cmd} `MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${APP} is running. Close it, then click Retry." /SD IDCANCEL IDCANCEL`
      Abort
    ${EndIf}
  ${Loop}
!macroend

; The installer runs elevated. Starting the app through Explorer runs it as
; the signed-in user instead, so files it saves aren't owned by the
; administrator and dragging files onto its window works.
Function LaunchApp
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${EXE}"'
FunctionEnd

Section "${APP} (required)" SecApp
  SectionIn RO
  !insertmacro WaitUntilClosed
  SetOutPath "$INSTDIR"
  File "${STAGE}/${EXE}"
  File "${STAGE}/README.md"
  SetOutPath "$INSTDIR\examples"
  File "${STAGE}/examples/sample.md"
  File "${STAGE}/examples/sample.png"
  SetOutPath "$INSTDIR"

  CreateShortcut "$SMPROGRAMS\${APP}.lnk" "$INSTDIR\${EXE}" "" "$INSTDIR\${EXE}" 0 SW_SHOWNORMAL "" "Markdown editor"

  ; Lets "linter" be started from the Run dialog.
  WriteRegStr HKLM "${APPPATH_KEY}" "" "$INSTDIR\${EXE}"

  WriteRegStr HKLM "Software\${APP}" "InstallDir" "$INSTDIR"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${EXE},0"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\${EXE}" "" "$INSTDIR\${EXE}" 0 SW_SHOWNORMAL "" "Markdown editor"
SectionEnd

!macro OpenWith ext
  WriteRegStr HKLM "Software\Classes\${ext}\OpenWithProgids" "${PROGID}" ""
  WriteRegStr HKLM "Software\Classes\Applications\${EXE}\SupportedTypes" "${ext}" ""
!macroend

; Adds linter to "Open with" for Markdown files. It doesn't take over the
; default program; choose that in Windows' "Open with" dialog.
Section "Add to $\"Open with$\" for Markdown files" SecOpenWith
  WriteRegStr HKLM "Software\Classes\${PROGID}" "" "Markdown document"
  WriteRegStr HKLM "Software\Classes\${PROGID}\DefaultIcon" "" "$INSTDIR\${EXE},0"
  WriteRegStr HKLM "Software\Classes\${PROGID}\shell\open\command" "" '"$INSTDIR\${EXE}" "%1"'
  WriteRegStr HKLM "Software\Classes\Applications\${EXE}" "FriendlyAppName" "${APP}"
  WriteRegStr HKLM "Software\Classes\Applications\${EXE}\shell\open\command" "" '"$INSTDIR\${EXE}" "%1"'
  !insertmacro OpenWith ".md"
  !insertmacro OpenWith ".markdown"
  !insertmacro OpenWith ".mdown"
  !insertmacro OpenWith ".mkd"
  !insertmacro OpenWith ".mkdn"
  ; SHCNE_ASSOCCHANGED: refresh Explorer's file type information.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} "The editor, a Start menu shortcut and a sample document."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "A shortcut on the desktop."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecOpenWith} "List ${APP} when you right-click a .md file and choose Open with. Your default program for .md files doesn't change."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

!macro un.OpenWith ext
  DeleteRegValue HKLM "Software\Classes\${ext}\OpenWithProgids" "${PROGID}"
  DeleteRegKey /ifempty HKLM "Software\Classes\${ext}\OpenWithProgids"
  DeleteRegKey /ifempty HKLM "Software\Classes\${ext}"
!macroend

Section "Uninstall"
  !insertmacro WaitUntilClosed

  ; The settings are in the user's profile and are kept unless asked, so
  ; installing again brings them back. Only the signed-in user's are found.
  ; The folder is removed only if nothing else is left in it.
  ${If} ${FileExists} "$APPDATA\${APP}\settings"
    ${If} ${Cmd} `MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also delete your ${APP} settings?$\r$\n$\r$\nIf you keep them, installing ${APP} again brings them back." /SD IDNO IDYES`
      Delete "$APPDATA\${APP}\settings"
      Delete "$APPDATA\${APP}\.settings.*.linter-tmp"
      RMDir "$APPDATA\${APP}"
    ${EndIf}
  ${EndIf}

  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\examples\sample.md"
  Delete "$INSTDIR\examples\sample.png"
  RMDir "$INSTDIR\examples"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP}.lnk"
  Delete "$DESKTOP\${APP}.lnk"

  DeleteRegKey HKLM "Software\Classes\${PROGID}"
  DeleteRegKey HKLM "Software\Classes\Applications\${EXE}"
  !insertmacro un.OpenWith ".md"
  !insertmacro un.OpenWith ".markdown"
  !insertmacro un.OpenWith ".mdown"
  !insertmacro un.OpenWith ".mkd"
  !insertmacro un.OpenWith ".mkdn"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'

  DeleteRegKey HKLM "${APPPATH_KEY}"
  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "Software\${APP}"
SectionEnd
