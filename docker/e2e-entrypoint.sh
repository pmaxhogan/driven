#!/bin/bash
# Entrypoint for the Driven e2e image (Dockerfile `e2e-runtime` stage).
#
# Boots the headless session plumbing every mode needs:
#   1. Xvfb on $DISPLAY (the WebKitGTK webview needs a real X display even
#      headless; there is no pure-offscreen mode for a Tauri window).
#   2. A session D-Bus + gnome-keyring with an empty-password login keyring,
#      so the `keyring` crate's secret-service calls (backend credentials,
#      per-account master keys) work exactly like a desktop session.
#   3. tauri-driver (WebDriver front) -> WebKitWebDriver (native driver),
#      listening on 0.0.0.0:4444 so a host-side runner can also attach.
#
# Modes:
#   docker run IMAGE                 -> run the full suite: driven-e2e run-all
#   docker run IMAGE driven-e2e ...  -> a specific driven-e2e invocation
#   docker run IMAGE hold            -> boot plumbing, then sleep forever
#         (the agent-exploration mode: `docker exec` in and drive the app,
#          inject faults, poke the state DB with driven-cli / sqlite3)
#   docker run IMAGE <anything else> -> exec'd verbatim after boot
#
# Everything runs inside one dbus-run-session so the keyring + app share a
# session bus, mirroring a real login session.

set -e

boot_and_exec() {
    # --- 1. Xvfb -----------------------------------------------------------
    display_num="${DISPLAY#:}"
    Xvfb "${DISPLAY}" -screen 0 1280x800x24 -nolisten tcp &
    # Wait for the display socket rather than sleeping blind.
    for _ in $(seq 1 50); do
        [ -e "/tmp/.X11-unix/X${display_num}" ] && break
        sleep 0.1
    done

    # --- 2. keyring (already inside dbus-run-session) ------------------------
    # Create-and-unlock the login keyring with a fixed throwaway password.
    # NON-empty on purpose: an empty stdin leaves the freshly-created login
    # collection LOCKED, and then every secret-service WRITE raises a prompt
    # no headless session can answer - the keyring crate surfaces that as
    # "SS error: result not returned from SS API" after a hang (debugged
    # 2026-08-02 with secret-tool + the Collection.Locked property). Reads of
    # absent entries worked either way, which made the breakage look like an
    # app bug rather than a locked collection.
    eval "$(printf 'driven-e2e\n' | gnome-keyring-daemon --replace --unlock --components=secrets 2>/dev/null)" || true
    export GNOME_KEYRING_CONTROL

    # --- 3. tauri-driver -> WebKitWebDriver ---------------------------------
    tauri-driver --port 4444 --native-port 4445 \
        --native-driver /usr/bin/WebKitWebDriver &
    for _ in $(seq 1 50); do
        curl -fsS "http://127.0.0.1:4444/status" >/dev/null 2>&1 && break
        sleep 0.1
    done

    exec "$@"
}

case "$1" in
    "")
        set -- driven-e2e run-all
        ;;
    hold)
        set -- sleep infinity
        ;;
esac

# Re-exec the boot under a session bus if we are not already inside one.
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    export BOOT_ARGS_MARKER=1
    exec dbus-run-session -- "$0" __booted__ "$@"
fi

if [ "$1" = "__booted__" ]; then
    shift
fi

boot_and_exec "$@"
