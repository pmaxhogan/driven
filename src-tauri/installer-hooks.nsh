; NSIS installer hooks for the Driven Windows bundle (issue #125).
;
; Tauri's NSIS installer template inserts NSIS_HOOK_PREINSTALL at the very top of
; the Install section - before SetOutPath and the first File write, and before
; its own CheckIfAppIsRunning step. That built-in kill targets ONLY the main
; binary (driven-app.exe); it never touches the bundled driven-vss-helper.exe
; sidecar. If an elevated VSS helper broker is still running (a same-session
; broker, or an orphan from a crashed / force-restarted prior session), it holds
; an open handle to its own exe and the install aborts with:
;
;   Error opening file for writing: ...\driven-vss-helper.exe   [Abort/Retry/Ignore]
;
; Belt-and-braces: force-terminate any lingering helper before files are copied.
; Error tolerated - taskkill exits non-zero when no such process exists, which is
; the normal (no helper running) case; we discard the exit code with `Pop $0`.
;
; LIMITATION (see the PR for #125): the helper always runs ELEVATED (it refuses to
; start un-elevated), and the default NSIS install mode is `currentUser`, so the
; updater-spawned installer runs UN-elevated. A medium-integrity taskkill cannot
; terminate the high-integrity helper (access-denied), so this hook only bites
; when the installer itself is elevated (an admin-run / perMachine install). The
; primary fix for the same-session case is the app-side pipe Shutdown before
; `download_and_install`; the durable fix for a crash-orphaned elevated helper is
; tracked as a follow-up (a parent-death watchdog in the helper).

!macro NSIS_HOOK_PREINSTALL
  nsExec::Exec '"$SYSDIR\taskkill.exe" /F /IM driven-vss-helper.exe'
  Pop $0
!macroend
