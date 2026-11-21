; Athrix NSIS installer hooks (bundle.windows.nsis.installerHooks in tauri.conf.json).
;
; Task 5.4: Explorer's right-click "Search here with Athrix" on folders, drives and the
; background of an open folder. Per-user (HKCU\Software\Classes), so no admin rights, matching
; the per-user install (installMode "currentUser"). The verb runs
;   "<install dir>\Athrix.exe" --search-in "%V"
; and the app turns it into an infolder:"<path>" search (src/launch_args.rs).
; Windows 11 lists it under "Show more options" (the classic menu).
;
; This file must stay UTF-8 *with BOM*: NSIS reads it as ANSI otherwise and the Arabic label
; is garbled.

!define ATHRIX_VERB "AthrixSearchHere"

; Writes the verb under HKCU\Software\Classes\<ROOT>\shell. The label is in $R0.
!macro ATHRIX_ADD_VERB ROOT
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\${ATHRIX_VERB}" "MUIVerb" "$R0"
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\${ATHRIX_VERB}" "Icon" '"$INSTDIR\${MAINBINARYNAME}.exe",0'
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\${ATHRIX_VERB}\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" --search-in "%V"'
!macroend

!macro ATHRIX_REMOVE_VERB ROOT
  DeleteRegKey HKCU "Software\Classes\${ROOT}\shell\${ATHRIX_VERB}"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  Push $R0
  ; The label follows the installer's language (1025 = Arabic).
  ${If} $LANGUAGE == 1025
    StrCpy $R0 "ابحث هنا بـ Athrix"
  ${Else}
    StrCpy $R0 "Search here with Athrix"
  ${EndIf}
  !insertmacro ATHRIX_ADD_VERB "Directory"
  !insertmacro ATHRIX_ADD_VERB "Directory\Background"
  !insertmacro ATHRIX_ADD_VERB "Drive"
  Pop $R0
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro ATHRIX_REMOVE_VERB "Directory"
  !insertmacro ATHRIX_REMOVE_VERB "Directory\Background"
  !insertmacro ATHRIX_REMOVE_VERB "Drive"
!macroend
