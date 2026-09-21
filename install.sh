#!/usr/bin/env bash
# Install, remove or extend the COSMIC system monitor applet.
set -euo pipefail

APP_ID="com.iambarth.CosmicAppletSysmon"
BIN_NAME="cosmic-applet-sysmon"
PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BIN_DIR="${HOME}/.local/bin"
DESKTOP_DIR="${HOME}/.local/share/applications"
ICON_DIR="${HOME}/.local/share/icons/hicolor/scalable/apps"
RAPL_RULE="/etc/udev/rules.d/90-rapl-readable.rules"

usage() {
    cat <<'EOF'
Usage: install.sh <command>

Commands:
  build        Compile the release binary (cargo build --release)
  install      Install binary, desktop entry and icons into ~/.local, then add
               the applet to the COSMIC panel layout
  panel-add    Add the applet to the COSMIC panel/dock layout only
  panel-remove Remove the applet from the COSMIC panel/dock layout only
  rapl-enable  Install a udev rule that makes the CPU package power counter
               (/sys/class/powercap/intel-rapl:0/energy_uj) readable, so the
               applet can show real CPU/system wattage (needs sudo)
  status       Show the installation state
  uninstall    Remove binary, desktop entry, icons and panel entries
EOF
}

require_binary() {
    if [ ! -x "${PROJECT_DIR}/target/release/${BIN_NAME}" ]; then
        echo "error: ${PROJECT_DIR}/target/release/${BIN_NAME} is missing, run 'install.sh build'" >&2
        exit 1
    fi
}

do_build() {
    cd "${PROJECT_DIR}"
    cargo build --release
}

do_install() {
    require_binary
    install -Dm755 "${PROJECT_DIR}/target/release/${BIN_NAME}" "${BIN_DIR}/${BIN_NAME}"
    restart_running_applets
    install -Dm644 "${PROJECT_DIR}/res/${APP_ID}.desktop" "${DESKTOP_DIR}/${APP_ID}.desktop"
    install -Dm644 "${PROJECT_DIR}"/res/icons/apps/*.svg "${ICON_DIR}/"
    update-desktop-database "${DESKTOP_DIR}" >/dev/null 2>&1 || true
    gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true
    echo "installed ${BIN_DIR}/${BIN_NAME}"
    panel_add
}

# A replaced binary does not affect processes that are already running: the
# panel keeps the old image until the applet is respawned. A panel config change
# makes it load the new binary immediately.
restart_running_applets() {
    if ! pgrep -f "${BIN_NAME}$" >/dev/null 2>&1; then
        return
    fi
    echo "restarting running applet instances so they load the new binary"
    panel_remove
    sleep 1
    panel_add
}

panel_add() {
    python3 "${PROJECT_DIR}/scripts/panel-applet.py" add "${APP_ID}"
}

panel_remove() {
    python3 "${PROJECT_DIR}/scripts/panel-applet.py" remove "${APP_ID}"
}

do_rapl() {
    echo "Installing ${RAPL_RULE} (needs root)"
    sudo tee "${RAPL_RULE}" >/dev/null <<'EOF'
# Make the RAPL package energy counter readable for regular users so that
# desktop monitoring tools can display CPU package power without root.
SUBSYSTEM=="powercap", ACTION=="add|change", RUN+="/bin/chmod 0444 /sys/class/powercap/%k/energy_uj"
EOF
    sudo udevadm control --reload-rules
    sudo udevadm trigger --subsystem-match=powercap
    sudo chmod 0444 /sys/class/powercap/intel-rapl:0/energy_uj || true
    if [ -r /sys/class/powercap/intel-rapl:0/energy_uj ]; then
        echo "ok: $(cat /sys/class/powercap/intel-rapl:0/energy_uj) uJ readable"
    else
        echo "warning: counter is still not readable" >&2
    fi
}

do_status() {
    for path in "${BIN_DIR}/${BIN_NAME}" "${DESKTOP_DIR}/${APP_ID}.desktop"; do
        [ -e "${path}" ] && echo "present ${path}" || echo "missing ${path}"
    done
    ls "${ICON_DIR}/${APP_ID}"*.svg >/dev/null 2>&1 && echo "present icons" || echo "missing icons"
    python3 "${PROJECT_DIR}/scripts/panel-applet.py" status "${APP_ID}"
    if [ -r /sys/class/powercap/intel-rapl:0/energy_uj ]; then
        echo "rapl: readable"
    else
        echo "rapl: not readable (run 'install.sh rapl-enable')"
    fi
}

do_uninstall() {
    panel_remove || true
    rm -f "${BIN_DIR}/${BIN_NAME}" "${DESKTOP_DIR}/${APP_ID}.desktop"
    rm -f "${ICON_DIR}/${APP_ID}"*.svg
    update-desktop-database "${DESKTOP_DIR}" >/dev/null 2>&1 || true
    gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true
    echo "removed applet (kept ${RAPL_RULE})"
}

case "${1:-}" in
    build) do_build ;;
    install) do_install ;;
    panel-add) panel_add ;;
    panel-remove) panel_remove ;;
    rapl-enable) do_rapl ;;
    status) do_status ;;
    uninstall) do_uninstall ;;
    *) usage; exit 2 ;;
esac
