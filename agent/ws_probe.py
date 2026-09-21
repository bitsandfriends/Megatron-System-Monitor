#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Tiny WebSocket client used to check a Megatron Sysmon Agent.

It performs the client handshake by hand (standard library only), reads the
pushed JSON documents and prints a compact summary of the newest one. This is
the same data path the COSMIC applet uses, so a successful run proves that the
agent is reachable and delivering values.

Usage::

    ws_probe.py node-01:8787 [--seconds 6] [--json]
    ws_probe.py node-01:8787 --processes [N]      # Prozessliste anfordern
    ws_probe.py node-01:8787 --kill PID:SIGNAL    # Signal an einen Prozess
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import socket
import struct
import sys
import time

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
# How long the command modes wait for their reply; the agent pushes snapshots
# on the same connection, so a reply may arrive after a frame or two.
COMMAND_TIMEOUT_S = 10.0


def connect(host: str, port: int, timeout: float) -> socket.socket:
    connection = socket.create_connection((host, port), timeout=timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET / HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n"
        f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    connection.sendall(request.encode())

    data = b""
    while b"\r\n\r\n" not in data:
        chunk = connection.recv(4096)
        if not chunk:
            raise ConnectionError("server closed the connection during the handshake")
        data += chunk
    head = data.split(b"\r\n\r\n", 1)[0].decode("latin-1")
    if "101" not in head.split("\r\n")[0]:
        raise ConnectionError(f"handshake failed: {head.splitlines()[0]}")
    return connection


def read_exact(connection: socket.socket, count: int) -> bytes:
    data = b""
    while len(data) < count:
        chunk = connection.recv(count - len(data))
        if not chunk:
            raise ConnectionError("connection closed")
        data += chunk
    return data


def read_frame(connection: socket.socket) -> tuple[int, bytes]:
    first, second = read_exact(connection, 2)
    opcode = first & 0x0F
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", read_exact(connection, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", read_exact(connection, 8))[0]
    mask = read_exact(connection, 4) if second & 0x80 else b""
    payload = read_exact(connection, length)
    if mask:
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return opcode, payload


def summarize(document: dict) -> str:
    node = document.get("node", {})
    gpu = document.get("gpu", {})
    engine = document.get("vllm", {})
    lines = [
        f"{document.get('host')}  agent {document.get('agent')} {document.get('version')}",
        f"  CPU      {node.get('cpu_pct')} % von {node.get('cpu_count')} Kernen, load {node.get('load1')}",
        f"  RAM      {node.get('mem_used_gib')} / {node.get('mem_total_gib')} GiB ({node.get('mem_pct')} %), "
        f"Swap {node.get('swap_used_gib')} / {node.get('swap_total_gib')} GiB",
        f"  GPU      {gpu.get('name')} {gpu.get('util_pct')} %, {gpu.get('power_w')} W, "
        f"{gpu.get('temp_c')} °C, {gpu.get('sm_mhz')} MHz",
    ]
    if engine.get("available"):
        lines.append(
            f"  vLLM     {engine.get('model')} | {engine.get('gen_tok_s')} tok/s | "
            f"KV {engine.get('kv_cache_pct')} % | Agenten {engine.get('running')} aktiv, "
            f"{engine.get('waiting')} wartend | TTFT {engine.get('ttft_ms')} ms"
        )
    else:
        lines.append(f"  vLLM     nicht verfügbar: {engine.get('error')}")
    return "\n".join(lines)


def send_text(connection: socket.socket, payload: str) -> None:
    """Sends one client text frame (a client must mask its frames)."""
    data = payload.encode("utf-8")
    header = bytearray([0x81])
    length = len(data)
    if length < 126:
        header.append(0x80 | length)
    elif length < 65536:
        header.append(0x80 | 126)
        header += struct.pack("!H", length)
    else:
        header.append(0x80 | 127)
        header += struct.pack("!Q", length)
    mask = os.urandom(4)
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(data))
    connection.sendall(bytes(header) + mask + masked)


def close_cleanly(connection: socket.socket) -> None:
    """Sends a close frame and closes the socket, even if the peer is gone."""
    try:
        connection.sendall(b"\x88\x80" + os.urandom(4))
    except OSError:
        pass
    finally:
        connection.close()


def wait_for_reply(connection: socket.socket, action: str, timeout: float) -> dict:
    """Reads frames until the reply whose ``action`` matches.

    The agent pushes its snapshot documents on the same connection, so
    non-matching text frames are skipped instead of treated as the reply.
    """
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f'keine Antwort auf "{action}"')
        connection.settimeout(remaining)
        opcode, payload = read_frame(connection)
        if opcode == 0x8:
            raise ConnectionError("der Agent hat die Verbindung geschlossen")
        if opcode == 0x9:
            connection.sendall(b"\x8a\x80" + os.urandom(4))
            continue
        if opcode != 0x1:
            continue
        try:
            document = json.loads(payload.decode("utf-8", errors="replace"))
        except ValueError:
            continue
        if isinstance(document, dict) and document.get("action") == action:
            return document


def summarize_processes(target: str, document: dict) -> str:
    processes = document.get("processes") or []
    lines = [
        f"{target}: {len(processes)} von {document.get('total')} Prozessen, "
        f"{document.get('cores')} Kerne, ts {document.get('ts')}",
        f"  {'PID':>7} {'PPID':>7} {'CPU %':>6} {'RSS MiB':>8} {'S':<2} "
        f"{'BENUTZER':<12} BEFEHL",
    ]
    for process in processes:
        command = str(process.get("cmd") or process.get("name") or "")
        if len(command) > 60:
            command = command[:57] + "..."
        mem_mib = (process.get("mem_kb") or 0) / 1024.0
        lines.append(
            f"  {process.get('pid') or 0:>7} {process.get('ppid') or 0:>7} "
            f"{process.get('cpu') or 0.0:>6} {mem_mib:>8.1f} "
            f"{str(process.get('state') or '?'):<2} "
            f"{str(process.get('user') or '?'):<12} {command}"
        )
    return "\n".join(lines)


def parse_kill(text: str) -> tuple[int, int]:
    """Parses ``PID:SIGNAL``; the caller must always name the signal."""
    pid_text, separator, signal_text = text.partition(":")
    if not separator or not pid_text or not signal_text:
        raise argparse.ArgumentTypeError("erwartet PID:SIGNAL, zum Beispiel 4711:15")
    try:
        return (int(pid_text), int(signal_text))
    except ValueError:
        raise argparse.ArgumentTypeError("PID und SIGNAL müssen ganze Zahlen sein") from None


def run_processes(connection: socket.socket, args: argparse.Namespace) -> int:
    """Requests the process table and prints it as a compact table."""
    request = json.dumps({"action": "processes", "limit": args.processes})
    try:
        send_text(connection, request)
        reply = wait_for_reply(connection, "processes", COMMAND_TIMEOUT_S)
    except (OSError, ConnectionError, TimeoutError) as error:
        close_cleanly(connection)
        print(f"{args.target}: Prozessliste fehlgeschlagen ({error})", file=sys.stderr)
        return 1
    close_cleanly(connection)

    if args.json:
        print(json.dumps(reply, indent=2, ensure_ascii=False))
        return 0 if reply.get("ok") else 1
    if not reply.get("ok"):
        print(f"{args.target}: Fehler: {reply.get('error')}", file=sys.stderr)
        return 1
    print(summarize_processes(args.target, reply))
    return 0


def run_kill(connection: socket.socket, args: argparse.Namespace) -> int:
    """Sends the kill request and prints the agent's answer."""
    pid, signal_number = args.kill
    request = json.dumps({"action": "kill", "pid": pid, "signal": signal_number})
    try:
        send_text(connection, request)
        reply = wait_for_reply(connection, "kill", COMMAND_TIMEOUT_S)
    except (OSError, ConnectionError, TimeoutError) as error:
        close_cleanly(connection)
        print(f"{args.target}: keine Antwort auf kill ({error})", file=sys.stderr)
        return 1
    close_cleanly(connection)

    if args.json:
        print(json.dumps(reply, indent=2, ensure_ascii=False))
        return 0 if reply.get("ok") else 1
    if reply.get("ok"):
        print(f"{args.target}: PID {reply.get('pid')} hat Signal "
              f"{reply.get('signal')} erhalten")
        return 0
    print(f"{args.target}: kill fehlgeschlagen (PID {pid}, Signal {signal_number}): "
          f"{reply.get('error')}", file=sys.stderr)
    return 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Check a Megatron Sysmon Agent over WebSocket")
    parser.add_argument("target", help="host or host:port (default port 8787)")
    parser.add_argument("--seconds", type=float, default=6.0, help="how long to listen")
    parser.add_argument("--json", action="store_true", help="print the raw JSON document")
    parser.add_argument("--processes", nargs="?", const=15, type=int, metavar="N",
                        help="request N processes (default 15) and print the table")
    parser.add_argument("--kill", type=parse_kill, metavar="PID:SIGNAL",
                        help="send SIGTERM (15) or SIGKILL (9) to PID and print the answer")
    args = parser.parse_args(argv)

    host, _, port_text = args.target.partition(":")
    port = int(port_text) if port_text else 8787

    try:
        connection = connect(host, port, timeout=5.0)
    except (OSError, ConnectionError) as error:
        print(f"{args.target}: not reachable ({error})", file=sys.stderr)
        return 1

    if args.kill is not None:
        return run_kill(connection, args)
    if args.processes is not None:
        return run_processes(connection, args)

    connection.settimeout(args.seconds)
    newest = None
    frames = 0
    deadline = time.monotonic() + args.seconds
    try:
        while time.monotonic() < deadline:
            opcode, payload = read_frame(connection)
            if opcode == 0x8:
                break
            if opcode == 0x9:
                connection.sendall(b"\x8a\x80" + os.urandom(4))
                continue
            if opcode == 0x1:
                frames += 1
                newest = json.loads(payload.decode("utf-8"))
    except (OSError, ConnectionError):
        pass
    finally:
        connection.close()

    if newest is None:
        print(f"{args.target}: connected but received no data frame", file=sys.stderr)
        return 1

    if args.json:
        print(json.dumps(newest, indent=2, ensure_ascii=False))
    else:
        print(f"{args.target}: ok, {frames} Frames in {args.seconds:g} s")
        print(summarize(newest))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
