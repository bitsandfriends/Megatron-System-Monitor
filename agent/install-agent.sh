#!/usr/bin/env bash
# Install, check or remove the Megatron Sysmon Agent on one DGX Spark node.
#
# Run this on the node itself (it needs sudo for the systemd unit):
#   ./install-agent.sh install
#   ./install-agent.sh status
#   ./install-agent.sh uninstall
#
# From the desktop use agent/deploy-agents.sh, which copies this directory to
# the nodes and calls this script over SSH.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UNIT_NAME="megatron-sysmon-agent"
UNIT_PATH="/etc/systemd/system/${UNIT_NAME}.service"
MODE="${SYSMON_AGENT_MODE:-auto}"
LIB_DIR="/usr/local/lib/megatron-sysmon"
USER_LIB_DIR="${HOME}/.local/lib/megatron-sysmon"
USER_UNIT_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user"
USER_UNIT_PATH="${USER_UNIT_DIR}/megatron-sysmon-agent.service"
AGENT_SRC="${HERE}/megatron_sysmon_agent.py"
ARCH="$(uname -m)"
BIN_SRC="${HERE}/megatron-sysmon-agent-${ARCH}"
BIN_NAME="megatron-sysmon-agent"
UNIT_SRC="${HERE}/megatron-sysmon-agent.service"
PORT="${SYSMON_AGENT_PORT:-8787}"
AGENT_USER="${SYSMON_AGENT_USER:-${SUDO_USER:-$(id -un)}}"
PYTHON="$(command -v python3)"
AGENT_ARGS="--bind 0.0.0.0 --port ${PORT} --push-interval 2 --gpu-interval 3 --vllm-interval 4 --vllm-url http://127.0.0.1:8000/metrics"

# Resolved by resolve_mode(): "system" (root service) or "user" (user service).
resolve_mode() {
    [ "${MODE}" = "auto" ] || return
    if selinux_blocks_connect; then
        MODE="user"
    else
        MODE="system"
    fi
}

# On SELinux systems a unit without policy runs as init_t, and init_t may not
# connect to local service ports (name_connect denied) - the agent would then see
# no engines at all. A user service runs unconfined and needs no root.
selinux_blocks_connect() {
    command -v getenforce >/dev/null 2>&1 || return 1
    [ "$(getenforce 2>/dev/null)" = "Enforcing" ]
}

usage() {
    cat <<EOF
Usage: install-agent.sh <command>

Commands:
  install     Copy the agent, install and start ${UNIT_NAME}.service (sudo)
  status      Show unit state, listening port and resource usage
  once        Print one raw JSON snapshot (runs the agent in --once mode)
  uninstall   Stop and remove the unit and the installed agent
  logs        Follow the journal of ${UNIT_NAME}

Environment overrides: SYSMON_AGENT_PORT (default ${PORT}),
SYSMON_AGENT_USER (default ${AGENT_USER}).
EOF
}

require_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "error: this command needs root (use sudo)" >&2
        exit 1
    fi
}

install_binary() {
    local target_dir="$1"
    install -d -m755 "${target_dir}"
    if [ -f "${BIN_SRC}" ]; then
        install -m755 "${BIN_SRC}" "${target_dir}/${BIN_NAME}"
        EXEC="${target_dir}/${BIN_NAME} ${AGENT_ARGS}"
        echo "installing the Rust agent (${BIN_NAME})"
    else
        [ -f "${AGENT_SRC}" ] || { echo "error: neither ${BIN_SRC} nor ${AGENT_SRC} present" >&2; exit 1; }
        install -m755 "${AGENT_SRC}" "${target_dir}/megatron_sysmon_agent.py"
        EXEC="${PYTHON} ${target_dir}/megatron_sysmon_agent.py ${AGENT_ARGS}"
        echo "installing the Python agent (no binary for ${ARCH} shipped)"
    fi
}

write_unit() {
    local unit_path="$1" agent_user="$2" scope="${3:-system}"
    sed -e "s|__AGENT_USER__|${agent_user}|g" \
        -e "s|__PORT__|${PORT}|g" \
        -e "s|__PYTHON__|${PYTHON}|g" \
        -e "s|__EXEC__|${EXEC}|g" \
        "${UNIT_SRC}" > "${unit_path}"
    if [ "${scope}" = "user" ]; then
        # A user unit belongs to default.target, not multi-user.target.
        sed -i 's|^WantedBy=multi-user.target|WantedBy=default.target|' "${unit_path}"
    fi
}

# `systemctl --user` needs a session bus; an SSH command has none by default.
user_bus_env() {
    local uid
    uid="$(id -u)"
    : "${XDG_RUNTIME_DIR:=/run/user/${uid}}"
    export XDG_RUNTIME_DIR
    if [ ! -d "${XDG_RUNTIME_DIR}" ]; then
        # Linger creates the runtime directory without an interactive session.
        sudo -n loginctl enable-linger "$(id -un)" >/dev/null 2>&1 || true
        for _ in 1 2 3 4 5; do
            [ -d "${XDG_RUNTIME_DIR}" ] && break
            sleep 1
        done
    fi
    export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
}

# An earlier installation - a system service or a root user service - can still
# hold the port. Both run our own binary, so stopping them is safe.
cleanup_previous_agents() {
    if pgrep -f "megatron-sysmon-agent --bind" >/dev/null 2>&1; then
        pkill -f "megatron-sysmon-agent --bind" >/dev/null 2>&1 || \
            sudo -n pkill -f "megatron-sysmon-agent --bind" >/dev/null 2>&1 || true
        sleep 1
        echo "stopped a previously running agent"
    fi
    if [ "$(id -u)" != "0" ] && sudo -n test -d /root/.local/lib/megatron-sysmon 2>/dev/null; then
        sudo -n rm -rf /root/.local/lib/megatron-sysmon \
            /root/.config/systemd/user/megatron-sysmon-agent.service >/dev/null 2>&1 || true
        echo "removed an earlier installation that ran as root"
    fi
}

do_install_user() {
    user_bus_env
    # A leftover system unit would keep port 8787 occupied.
    if [ -f "${UNIT_PATH}" ]; then
        if sudo -n systemctl disable --now "${UNIT_NAME}.service" >/dev/null 2>&1; then
            sudo -n rm -f "${UNIT_PATH}" 2>/dev/null || true
            systemctl daemon-reload >/dev/null 2>&1 || true
            echo "removed the previous system service"
        else
            echo "warning: ${UNIT_PATH} still exists; stop it before the user service can bind the port" >&2
        fi
    fi
    cleanup_previous_agents
    install_binary "${USER_LIB_DIR}"
    install -d -m755 "${USER_UNIT_DIR}"
    write_unit "${USER_UNIT_PATH}" "$(id -un)" user
    systemctl --user daemon-reload
    systemctl --user enable "${UNIT_NAME}.service"
    # `enable --now` would leave a running unit alone; a restart is what picks up
    # the freshly installed binary.
    systemctl --user restart "${UNIT_NAME}.service"
    # Linger keeps the agent running without an open session.
    if command -v loginctl >/dev/null 2>&1; then
        if [ "$(loginctl show-user "$(id -un)" --property=Linger --value 2>/dev/null)" != "yes" ]; then
            if sudo -n loginctl enable-linger "$(id -un)" >/dev/null 2>&1; then
                echo "linger enabled: the agent also runs without a login session"
            else
                echo "hint: run 'sudo loginctl enable-linger $(id -un)' so the agent survives logout"
            fi
        fi
    fi
    echo "mode: user service (SELinux keeps a system service from reaching local ports)"
    open_firewall_port_unprivileged
}

open_firewall_port_unprivileged() {
    if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
        echo "firewall (ufw) is active; opening ${PORT}/tcp needs root - check 'ufw status'"
    else
        echo "firewall: nothing to open for a user service"
    fi
}

do_install() {
    resolve_mode
    if [ "${MODE}" = "user" ]; then
        do_install_user
        return
    fi
    require_root
    install -d -m755 "${LIB_DIR}"

    # The Rust binary is the default: a static-feeling single file with a much
    # smaller footprint. The Python agent stays as the compatible fallback for
    # hosts where the binary cannot be shipped.
    if [ -f "${BIN_SRC}" ]; then
        install -m755 "${BIN_SRC}" "${LIB_DIR}/${BIN_NAME}"
        EXEC="${LIB_DIR}/${BIN_NAME} ${AGENT_ARGS}"
        echo "installing the Rust agent (${BIN_NAME})"
    else
        [ -f "${AGENT_SRC}" ] || { echo "error: neither ${BIN_SRC} nor ${AGENT_SRC} present" >&2; exit 1; }
        install -m755 "${AGENT_SRC}" "${LIB_DIR}/megatron_sysmon_agent.py"
        EXEC="${PYTHON} ${LIB_DIR}/megatron_sysmon_agent.py ${AGENT_ARGS}"
        echo "installing the Python agent (no binary for ${ARCH} shipped)"
    fi

    sed -e "s|__AGENT_USER__|${AGENT_USER}|g" \
        -e "s|__PORT__|${PORT}|g" \
        -e "s|__PYTHON__|${PYTHON}|g" \
        -e "s|__EXEC__|${EXEC}|g" \
        "${UNIT_SRC}" > "${UNIT_PATH}"
    chmod 644 "${UNIT_PATH}"
    open_firewall_port
    systemctl daemon-reload
    systemctl enable --now "${UNIT_NAME}.service"
    # `enable --now` does not restart an already running service, so an updated
    # agent file would keep the old code loaded until the next reboot.
    systemctl restart "${UNIT_NAME}.service"
    sleep 2
    do_status
}

do_status() {
    echo "== unit"
    if systemctl show --no-pager \
        --property=ActiveState,SubState,MainPID,User,CPUUsageNSec,MemoryCurrent \
        "${UNIT_NAME}.service" >/dev/null 2>&1; then
        systemctl show --no-pager \
            --property=ActiveState,SubState,MainPID,User,CPUUsageNSec,MemoryCurrent \
            "${UNIT_NAME}.service"
    else
        echo "unit ${UNIT_NAME} not installed"
    fi
    echo "== listener"
    ss -ltnp 2>/dev/null | grep -E ":${PORT}\b" || echo "nothing listening on ${PORT}"
    echo "== process cost (pcpu is the average since process start)"
    ps -o pid,pcpu,pmem,rss,etimes,args -C "${BIN_NAME}" 2>/dev/null | grep "${BIN_NAME}" || true
    ps -o pid,pcpu,pmem,rss,etimes,args -C python3 2>/dev/null | grep megatron_sysmon_agent || true
    SELF_TEST=""
    for directory in "${LIB_DIR}" "${USER_LIB_DIR}"; do
        if [ -x "${directory}/${BIN_NAME}" ]; then
            SELF_TEST="${directory}/${BIN_NAME}"
        elif [ -x "${directory}/megatron_sysmon_agent.py" ]; then
            SELF_TEST="${PYTHON} ${directory}/megatron_sysmon_agent.py"
        fi
        [ -n "${SELF_TEST}" ] && break
    done
    if [ -n "${SELF_TEST}" ]; then
        echo "== self test (one snapshot, abridged)"
        ${SELF_TEST} --once --gpu-interval 0.5 2>/dev/null \
            | "${PYTHON}" -c 'import json,sys
d = json.load(sys.stdin)
n, g, v = d["node"], d["gpu"], d["vllm"]
print(f'"'"'{d["host"]}: cpu {n["cpu_pct"]}%, ram {n["mem_used_gib"]}/{n["mem_total_gib"]} GiB, gpu {g["power_w"]} W, vllm {v.get("model") or v.get("error")}'"'"')'
    fi
}

# The agent is useless if the host firewall drops its port. Open it where the
# usual tools are present, and report what happened either way.
open_firewall_port() {
    if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
        if firewall-cmd --query-port="${PORT}/tcp" >/dev/null 2>&1; then
            echo "firewall: port ${PORT}/tcp is already open (firewalld)"
        else
            firewall-cmd --permanent --add-port="${PORT}/tcp" >/dev/null 2>&1 || true
            firewall-cmd --reload >/dev/null 2>&1 || true
            echo "firewall: opened ${PORT}/tcp in firewalld"
        fi
    elif command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
        if ufw status 2>/dev/null | grep -q "${PORT}/tcp"; then
            echo "firewall: port ${PORT}/tcp is already open (ufw)"
        else
            ufw allow "${PORT}/tcp" >/dev/null 2>&1 || true
            echo "firewall: opened ${PORT}/tcp in ufw"
        fi
    else
        echo "firewall: neither firewalld nor ufw active - nothing to open"
    fi
}

do_once() {
    "${PYTHON}" "${AGENT_SRC}" --once "$@"
}

do_uninstall() {
    if [ -f "${USER_UNIT_PATH}" ] && [ ! -f "${UNIT_PATH}" ]; then
        systemctl --user disable --now "${UNIT_NAME}.service" >/dev/null 2>&1 || true
        rm -f "${USER_UNIT_PATH}"
        rm -rf "${USER_LIB_DIR}"
        systemctl --user daemon-reload >/dev/null 2>&1 || true
        echo "removed the user service"
        return
    fi
    require_root
    systemctl disable --now "${UNIT_NAME}.service" 2>/dev/null || true
    rm -f "${UNIT_PATH}"
    systemctl daemon-reload
    rm -rf "${LIB_DIR}"
    echo "removed ${UNIT_NAME} (unit, ${LIB_DIR})"
}

case "${1:-}" in
    install) do_install ;;
    status) do_status ;;
    once) shift; do_once "$@" ;;
    uninstall) do_uninstall ;;
    logs) journalctl -u "${UNIT_NAME}.service" -f ;;
    *) usage; exit 2 ;;
esac
