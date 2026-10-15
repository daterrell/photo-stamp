; Photo Stamp setup: installs photo-stamp for this Windows user (no admin rights needed), asks
; where photos will be dropped, starts it at login and opens the dashboard.
;
; Built on Linux by scripts/build-windows.sh:
;   makensis -WX -DVERSION=0.7.1 -DVIVERSION=0.7.1.0 -DDIST=dist/photo-stamp-windows \
;            -DOUTFILE=dist/photo-stamp-setup.exe installer/photo-stamp.nsi
;
; Silent install (tests): photo-stamp-setup.exe /S [/DROP=<folder>] [/INSTANCE=<name>] [/D=<install dir>]
; /INSTANCE keeps a test install apart from a real one (its own Settings > Apps entry, start-at-login
; entry and shortcut); /D must come last.
;
; Setup only copies files and records the uninstaller; `photo-stamp.exe --configure` does the rest
; (app.json, start at login, notifications), and `--quit` stops a running copy before files are
; replaced. Uninstalling keeps the photos, the drop folder and photo-stamp's data folder.

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !define VERSION "0.0.0-dev"
!endif
!ifndef VIVERSION
  !define VIVERSION "0.0.0.0"
!endif
!ifndef DIST
  !define DIST "../dist/photo-stamp-windows"
!endif
!ifndef OUTFILE
  !define OUTFILE "../dist/photo-stamp-setup.exe"
!endif

!define APP "Photo Stamp"
!define EXE "photo-stamp.exe"
!define UNINSTALL_ROOT "Software\Microsoft\Windows\CurrentVersion\Uninstall"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "${APP}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\${APP}"
SetCompressor /SOLID lzma
BrandingText "${APP} ${VERSION}"
ShowInstDetails nevershow
ShowUninstDetails nevershow

VIProductVersion "${VIVERSION}"
VIFileVersion "${VIVERSION}"

Var DropDir
Var Instance
; "PhotoStamp" or "PhotoStamp-<instance>": the Settings > Apps entry.
Var UKey
; "photo-stamp" or "photo-stamp-<instance>": the start-at-login entry (see AppConfig::startup_name).
Var RunName
; "Photo Stamp" or "Photo Stamp (<instance>)": the Start menu shortcut.
Var Shortcut

!define MUI_ICON "../assets/photo-stamp.ico"
!define MUI_UNICON "../assets/photo-stamp.ico"
!define MUI_ABORTWARNING

; Page 1: the drop folder.
!define MUI_PAGE_HEADER_TEXT "Where will you drop photos?"
!define MUI_PAGE_HEADER_SUBTEXT "Photo Stamp watches one folder on this PC."
!define MUI_DIRECTORYPAGE_TEXT_TOP "Put photos in this folder (subfolders are fine) and Photo Stamp writes where and when each was taken into the picture. Stamped photos go to the $\"outbox$\" folder inside it; the originals are kept in its $\"originals$\" folder.$\r$\n$\r$\nClick Install to set it up."
!define MUI_DIRECTORYPAGE_TEXT_DESTINATION "Drop folder"
!define MUI_DIRECTORYPAGE_VARIABLE $DropDir
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE CheckDropDir
!insertmacro MUI_PAGE_DIRECTORY

!insertmacro MUI_PAGE_INSTFILES

; Last page: open the dashboard.
!define MUI_FINISHPAGE_TITLE "Photo Stamp is ready"
!define MUI_FINISHPAGE_TEXT "Photo Stamp is running: its icon is by the clock (it may be under the ^ arrow). It starts by itself whenever you log in.$\r$\n$\r$\nDrop photos into:$\r$\n$DropDir"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Open Photo Stamp now"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "FileDescription" "${APP} setup"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT license"

; The names that depend on /INSTANCE.
!macro SetNames
  ${If} $Instance == ""
    StrCpy $UKey "${UNINSTALL_ROOT}\PhotoStamp"
    StrCpy $RunName "photo-stamp"
    StrCpy $Shortcut "${APP}"
  ${Else}
    StrCpy $UKey "${UNINSTALL_ROOT}\PhotoStamp-$Instance"
    StrCpy $RunName "photo-stamp-$Instance"
    StrCpy $Shortcut "${APP} ($Instance)"
  ${EndIf}
!macroend

Function .onInit
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/INSTANCE=" $Instance
  ${If} ${Errors}
    StrCpy $Instance ""
  ${EndIf}
  !insertmacro SetNames
  ClearErrors
  ${GetOptions} $0 "/DROP=" $DropDir
  ${If} ${Errors}
  ${OrIf} $DropDir == ""
    ; Installing again: offer the folder chosen last time.
    ReadRegStr $DropDir HKCU "$UKey" "DropFolder"
  ${EndIf}
  ${If} $DropDir == ""
    StrCpy $DropDir "$PICTURES\${APP}"
  ${EndIf}
FunctionEnd

; Not a drive's top folder, the program's folder or photo-stamp's data folder.
Function CheckDropDir
  StrLen $0 $DropDir
  ${If} $0 <= 3
    MessageBox MB_ICONEXCLAMATION|MB_OK "Please choose a folder, not a whole drive."
    Abort
  ${EndIf}
  StrLen $1 "$INSTDIR"
  StrCpy $2 $DropDir $1
  ${If} $2 == "$INSTDIR"
    MessageBox MB_ICONEXCLAMATION|MB_OK "Please choose a folder outside Photo Stamp's program folder."
    Abort
  ${EndIf}
  StrLen $1 "$LOCALAPPDATA\photo-stamp"
  StrCpy $2 $DropDir $1
  ${If} $2 == "$LOCALAPPDATA\photo-stamp"
    MessageBox MB_ICONEXCLAMATION|MB_OK "Please choose a folder outside Photo Stamp's data folder."
    Abort
  ${EndIf}
FunctionEnd

Section "Install"
  ; A copy that's running (installing again, or a newer version) finishes its photos and quits.
  ${If} ${FileExists} "$INSTDIR\${EXE}"
    DetailPrint "Stopping Photo Stamp…"
    nsExec::Exec '"$INSTDIR\${EXE}" --quit'
    Pop $0
  ${EndIf}

  SetOutPath "$INSTDIR"
  File /r "${DIST}/*"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$DropDir"
  ${If} $Instance == ""
    nsExec::ExecToStack '"$INSTDIR\${EXE}" --configure "$DropDir"'
  ${Else}
    nsExec::ExecToStack '"$INSTDIR\${EXE}" --configure "$DropDir" --instance "$Instance"'
  ${EndIf}
  Pop $0
  Pop $1
  ${If} $0 != "0"
    MessageBox MB_ICONSTOP|MB_OK "Photo Stamp couldn't be set up ($0).$\r$\n$1" /SD IDOK
    Abort
  ${EndIf}

  ; Settings > Apps.
  WriteRegStr HKCU "$UKey" "DisplayName" "$Shortcut"
  WriteRegStr HKCU "$UKey" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "$UKey" "Publisher" "photo-stamp"
  WriteRegStr HKCU "$UKey" "DisplayIcon" "$INSTDIR\${EXE},0"
  WriteRegStr HKCU "$UKey" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "$UKey" "DropFolder" "$DropDir"
  WriteRegStr HKCU "$UKey" "Instance" "$Instance"
  ${If} $Instance == ""
    WriteRegStr HKCU "$UKey" "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKCU "$UKey" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  ${Else}
    WriteRegStr HKCU "$UKey" "UninstallString" '"$INSTDIR\uninstall.exe" /INSTANCE=$Instance'
    WriteRegStr HKCU "$UKey" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S /INSTANCE=$Instance'
  ${EndIf}
  WriteRegDWORD HKCU "$UKey" "NoModify" 1
  WriteRegDWORD HKCU "$UKey" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKCU "$UKey" "EstimatedSize" "$0"

  CreateShortcut "$SMPROGRAMS\$Shortcut.lnk" "$INSTDIR\${EXE}" "" "$INSTDIR\${EXE}" 0

  ; Start it now, quietly; the last page (or a click on the Start menu shortcut) opens the dashboard.
  Exec '"$INSTDIR\${EXE}" --background'
SectionEnd

Function un.onInit
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/INSTANCE=" $Instance
  ${If} ${Errors}
    StrCpy $Instance ""
  ${EndIf}
  !insertmacro SetNames
FunctionEnd

Section "Uninstall"
  ${If} ${FileExists} "$INSTDIR\${EXE}"
    nsExec::Exec '"$INSTDIR\${EXE}" --quit'
    Pop $0
  ${EndIf}
  DeleteRegValue HKCU "${RUN_KEY}" "$RunName"
  DeleteRegKey HKCU "$UKey"
  Delete "$SMPROGRAMS\$Shortcut.lnk"
  ; Only ever remove a folder that really is Photo Stamp's (never, say, a whole drive).
  ${If} ${FileExists} "$INSTDIR\${EXE}"
    RMDir /r "$INSTDIR"
  ${EndIf}
  ; Photos, the drop folder and photo-stamp's settings and history (in %LOCALAPPDATA%\photo-stamp)
  ; are kept: installing again carries on where it left off.
  DetailPrint "Your photos and the drop folder were kept."
SectionEnd
