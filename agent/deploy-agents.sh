#!/usr/bin/env bash
# Deploy, check or remove the Megatron Sysmon Agent on the Spark nodes.
#
# Runs on the desktop and drives the nodes over SSH:
#   ./deploy-agents.sh install     # copy + install + start, then probe
#   ./deploy-agents.sh status      # unit state and probe output per node
#   ./deploy-agents.sh probe       # only the WebSocket check
#   ./deploy-agents.sh uninstall   # stop and remove the agent on the nodes
#
# Override the target list with SYSMON_AGENT_HOSTS="node-01 node-02".
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOSTS="${SYSMON_AGENT_HOSTS:-node-01 node-02}"
PORT="${SYSMON_AGENT_PORT:-8787}"
REMOTE_DIR="/tmp/megatron-sysmon-agent"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=8)

usage() {
    cat <<EOF
Usage: deploy-agents.sh <command>

Commands:
  install     Copy the agent to ${HOSTS// /, }, install and start it, then probe
  status      Show unit state, port and cost on every node, then probe
  probe       Only run the WebSocket check against every node
  uninstall   Stop and remove the agent on every node

Environment: SYSMON_AGENT_HOSTS (default "${HOSTS}"), SYSMON_AGENT_PORT (default ${PORT})
EOF
}

copy_files() {
    local host="$1"
    ssh "${SSH_OPTS[@]}" "${host}" "rm -rf ${REMOTE_DIR} && mkdir -p ${REMOTE_DIR}"
    scp -q "${SSH_OPTS[@]}" \
        "${HERE}/megatron_sysmon_agent.py" \
        "${HERE}/megatron-sysmon-agent.service" \
        "${HERE}/install-agent.sh" \
        "${host}:${REMOTE_DIR}/"
    for binary in "${HERE}/rust/dist/"megatron-sysmon-agent-*; do
        [ -f "${binary}" ] || continue
        scp -q "${SSH_OPTS[@]}" "${binary}" "${host}:${REMOTE_DIR}/"
    done
    [ -f "${HERE}/rust/dist/megatron-sysmon-agent-aarch64" ] || \
        echo "hint: no binary yet - run agent/build-agent.sh for the Rust agent"
}

do_install() {
    for host in ${HOSTS}; do
        echo "=== ${host}: install"
        copy_files "${host}"
        ssh "${SSH_OPTS[@]}" "${host}" "sudo -n bash ${REMOTE_DIR}/install-agent.sh install"
        echo
    done
    do_probe
}

do_status() {
    for host in ${HOSTS}; do
        echo "=== ${host}: status"
        ssh "${SSH_OPTS[@]}" "${host}" \
            "sudo -n bash ${REMOTE_DIR}/install-agent.sh status 2>/dev/null || \
             sudo -n systemctl --no-pager --property=ActiveState,SubState,MainPID,CPUUsageNSec,MemoryCurrent megatron-sysmon-agent.service; \
             ss -ltnp 2>/dev/null | grep -E ':${PORT}\b' || true"
        echo
    done
    do_probe
}

do_probe() {
    for host in ${HOSTS}; do
        python3 "${HERE}/ws_probe.py" "${host}:${PORT}" --seconds 5 || true
        echo
    done
}

do_uninstall() {
    for host in ${HOSTS}; do
        echo "=== ${host}: uninstall"
        copy_files "${host}"
        ssh "${SSH_OPTS[@]}" "${host}" "sudo -n bash ${REMOTE_DIR}/install-agent.sh uninstall"
        echo
    done
}

case "${1:-}" in
    install) do_install ;;
    status) do_status ;;
    probe) do_probe ;;
    uninstall) do_uninstall ;;
    *) usage; exit 2 ;;
esac
