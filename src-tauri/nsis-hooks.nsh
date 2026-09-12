; Process handling around install/uninstall.
;
; Without this, installing or uninstalling while Kitty is running leaves locked
; .exe and .dll files behind — confirmed real bug, twice. The first time, an
; uninstall with the app open removed the off-by-default plugins (never
; running, never locked) and left kitty.exe and every running plugin in place.
; The second time, an upgrade failed with a wall of
;   "Error opening file for writing: ...\resources\libGemmaModelConstraintProvider.dll"
; because the LiteRT DLLs are loaded by the BigTiny daemon, which was still
; running.
;
; That second failure happened *despite* this file, because the kill list had
; gone stale: it named `bigtiny-daemon.exe` (BigTiny **V1**) and five retired
; plugins, and named none of the three processes that actually run today. A
; list of process names is exactly the kind of thing that rots silently when
; binaries are renamed, so the names below are the current ones from
; `tauri.conf.json`'s `externalBin`.
;
; `nsExec::Exec` (built into NSIS) runs each command hidden, so no console
; window flashes.

; Kitty's own processes. Stopped without asking: they belong to the app being
; installed or removed, and stopping them is implied by the action the user
; just started.
!macro StopKittyProcesses
  nsExec::Exec 'taskkill /F /IM kitty.exe'
  ; `kitty-tools` / `kitty-web` / `kitty-wasm` are deliberately NOT here. They
  ; are the V2 daemon's children, not Kitty's, and its health watcher respawns
  ; them within seconds — so killing them ahead of the question below would
  ; both accomplish nothing and disturb a shared daemon the user may be about
  ; to decline to stop. They go with the daemon, in `AskToStopDaemon`.
  ;
  ; BigTiny V1 and the retired Python plugins. Kept because an install that has
  ; been upgraded across versions can still have an old one running, and a
  ; `taskkill` for an absent image is a harmless no-op.
  nsExec::Exec 'taskkill /F /IM bigtiny-daemon.exe'
  nsExec::Exec 'taskkill /F /IM adaptive-pathway-sidecar.exe'
  nsExec::Exec 'taskkill /F /IM adaptive-pathway-mcp.exe'
  nsExec::Exec 'taskkill /F /IM replacement-mcp.exe'
  nsExec::Exec 'taskkill /F /IM brave-mcp-search.exe'
  nsExec::Exec 'taskkill /F /IM wasm-math-mcp.exe'
  nsExec::Exec 'taskkill /F /IM visualizations.exe'
!macroend

; The BigTiny V2 daemon is asked about rather than killed, which is the one
; place this file deliberately differs from the rest.
;
; V2 inverted V1's ownership model: the daemon is a shared machine resource
; that Kitty *attaches to*, not a child it owns. Another frontend may be
; attached to the same daemon, and a scheduled task can be mid-run with no
; window open at all — so stopping it is not obviously the user's intent the
; way stopping Kitty itself is. `lifecycle/bigtiny_v2.rs` states the rule for
; the running app ("Never kill"); this is the installer honouring the same one.
;
; It still has to stop for the install to succeed, because it holds the LiteRT
; DLLs open. So declining aborts with a plain explanation rather than letting
; the user walk into the file-in-use cascade that started all this.
;
; SUFFIX makes the labels unique: this macro is expanded twice (install and
; uninstall) and NSIS would otherwise see duplicate label definitions.
!macro AskToStopDaemon SUFFIX
  ; `find` exits 0 only when the image is actually in the task list, so this is
  ; a presence check without needing a plugin.
  nsExec::Exec 'cmd /c tasklist /FI "IMAGENAME eq bigtiny2-daemon.exe" /NH | find /I "bigtiny2-daemon.exe"'
  ; `$R9`, not `$0`: the hook is spliced into Tauri's own installer script,
  ; which uses the low registers around it.
  Pop $R9
  StrCmp $R9 "0" 0 daemon_absent_${SUFFIX}

    ; A silent install (/S) has nobody to ask and must not hang on a dialog.
    ; Stopping is the right default there: it is what the caller asked for by
    ; running the installer at all.
    IfSilent stop_daemon_${SUFFIX}

    MessageBox MB_YESNO|MB_ICONQUESTION \
      "BigTiny is still running.$\n$\n\
It has to stop before the files it has open can be replaced, but it is shared \
— another app may be attached to it, and a scheduled task may be running with \
no window open.$\n$\n\
Stop BigTiny and continue?" \
      IDYES stop_daemon_${SUFFIX}

    Abort "Cancelled — BigTiny is still running. Close it, or re-run this and \
choose Yes."

  stop_daemon_${SUFFIX}:
    nsExec::Exec 'taskkill /F /IM bigtiny2-daemon.exe'
    ; Its MCP servers are children of the daemon and are respawned by its health
    ; watcher, so they have to go with it rather than before it.
    nsExec::Exec 'taskkill /F /IM kitty-tools.exe'
    nsExec::Exec 'taskkill /F /IM kitty-web.exe'
    nsExec::Exec 'taskkill /F /IM kitty-wasm.exe'

  daemon_absent_${SUFFIX}:
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro StopKittyProcesses
  !insertmacro AskToStopDaemon "install"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro StopKittyProcesses
  !insertmacro AskToStopDaemon "uninstall"
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
