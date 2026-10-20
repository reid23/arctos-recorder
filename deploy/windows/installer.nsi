; The base station's Windows installer, built by deploy/windows/package.sh
; with VERSION, APP (the folder to install), ICON and OUTFILE defined.

Unicode true
!include "MUI2.nsh"

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ArctosRecorder"
; Cameras find the base station by broadcasting on UDP port 7701 and then
; talk to it on TCP port 7700, from the self-assigned addresses of the
; camera network.
!define FIREWALL_RULE "Arctos Recorder cameras"

Name "Arctos Recorder"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\Arctos Recorder"
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_FINISHPAGE_RUN "$INSTDIR\bsw.exe"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section
    SetShellVarContext all
    SetOutPath "$INSTDIR"
    File /r "${APP}/*"
    File "/oname=icon.ico" "${ICON}"
    WriteUninstaller "$INSTDIR\Uninstall.exe"
    CreateShortcut "$SMPROGRAMS\Arctos Recorder.lnk" "$INSTDIR\bsw.exe" "" "$INSTDIR\icon.ico"

    nsExec::Exec 'netsh advfirewall firewall delete rule name="${FIREWALL_RULE}"'
    nsExec::Exec 'netsh advfirewall firewall add rule name="${FIREWALL_RULE}" dir=in action=allow program="$INSTDIR\bsw.exe" protocol=UDP localport=7701 remoteip=169.254.0.0/16 profile=any'
    nsExec::Exec 'netsh advfirewall firewall add rule name="${FIREWALL_RULE}" dir=in action=allow program="$INSTDIR\bsw.exe" protocol=TCP localport=7700 remoteip=169.254.0.0/16 profile=any'

    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "Arctos Recorder"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\icon.ico"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
    WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
    WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
    SetShellVarContext all
    nsExec::Exec 'netsh advfirewall firewall delete rule name="${FIREWALL_RULE}"'
    Delete "$SMPROGRAMS\Arctos Recorder.lnk"
    RMDir /r "$INSTDIR"
    DeleteRegKey HKLM "${UNINSTALL_KEY}"
SectionEnd
