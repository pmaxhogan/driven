// Shared IPC error normalization (SPEC s24; DESIGN s8.7). Every `#[tauri::command]`
// rejects with the stable `{ code, message, ... }` shape (SPEC s24), where `code`
// is the dotted error code that is ALSO the i18n bundle key. The frontend must
// never render the raw `message` (backend English) or `String(e)` (which can be
// `[object Object]` for a Tauri object error) - it reads `.code` and renders
// `t(\`errors.${code}.long\`)` (the M6 pattern). This module is the single seam
// that turns any thrown IPC error into that stable code.

/**
 * Map a rejected IPC error onto a stable SPEC s24 code. Tauri serializes the
 * `{ code, message, ... }` shape; we read `.code` when present and fall back to
 * `internal.bug` so the view always has a translatable key.
 */
export function toErrorCode(e: unknown): string {
  if (e && typeof e === "object" && "code" in e) {
    const code = (e as { code: unknown }).code;
    if (typeof code === "string" && code.length > 0) {
      return code;
    }
  }
  return "internal.bug";
}
