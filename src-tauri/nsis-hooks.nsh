; Force-kills Kitty and every child process it can spawn before install/
; uninstall file operations run. Without this, an uninstall while Kitty (or
; bigtiny-daemon/adaptive-pathway-sidecar/etc.) is still running leaves those
; locked .exe files behind in the install directory — confirmed real bug: an
; uninstall with the app open removed the off-by-default plugins
; (replacement-mcp, brave-mcp-search — never running, never locked) but left
; kitty.exe and every always-or-often-running plugin exe in place. Also run
; on install so upgrading over a running instance doesn't hit the same lock.
; `nsExec::Exec` (built into NSIS) runs each command hidden, no console flash.

!macro KillKittyProcesses
  nsExec::Exec 'taskkill /F /IM kitty.exe'
  nsExec::Exec 'taskkill /F /IM bigtiny-daemon.exe'
  nsExec::Exec 'taskkill /F /IM adaptive-pathway-sidecar.exe'
  nsExec::Exec 'taskkill /F /IM adaptive-pathway-mcp.exe'
  nsExec::Exec 'taskkill /F /IM replacement-mcp.exe'
  nsExec::Exec 'taskkill /F /IM brave-mcp-search.exe'
  nsExec::Exec 'taskkill /F /IM wasm-math-mcp.exe'
  nsExec::Exec 'taskkill /F /IM kitty-wasm.exe'
  nsExec::Exec 'taskkill /F /IM visualizations.exe'
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro KillKittyProcesses
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro KillKittyProcesses
!macroend

; Autostart is an HKCU Run entry written by the app itself (wizard.rs), not by
; the installer, so NSIS doesn't know to remove it — left behind, it makes
; Windows try to launch a now-deleted kitty.exe at every sign-in. `GooseOverlay`
; is the pre-rename value name; both are cleared. `/f` makes a missing value a
; no-op rather than an error.
!macro NSIS_HOOK_POSTUNINSTALL
  nsExec::Exec 'reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v Kitty /f'
  nsExec::Exec 'reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v GooseOverlay /f'
!macroend
