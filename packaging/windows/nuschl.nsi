; SPDX-License-Identifier: EUPL-1.2
; Per-user Windows installer: the files of the portable zip in
; %LOCALAPPDATA%\Programs\Nuschl, a Start menu entry, an uninstall entry and
; the mumble:// and nuschl:// handlers, all under HKCU. The in-place updater
; (crates/update) keeps updating the installed folder.
;
;   makensis -WX -DVERSION=x.y.z -DPAYLOAD=dir -DICON=nuschl.ico -DOUTFILE=setup.exe nuschl.nsi
;
; Silent: setup.exe /S [/D=folder] and Uninstall.exe /S (keeps settings).
Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!include LogicLib.nsh
!include MUI2.nsh

!define UNINSTALL "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nuschl"
!define RUN "Software\Microsoft\Windows\CurrentVersion\Run"
!define OPEN '"$INSTDIR\nuschl.exe" "%1"'

Name "Nuschl"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Nuschl"
InstallDirRegKey HKCU "${UNINSTALL}" "InstallLocation"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\nuschl.exe"
!insertmacro MUI_PAGE_FINISH
!define MUI_COMPONENTSPAGE_NODESC
!define MUI_COMPONENTSPAGE_TEXT_TOP "Removes Nuschl from $INSTDIR."
!define MUI_COMPONENTSPAGE_TEXT_COMPLIST "Options:"
!insertmacro MUI_UNPAGE_COMPONENTS
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

!macro Scheme SCHEME DESCRIPTION
  WriteRegStr HKCU "Software\Classes\${SCHEME}" "" "URL:${DESCRIPTION}"
  WriteRegStr HKCU "Software\Classes\${SCHEME}" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\${SCHEME}\DefaultIcon" "" "$INSTDIR\nuschl.exe,0"
  WriteRegStr HKCU "Software\Classes\${SCHEME}\shell\open\command" "" '${OPEN}'
!macroend

; Removes a handler only while it still opens this installation.
!macro RemoveScheme SCHEME
  ReadRegStr $0 HKCU "Software\Classes\${SCHEME}\shell\open\command" ""
  ${If} $0 == '${OPEN}'
    DeleteRegKey HKCU "Software\Classes\${SCHEME}"
  ${EndIf}
!macroend

Section
  SetOutPath "$INSTDIR"
  File /r "${PAYLOAD}/*"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Nuschl.lnk" "$INSTDIR\nuschl.exe"
  !insertmacro Scheme mumble Mumble
  !insertmacro Scheme nuschl Nuschl
  WriteRegStr HKCU "${UNINSTALL}" "DisplayName" "Nuschl"
  WriteRegStr HKCU "${UNINSTALL}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL}" "Publisher" "Nuschl"
  WriteRegStr HKCU "${UNINSTALL}" "DisplayIcon" "$INSTDIR\nuschl.exe"
  WriteRegStr HKCU "${UNINSTALL}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL}" "NoRepair" 1
SectionEnd

; Deletes the release's files by name, so a folder chosen with /D keeps
; everything else in it.
Section "-un.Program"
  Delete "$INSTDIR\nuschl.exe"
  ${If} ${FileExists} "$INSTDIR\nuschl.exe"
    MessageBox MB_OK|MB_ICONSTOP "Quit Nuschl, then uninstall again." /SD IDOK
    Abort
  ${EndIf}
  Delete "$INSTDIR\nuschl-*.exe"
  RMDir /r "$INSTDIR\licenses"
  Delete "$INSTDIR\share\nuschl\commit"
  RMDir "$INSTDIR\share\nuschl"
  RMDir "$INSTDIR\share"
  RMDir /r "$INSTDIR\.nuschl-update"
  RMDir /r "$INSTDIR\.nuschl-old"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Nuschl.lnk"
  !insertmacro RemoveScheme mumble
  !insertmacro RemoveScheme nuschl
  ReadRegStr $0 HKCU "${RUN}" "Nuschl"
  ${If} $0 == '"$INSTDIR\nuschl.exe" --autostart'
    DeleteRegValue HKCU "${RUN}" "Nuschl"
  ${EndIf}
  DeleteRegKey HKCU "${UNINSTALL}"
SectionEnd

Section /o "un.Remove settings and history"
  RMDir /r "$APPDATA\Nuschl"
  RMDir /r "$LOCALAPPDATA\Nuschl"
SectionEnd
