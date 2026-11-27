; DELTA per-user installer (NSIS 3.13, M3 S-25/S-26/S-29/S-34). Compiled by
; `delta-cli package-build`, which passes every identity value with -D and the
; generated file list (DELTA_FILES_NSH).
;
; Command line: /S installs silently; /RELAUNCH starts DELTA again once the
; installation is done (/DELTAARGS=<hex> restores its previous arguments with
; --restore-args); /LOG=<file> appends what Setup did to <file>.
; The folder is always %LOCALAPPDATA%\Programs\<identity> (a /D= is ignored).
; The uninstall entry is HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\<app id>.
; Application data lives outside the install folder and is never removed here.

!if "${NSIS_VERSION}" != "v3.13"
  !error "DELTA installers require NSIS 3.13, not ${NSIS_VERSION} (S-25)"
!endif

!macro DELTA_REQUIRE NAME
  !ifndef ${NAME}
    !error "${NAME} must be defined by delta-cli package-build"
  !endif
!macroend
!insertmacro DELTA_REQUIRE DELTA_IDENTITY          ; delta, delta-dev, delta-test, delta-dev-test
!insertmacro DELTA_REQUIRE DELTA_APP_ID            ; io.github.chialecode.<identity>
!insertmacro DELTA_REQUIRE DELTA_DISPLAY_NAME      ; DELTA, DELTA Dev, ...
!insertmacro DELTA_REQUIRE DELTA_VERSION           ; DisplayVersion
!insertmacro DELTA_REQUIRE DELTA_NUMERIC_VERSION   ; four numbers, e.g. 0.1.1.0
!insertmacro DELTA_REQUIRE DELTA_DESKTOP_EXE
!insertmacro DELTA_REQUIRE DELTA_CLI_EXE
!insertmacro DELTA_REQUIRE DELTA_MUTEX             ; held by the running desktop app
!insertmacro DELTA_REQUIRE DELTA_AUMID             ; the app's AppUserModelID, set on the shortcuts
!insertmacro DELTA_REQUIRE DELTA_DESKTOP_ON_UPDATE ; 1: silent over-installs make the desktop shortcut again
!insertmacro DELTA_REQUIRE DELTA_SOURCE_DIR        ; staged files
!insertmacro DELTA_REQUIRE DELTA_ICON
!insertmacro DELTA_REQUIRE DELTA_OUT_FILE
!insertmacro DELTA_REQUIRE DELTA_FILES_NSH         ; generated: DELTA_REMOVE_INSTALLED_FILES

!define DELTA_UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${DELTA_APP_ID}"
!define DELTA_SETUP_MUTEX "${DELTA_IDENTITY}-setup"
!define DELTA_START_MENU_LINK "$SMPROGRAMS\${DELTA_DISPLAY_NAME}.lnk"
!define DELTA_DESKTOP_LINK "$DESKTOP\${DELTA_DISPLAY_NAME}.lnk"
; 175 x 200 ms = 35 s for the app to exit after the quit-for-update request.
!define DELTA_QUIT_WAIT_TICKS 175

; Exit codes besides NSIS's own (0 done, 1 cancelled, 2 aborted).
!define DELTA_EXIT_BUSY 3              ; DELTA answered busy or could not save, and keeps running
!define DELTA_EXIT_QUIT_TIMEOUT 4      ; DELTA did not exit in time
!define DELTA_EXIT_QUIT_FAILED 5       ; DELTA rejected the quit request and keeps running
!define DELTA_EXIT_SETUP_RUNNING 6     ; another Setup of this identity is running
!define DELTA_EXIT_FOLDER_TAKEN 7      ; the folder holds another program's Uninstall.exe
!define DELTA_EXIT_FILES_NOT_WRITTEN 8 ; a file could not be written (held open elsewhere)
!define DELTA_EXIT_MIGRATION_FAILED 9  ; the M2 portable installation could not be migrated

Unicode true
ManifestDPIAware true
; zlib: its module is under the zlib/libpng licence like the rest of the stub
; (NSIS's LZMA module is CPL-1.0). Not solid, so the data block optimizer
; stores the migration helper (the same bytes as the CLI) only once.
SetCompressor zlib
RequestExecutionLevel user
Name "${DELTA_DISPLAY_NAME}"
OutFile "${DELTA_OUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\${DELTA_IDENTITY}"
BrandingText " "
ShowInstDetails nevershow
ShowUninstDetails nevershow

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "WordFunc.nsh"
!include "x64.nsh"
!include "Sections.nsh"

!define MUI_ICON "${DELTA_ICON}"
!define MUI_UNICON "${DELTA_ICON}"
!define MUI_COMPONENTSPAGE_NODESC
!define MUI_FINISHPAGE_RUN "$INSTDIR\${DELTA_DESKTOP_EXE}"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

VIProductVersion "${DELTA_NUMERIC_VERSION}"
VIFileVersion "${DELTA_NUMERIC_VERSION}"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "ProductName" "${DELTA_DISPLAY_NAME}"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "ProductVersion" "${DELTA_VERSION}"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "FileVersion" "${DELTA_NUMERIC_VERSION}"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "FileDescription" "${DELTA_DISPLAY_NAME} Setup"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "CompanyName" "DELTA Contributors"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "LegalCopyright" "Copyright (C) DELTA Contributors. Apache-2.0."

; Sets $8 to the first of this version's files that is already in $INSTDIR
; and cannot be opened for writing (held open by a running delta CLI, an
; editor or a scanner), unless $8 is set already.
!macro DELTA_CHECK_WRITABLE RELATIVE
  ${If} $8 == ""
  ${AndIf} ${FileExists} "$INSTDIR\${RELATIVE}"
    ClearErrors
    FileOpen $9 "$INSTDIR\${RELATIVE}" a
    ${If} ${Errors}
      StrCpy $8 "${RELATIVE}"
    ${Else}
      FileClose $9
    ${EndIf}
  ${EndIf}
!macroend

; DELTA_REMOVE_INSTALLED_FILES: deletes exactly the files this version
; installs, then the folders it made when they are empty.
; DELTA_CHECK_FILES_WRITABLE: DELTA_CHECK_WRITABLE for each of those files.
!include "${DELTA_FILES_NSH}"

Var LogFile
; No uninstall entry of this identity when Setup started: nothing is installed.
Var FirstInstall

; Appends the line on the stack to the /LOG= file, if one was given.
!macro DELTA_LOG_FUNCTION UN
Function ${UN}DeltaLog
  Exch $0
  Push $1
  ${If} $LogFile != ""
    ClearErrors
    FileOpen $1 "$LogFile" a
    ${IfNot} ${Errors}
      FileSeek $1 0 END
      FileWrite $1 "DELTA: $0$\r$\n"
      FileClose $1
    ${EndIf}
  ${EndIf}
  Pop $1
  Pop $0
FunctionEnd
!macroend
!insertmacro DELTA_LOG_FUNCTION ""
!insertmacro DELTA_LOG_FUNCTION "un."

!macro DELTA_LOG TEXT
  Push "${TEXT}"
  !ifdef __UNINSTALL__
    Call un.DeltaLog
  !else
    Call DeltaLog
  !endif
!macroend

; Reads /LOG=<file> into $LogFile. Rust quotes an argument with a space as a
; whole ("/LOG=C:\a b\setup.log"), which GetOptions does not take; quoting the
; value instead (/LOG="C:\a b\setup.log") it does.
!macro DELTA_READ_LOG_OPTION_FUNCTION UN
Function ${UN}DeltaReadLogOption
  Push $0
  ${GetParameters} $0
  ${WordReplace} $0 '"/LOG=' '/LOG="' "+" $0
  ClearErrors
  ${GetOptions} $0 "/LOG=" $LogFile
  ${If} ${Errors}
    StrCpy $LogFile ""
  ${EndIf}
  Pop $0
FunctionEnd
!macroend
!insertmacro DELTA_READ_LOG_OPTION_FUNCTION ""
!insertmacro DELTA_READ_LOG_OPTION_FUNCTION "un."

; Leaves "1" on the stack when DELTA of this identity runs (its single-instance
; mutex exists, or exists and cannot be opened), else "0".
!macro DELTA_IS_RUNNING_FUNCTION UN
Function ${UN}DeltaIsRunning
  Push $0
  Push $1
  System::Call 'kernel32::OpenMutexW(i 0x00100000, i 0, w "${DELTA_MUTEX}") p.r0 ?e'
  Pop $1
  ${If} $0 <> 0
    System::Call 'kernel32::CloseHandle(p r0)'
    StrCpy $1 "1"
  ${ElseIf} $1 = 5
    StrCpy $1 "1"
  ${Else}
    StrCpy $1 "0"
  ${EndIf}
  Exch $1
  Exch
  Pop $0
FunctionEnd
!macroend
!insertmacro DELTA_IS_RUNNING_FUNCTION ""
!insertmacro DELTA_IS_RUNNING_FUNCTION "un."

; S-29: never force-close. Asks a running DELTA (through the installed CLI and
; the control pipe) to save and exit, and waits for it; when it is busy or does
; not exit, Setup quits with one of the DELTA_EXIT_* codes and nothing changed.
!macro DELTA_CLOSE_APP_FUNCTION UN
Function ${UN}DeltaCloseApp
  Push $0
  Push $1
  Push $2
  Call ${UN}DeltaIsRunning
  Pop $0
  ${If} $0 == "1"
    ; /RELAUNCH comes only from the in-app updater, which started this setup and
    ; is exiting by itself: asking it to quit would find it busy exiting.
    ${GetParameters} $1
    ClearErrors
    ${GetOptions} $1 "/RELAUNCH" $2
    ${IfNot} ${Errors}
      !insertmacro DELTA_LOG "${DELTA_IDENTITY} is exiting after starting this update; waiting for it"
      Goto delta_wait
    ${EndIf}
    !insertmacro DELTA_LOG "${DELTA_IDENTITY} is running; requesting quit-for-update"
    MessageBox MB_OKCANCEL|MB_ICONEXCLAMATION "${DELTA_DISPLAY_NAME} 正在运行。点“确定”让它保存后退出并继续，点“取消”退出。" /SD IDOK IDOK delta_quit
    !insertmacro DELTA_LOG "cancelled while ${DELTA_IDENTITY} runs; installation aborted, nothing changed"
    SetErrorLevel 1
    Quit
    delta_quit:
    ; The running copy is the installed one.
    ReadRegStr $1 HKCU "${DELTA_UNINSTALL_KEY}" "InstallLocation"
    ${If} $1 == ""
      StrCpy $1 "$INSTDIR"
    ${EndIf}
    StrCpy $1 "$1\${DELTA_CLI_EXE}"
    StrCpy $2 "none"
    ${If} ${FileExists} "$1"
      nsExec::Exec '"$1" control quit-for-update --identity ${DELTA_IDENTITY} --timeout-secs 30'
      Pop $2
      !insertmacro DELTA_LOG "quit-for-update exit code $2"
    ${Else}
      !insertmacro DELTA_LOG "no installed control command; waiting for the application to exit"
    ${EndIf}
    ; 3 (busy or save failed), 4 (it already waited 30 s) and 6 (rejected) are
    ; final: do not wait again while the app still holds its mutex.
    ${If} $2 == "3"
    ${OrIf} $2 == "4"
    ${OrIf} $2 == "6"
      Call ${UN}DeltaIsRunning
      Pop $0
      ${If} $0 == "1"
        !insertmacro DELTA_LOG "application is still running (busy, unsaved or not responding); installation aborted, nothing changed"
        MessageBox MB_OK|MB_ICONSTOP "${DELTA_DISPLAY_NAME} 仍在运行或正忙。请保存工作、关闭它后再运行安装程序。" /SD IDOK
        ${If} $2 == "3"
          SetErrorLevel ${DELTA_EXIT_BUSY}
        ${ElseIf} $2 == "4"
          SetErrorLevel ${DELTA_EXIT_QUIT_TIMEOUT}
        ${Else}
          SetErrorLevel ${DELTA_EXIT_QUIT_FAILED}
        ${EndIf}
        Quit
      ${EndIf}
    ${EndIf}
    delta_wait:
    StrCpy $2 0
    ${Do}
      Call ${UN}DeltaIsRunning
      Pop $0
      ${If} $0 != "1"
        ${Break}
      ${EndIf}
      ${If} $2 >= ${DELTA_QUIT_WAIT_TICKS}
        !insertmacro DELTA_LOG "application is still running (busy, unsaved or not responding); installation aborted, nothing changed"
        MessageBox MB_OK|MB_ICONSTOP "${DELTA_DISPLAY_NAME} 仍在运行或正忙。请保存工作、关闭它后再运行安装程序。" /SD IDOK
        SetErrorLevel ${DELTA_EXIT_QUIT_TIMEOUT}
        Quit
      ${EndIf}
      Sleep 200
      IntOp $2 $2 + 1
    ${Loop}
    !insertmacro DELTA_LOG "application saved and exited"
    ; Let Windows release the image files of the exited process.
    Sleep 250
  ${EndIf}
  Pop $2
  Pop $1
  Pop $0
FunctionEnd
!macroend
!insertmacro DELTA_CLOSE_APP_FUNCTION ""
!insertmacro DELTA_CLOSE_APP_FUNCTION "un."

; Sets System.AppUserModel.ID on the shortcut on the stack to DELTA_AUMID,
; the ID the app sets for itself, so the taskbar keeps both together.
Function DeltaSetShortcutAumid
  Exch $0
  Push $1
  Push $2
  Push $3
  Push $4
  Push $5
  Push $6
  ; CLSID_ShellLink, IID_IShellLinkW
  System::Call 'ole32::CoCreateInstance(g "{00021401-0000-0000-C000-000000000046}", p 0, i 1, g "{000214F9-0000-0000-C000-000000000046}", *p .r1) i .r5'
  ${If} $5 = 0
    ; IID_IPersistFile; Load(path, STGM_READWRITE)
    System::Call '$1->0(g "{0000010b-0000-0000-C000-000000000046}", *p .r2) i .r5'
    ${If} $5 = 0
      System::Call '$2->5(w r0, i 2) i .r5'
      ${If} $5 = 0
        ; IID_IPropertyStore
        System::Call '$1->0(g "{886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99}", *p .r3) i .r5'
        ${If} $5 = 0
          ; PKEY_AppUserModel_ID and a VT_LPWSTR PROPVARIANT
          System::Call '*(&g16 "{9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}", &i4 5) p .r4'
          System::Call '*(&i2 31, &i2 0, &i2 0, &i2 0, w "${DELTA_AUMID}", i 0, i 0) p .r6'
          ; SetValue, Commit, then IPersistFile::Save(NULL, TRUE)
          System::Call '$3->6(p r4, p r6) i .r5'
          ${If} $5 = 0
            System::Call '$3->7() i .r5'
          ${EndIf}
          ${If} $5 = 0
            System::Call '$2->6(p 0, i 1) i .r5'
          ${EndIf}
          System::Free $4
          System::Free $6
          System::Call '$3->2()'
        ${EndIf}
      ${EndIf}
      System::Call '$2->2()'
    ${EndIf}
    System::Call '$1->2()'
  ${EndIf}
  ${If} $5 <> 0
    IntFmt $5 "0x%08X" $5
    !insertmacro DELTA_LOG "could not set the AppUserModelID of $0 ($5)"
  ${EndIf}
  Pop $6
  Pop $5
  Pop $4
  Pop $3
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

; Leaves "1" on the stack when $INSTDIR holds an Uninstall.exe that is not
; this identity's installed copy, else "0". Such an uninstaller is never run
; or overwritten.
Function DeltaFolderTaken
  Push $0
  Push $1
  StrCpy $1 "0"
  ${If} ${FileExists} "$INSTDIR\Uninstall.exe"
    ReadRegStr $0 HKCU "${DELTA_UNINSTALL_KEY}" "InstallLocation"
    ${If} $0 != $INSTDIR
      StrCpy $1 "1"
      ; Our own copy whose uninstall entry is gone (an interrupted install, a
      ; registry cleaner) is updated in place instead.
      ${If} $0 == ""
      ${AndIf} ${FileExists} "$INSTDIR\${DELTA_DESKTOP_EXE}"
        StrCpy $1 "0"
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Exch $1
  Exch
  Pop $0
FunctionEnd

Section "-Install"
  Call DeltaFolderTaken
  Pop $0
  ${If} $0 == "1"
    !insertmacro DELTA_LOG "$INSTDIR holds another program's Uninstall.exe; installation aborted, nothing changed"
    MessageBox MB_OK|MB_ICONSTOP "$INSTDIR 里已装有别的程序，没有安装 ${DELTA_DISPLAY_NAME}。" /SD IDOK
    SetErrorLevel ${DELTA_EXIT_FOLDER_TAKEN}
    Quit
  ${EndIf}
  Call DeltaCloseApp

  ; A file of this version held open by another program (a running delta CLI)
  ; would make the old uninstaller and File skip it and leave a mix of
  ; versions: stop before anything changes.
  StrCpy $8 ""
  !insertmacro DELTA_CHECK_FILES_WRITABLE
  ${If} $8 != ""
    !insertmacro DELTA_LOG "$INSTDIR\$8 is held open by another program; installation aborted, nothing changed"
    MessageBox MB_OK|MB_ICONSTOP "$INSTDIR\$8 正被别的程序占用，没有做任何改动。请关闭占用它的程序后重新运行安装程序。" /SD IDOK
    SetErrorLevel ${DELTA_EXIT_FILES_NOT_WRITTEN}
    Quit
  ${EndIf}

  ; S-34: remove an M2 portable installation's verified files before copying;
  ; modified and unknown files are kept and listed in the log by the helper.
  ${If} ${FileExists} "$INSTDIR\install-manifest.json"
    !insertmacro DELTA_LOG "M2 portable installation found; migrating"
    InitPluginsDir
    File "/oname=$PLUGINSDIR\delta-migrate-helper.exe" "${DELTA_SOURCE_DIR}\${DELTA_CLI_EXE}"
    nsExec::Exec '"$PLUGINSDIR\delta-migrate-helper.exe" __migrate-portable --identity ${DELTA_IDENTITY} --install-dir "$INSTDIR" --log "$LogFile"'
    Pop $0
    ${If} $0 != "0"
      !insertmacro DELTA_LOG "migration exit code $0; installation aborted, nothing changed"
      MessageBox MB_OK|MB_ICONSTOP "没能迁移之前的便携版，没有做任何改动。" /SD IDOK
      SetErrorLevel ${DELTA_EXIT_MIGRATION_FAILED}
      Quit
    ${EndIf}
  ${EndIf}

  ReadRegStr $0 HKCU "${DELTA_UNINSTALL_KEY}" "InstallLocation"
  ${If} $0 == ""
  ${OrIf} $0 == $INSTDIR
    ; Installed here (or our own copy here lost its entry; the folder check refused anything else).
    ${If} ${FileExists} "$INSTDIR\Uninstall.exe"
      ; The installed version's uninstaller removes exactly its own files
      ; (/UPDATE keeps the shortcuts and the uninstall entry), so files this
      ; version no longer ships do not linger. _?= runs it in place and waits.
      ClearErrors
      ExecWait '"$INSTDIR\Uninstall.exe" /S /UPDATE _?=$INSTDIR'
      ${If} ${Errors}
        !insertmacro DELTA_LOG "the previous version's uninstaller did not run; its own files may remain"
      ${EndIf}
    ${EndIf}
  ${ElseIf} ${FileExists} "$0\Uninstall.exe"
    ; Installed in another folder (LOCALAPPDATA moved): uninstall that copy
    ; completely first, so one copy and one uninstall entry remain.
    ClearErrors
    ExecWait '"$0\Uninstall.exe" /S _?=$0' $1
    ${If} ${Errors}
    ${OrIf} $1 <> 0
      ; Keep its uninstaller: the old copy is still there and can be removed later.
      !insertmacro DELTA_LOG "could not uninstall the copy in $0"
    ${Else}
      Delete "$0\Uninstall.exe"
      RMDir "$0"
    ${EndIf}
  ${EndIf}

  SetOutPath "$INSTDIR"
  ; Under /S a file that cannot be written is skipped (NSIS answers its own prompt with Ignore)
  ; and only the error flag says so: stop before the entry claims the new version.
  ClearErrors
  File /r "${DELTA_SOURCE_DIR}\*"
  ${If} ${Errors}
    !insertmacro DELTA_LOG "a file in $INSTDIR could not be written (held open by another program); installation stopped"
    MessageBox MB_OK|MB_ICONSTOP "有文件被别的程序占用，没能写入 $INSTDIR。请关闭占用它的程序后重新运行安装程序。" /SD IDOK
    SetErrorLevel ${DELTA_EXIT_FILES_NOT_WRITTEN}
    Quit
  ${EndIf}
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  WriteRegStr HKCU "${DELTA_UNINSTALL_KEY}" "DisplayName" "${DELTA_DISPLAY_NAME}"
  WriteRegStr HKCU "${DELTA_UNINSTALL_KEY}" "DisplayVersion" "${DELTA_VERSION}"
  WriteRegStr HKCU "${DELTA_UNINSTALL_KEY}" "DisplayIcon" '"$INSTDIR\${DELTA_DESKTOP_EXE}"'
  WriteRegStr HKCU "${DELTA_UNINSTALL_KEY}" "Publisher" "DELTA Contributors"
  WriteRegStr HKCU "${DELTA_UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/chialecode/delta"
  WriteRegStr HKCU "${DELTA_UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${DELTA_UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${DELTA_UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${DELTA_UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${DELTA_UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${DELTA_UNINSTALL_KEY}" "EstimatedSize" "$0"
SectionEnd

; Shortcuts. In the wizard both are optional: ticked on a first install,
; otherwise as the shortcuts are now. Silent installs decide in .onInit: a
; first install makes both; a silent over-install always skips SecStartMenu so
; the Start menu shortcut is never recreated or rewritten (absent stays absent,
; existing stays untouched); for SecDesktop it keeps an existing shortcut and
; makes no new one, except that dev-like identities (DELTA_DESKTOP_ON_UPDATE=1)
; still make the desktop shortcut as a sign of the update.
Section "开始菜单快捷方式" SecStartMenu
  SetOutPath "$INSTDIR"
  CreateShortCut "${DELTA_START_MENU_LINK}" "$INSTDIR\${DELTA_DESKTOP_EXE}"
  Push "${DELTA_START_MENU_LINK}"
  Call DeltaSetShortcutAumid
SectionEnd

Section "桌面快捷方式" SecDesktop
  SetOutPath "$INSTDIR"
  CreateShortCut "${DELTA_DESKTOP_LINK}" "$INSTDIR\${DELTA_DESKTOP_EXE}"
  Push "${DELTA_DESKTOP_LINK}"
  Call DeltaSetShortcutAumid
SectionEnd

Section "-Finish"
  ; A box cleared in the wizard removes that shortcut of an earlier installation.
  ${IfNot} ${Silent}
    ${IfNot} ${SectionIsSelected} ${SecStartMenu}
      Delete "${DELTA_START_MENU_LINK}"
    ${EndIf}
    ${IfNot} ${SectionIsSelected} ${SecDesktop}
      Delete "${DELTA_DESKTOP_LINK}"
    ${EndIf}
  ${EndIf}
  !insertmacro DELTA_LOG "installed ${DELTA_IDENTITY} ${DELTA_VERSION} in $INSTDIR"

  ; After an update DELTA starts again with its previous arguments.
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/RELAUNCH" $1
  ${IfNot} ${Errors}
    ClearErrors
    ${GetOptions} $0 "/DELTAARGS=" $1
    ${If} ${Errors}
    ${OrIf} $1 == ""
      Exec '"$INSTDIR\${DELTA_DESKTOP_EXE}"'
    ${Else}
      Exec '"$INSTDIR\${DELTA_DESKTOP_EXE}" --restore-args $1'
    ${EndIf}
    !insertmacro DELTA_LOG "started ${DELTA_DESKTOP_EXE} again"
  ${EndIf}
SectionEnd

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "${DELTA_DISPLAY_NAME} 只支持 64 位 Windows。" /SD IDOK
    Abort
  ${EndIf}
  ; The folder is fixed: the CLI, install-dev and the gates find it there.
  StrCpy $INSTDIR "$LOCALAPPDATA\Programs\${DELTA_IDENTITY}"
  Call DeltaReadLogOption

  ; One Setup of this identity at a time; the handle stays open until Setup exits.
  System::Call 'kernel32::CreateMutexW(p 0, i 0, w "${DELTA_SETUP_MUTEX}") p .r0 ?e'
  Pop $1
  ${If} $1 = 183
    !insertmacro DELTA_LOG "another Setup of ${DELTA_IDENTITY} is running; installation aborted, nothing changed"
    MessageBox MB_OK|MB_ICONSTOP "${DELTA_DISPLAY_NAME} 的另一个安装程序正在运行。" /SD IDOK
    SetErrorLevel ${DELTA_EXIT_SETUP_RUNNING}
    Quit
  ${EndIf}

  StrCpy $FirstInstall "0"
  ReadRegStr $0 HKCU "${DELTA_UNINSTALL_KEY}" "UninstallString"
  ${If} $0 == ""
    StrCpy $FirstInstall "1"
  ${EndIf}
  !insertmacro DELTA_LOG "first install of ${DELTA_IDENTITY}: $FirstInstall"

  ; Shortcut defaults (see the shortcut sections).
  ${If} $FirstInstall != "1"
    ${If} ${Silent}
      ; Silent over-installs (updates and dev reinstalls) never recreate or
      ; rewrite the Start menu shortcut: absent stays absent and existing stays
      ; untouched.
      !insertmacro UnselectSection ${SecStartMenu}
    ${ElseIfNot} ${FileExists} "${DELTA_START_MENU_LINK}"
      !insertmacro UnselectSection ${SecStartMenu}
    ${EndIf}
  ${EndIf}
  ${If} $FirstInstall != "1"
  ${AndIfNot} ${FileExists} "${DELTA_DESKTOP_LINK}"
    !insertmacro UnselectSection ${SecDesktop}
    !if "${DELTA_DESKTOP_ON_UPDATE}" == "1"
      ${If} ${Silent}
        !insertmacro SelectSection ${SecDesktop}
      ${EndIf}
    !endif
  ${EndIf}
FunctionEnd

; Deletes the shortcut on the stack, trying again for 5 s while Explorer
; still holds a new shortcut open; succeeds when there is none.
Function un.DeltaDeleteShortcut
  Exch $0
  Push $1
  StrCpy $1 50
  ${Do}
    ClearErrors
    Delete "$0"
    ${IfNot} ${Errors}
      ${Break}
    ${EndIf}
    IntOp $1 $1 - 1
    ${If} $1 <= 0
      ${Break}
    ${EndIf}
    Sleep 100
  ${Loop}
  Pop $1
  Pop $0
FunctionEnd

Function un.onInit
  Call un.DeltaReadLogOption
FunctionEnd

Section "Uninstall"
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${If} ${Errors}
    Call un.DeltaCloseApp
  ${EndIf}

  !insertmacro DELTA_REMOVE_INSTALLED_FILES

  ; An update (run by the next installer) stops here: the installer then
  ; writes the new files, the uninstaller, any selected shortcuts and the entry.
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${If} ${Errors}
    Push "${DELTA_START_MENU_LINK}"
    Call un.DeltaDeleteShortcut
    Push "${DELTA_DESKTOP_LINK}"
    Call un.DeltaDeleteShortcut
    DeleteRegKey HKCU "${DELTA_UNINSTALL_KEY}"
    Delete "$INSTDIR\Uninstall.exe"
    ; Only when empty: anything the user put there stays.
    RMDir "$INSTDIR"
    !insertmacro DELTA_LOG "uninstall done"
  ${EndIf}
SectionEnd
