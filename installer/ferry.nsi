; Ferry installer (NSIS 3, built on Linux by CI with makensis).
; Per-user install: no admin rights, files under %LOCALAPPDATA%\Programs\Ferry.
; The Start Menu shortcut is created by ferry.exe itself on first run, because it must carry
; Ferry's AppUserModelID for notifications; the installer only removes it on uninstall.
;
; makensis -DVERSION=1.2.3 -DVERNUM=1.2.3 -DBIN=<dir with ferry.exe> -DOUT=<setup exe path> installer/ferry.nsi

Unicode true
!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef BIN
  !define BIN "../target/x86_64-pc-windows-gnu/release"
!endif
!ifndef VERNUM
  !define VERNUM "0.0.0"
!endif
!ifndef OUT
  !define OUT "Ferry-Setup-${VERSION}.exe"
!endif

!define APP "Ferry"
!define PUBLISHER "Ferry"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Ferry"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "${APP}"
OutFile "${OUT}"
InstallDir "$LOCALAPPDATA\Programs\Ferry"
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma
ManifestDPIAware true

VIProductVersion "${VERNUM}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "FileDescription" "${APP} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "${PUBLISHER}"

BrandingText "Ferry ${VERSION}"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!define MUI_ICON "../assets/brand/ferry.ico"
!define MUI_UNICON "../assets/brand/ferry.ico"
!define MUI_ABORTWARNING

; Branded artwork from assets/brand/build.py, one file per display scale (100-200%). Each page
; loads the file matching the screen's DPI at its native size, so Windows never resizes it:
; resizing a bitmap at run time is what made the text look jagged.
!define ART "../assets/brand"
!define MUI_WELCOMEFINISHPAGE_BITMAP "${ART}/installer-side-100.bmp"
!define MUI_WELCOMEFINISHPAGE_BITMAP_STRETCH "NoStretchNoCrop"
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_RIGHT
!define MUI_HEADERIMAGE_BITMAP "${ART}/installer-header-100.bmp"
!define MUI_HEADERIMAGE_BITMAP_STRETCH "NoStretchNoCrop"
!define MUI_HEADERIMAGE_UNBITMAP "${ART}/installer-header-100.bmp"
!define MUI_HEADERIMAGE_UNBITMAP_STRETCH "NoStretchNoCrop"
!define MUI_CUSTOMFUNCTION_GUIINIT FerryGuiInit
!define MUI_CUSTOMFUNCTION_UNGUIINIT un.FerryGuiInit

!define MUI_WELCOMEPAGE_TITLE "Install Ferry ${VERSION}"
!define MUI_WELCOMEPAGE_TEXT "Ferry keeps folders on one server in step with another by carrying the files on this laptop, instead of sending them over the internet.$\r$\n$\r$\nFerry installs for you only, without admin rights, and runs quietly in the notification area.$\r$\n$\r$\nSelect Next to continue."
!define MUI_FINISHPAGE_TITLE "Ferry is installed"
!define MUI_FINISHPAGE_TEXT "Ferry runs in the notification area. Open it from its tray icon to add your two servers and choose the folders to carry."
!define MUI_FINISHPAGE_RUN "$INSTDIR\ferry.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start Ferry now"
!define MUI_UNCONFIRMPAGE_TEXT_TOP "Ferry will be removed from this computer. Your settings, and any files still carried on this laptop, are kept in your user folders."

!define MUI_PAGE_CUSTOMFUNCTION_SHOW FerryWelcomeShow
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_SHOW FerryFinishShow
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Var Scale ; 100, 125, 150, 175 or 200, from the screen's DPI

!macro FerryArt UN
  ; System DPI (the installer is DPI-aware), rounded to the nearest artwork scale.
  Function ${UN}FerryScale
    System::Call 'user32::GetDC(p 0) p .r1'
    System::Call 'gdi32::GetDeviceCaps(p r1, i 88) i .r0'
    System::Call 'user32::ReleaseDC(p 0, p r1)'
    ${If} $0 <= 108
      StrCpy $Scale 100
    ${ElseIf} $0 <= 132
      StrCpy $Scale 125
    ${ElseIf} $0 <= 156
      StrCpy $Scale 150
    ${ElseIf} $0 <= 180
      StrCpy $Scale 175
    ${Else}
      StrCpy $Scale 200
    ${EndIf}
  FunctionEnd

  Function ${UN}FerryGuiInit
    Call ${UN}FerryScale
    InitPluginsDir
    File "/oname=$PLUGINSDIR\side-100.bmp" "${ART}/installer-side-100.bmp"
    File "/oname=$PLUGINSDIR\side-125.bmp" "${ART}/installer-side-125.bmp"
    File "/oname=$PLUGINSDIR\side-150.bmp" "${ART}/installer-side-150.bmp"
    File "/oname=$PLUGINSDIR\side-175.bmp" "${ART}/installer-side-175.bmp"
    File "/oname=$PLUGINSDIR\side-200.bmp" "${ART}/installer-side-200.bmp"
    File "/oname=$PLUGINSDIR\header-100.bmp" "${ART}/installer-header-100.bmp"
    File "/oname=$PLUGINSDIR\header-125.bmp" "${ART}/installer-header-125.bmp"
    File "/oname=$PLUGINSDIR\header-150.bmp" "${ART}/installer-header-150.bmp"
    File "/oname=$PLUGINSDIR\header-175.bmp" "${ART}/installer-header-175.bmp"
    File "/oname=$PLUGINSDIR\header-200.bmp" "${ART}/installer-header-200.bmp"
    ; Control 1046 is Modern UI's header image.
    SetBrandingImage /IMGID=1046 "$PLUGINSDIR\header-$Scale.bmp"
  FunctionEnd
!macroend
!insertmacro FerryArt ""
!insertmacro FerryArt "un."

Function FerryWelcomeShow
  ${NSD_FreeImage} $mui.WelcomePage.Image.Bitmap
  ${NSD_SetImage} $mui.WelcomePage.Image "$PLUGINSDIR\side-$Scale.bmp" $mui.WelcomePage.Image.Bitmap
FunctionEnd

Function FerryFinishShow
  ${NSD_FreeImage} $mui.FinishPage.Image.Bitmap
  ${NSD_SetImage} $mui.FinishPage.Image "$PLUGINSDIR\side-$Scale.bmp" $mui.FinishPage.Image.Bitmap
FunctionEnd

; Ferry can't be replaced or removed while it runs. ferry-ui.exe is the window program of
; early builds.
!macro StopFerry
  nsExec::Exec 'taskkill /F /IM ferry-ui.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM ferry.exe'
  Pop $0
  Sleep 500
!macroend

; A silent install (/S) has no finish page, and Ferry's own updater quit to let it run: start
; Ferry again either way. On a failure too, so the laptop isn't left without it until sign-in.
Function .onInstSuccess
  ${If} ${Silent}
    Exec '"$INSTDIR\ferry.exe"'
  ${EndIf}
FunctionEnd

Function .onInstFailed
  ${If} ${Silent}
  ${AndIf} ${FileExists} "$INSTDIR\ferry.exe"
    Exec '"$INSTDIR\ferry.exe"'
  ${EndIf}
FunctionEnd

Section "Ferry" SecMain
  SectionIn RO
  !insertmacro StopFerry
  SetOutPath "$INSTDIR"
  File "${BIN}/ferry.exe"
  Delete "$INSTDIR\ferry-ui.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\ferry.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  !insertmacro StopFerry
  Delete "$INSTDIR\ferry.exe"
  Delete "$INSTDIR\ferry-ui.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  ; Created by ferry.exe at runtime.
  Delete "$APPDATA\Microsoft\Windows\Start Menu\Programs\Ferry.lnk"
  DeleteRegValue HKCU "${RUN_KEY}" "Ferry"
  DeleteRegKey HKCU "${UNINST_KEY}"
  ; Settings (%APPDATA%\Ferry), state and any carried files (%LOCALAPPDATA%\Ferry) are kept on
  ; purpose: they may hold files not yet delivered home. Remove them by hand if you're done.
SectionEnd
