#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Megatron Sysmon Agent.

A deliberately small WebSocket agent for the DGX Spark nodes. It samples the
node (CPU, RAM, swap, load, GPU power/utilization/temperature) and, when a vLLM
server answers on the local metrics port, the vLLM engine values (loaded model,
token rates, KV-cache usage, running/waiting requests). Every connected client
receives one JSON document per push interval.

Design constraints:

* Python standard library only - nothing to install on the node.
* Push instead of poll: the desktop applet opens one TCP connection and simply
  reads frames, so there is no per-sample process spawn on the node.
* ``nvidia-smi`` runs once in ``--loop-ms`` mode instead of being spawned for
  every sample, which keeps the GPU query cost near zero.
* The vLLM metrics endpoint is scraped locally (loopback) at a slower rate than
  the node values and only the needed lines are parsed.
* The applet may answer on the same connection: ``{"action":"processes"}``
  returns the process table (``limit`` clamped to 1..1000, default 200) and
  ``{"action":"kill","pid":<n>,"signal":<n>}`` signals a process, restricted to
  SIGTERM (15) and SIGKILL (9).

Usage::

    megatron_sysmon_agent.py --port 8787 --push-interval 2
    megatron_sysmon_agent.py --once          # print one snapshot and exit
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pwd
import socket
import struct
import subprocess
import sys
import threading
import time
import urllib.request

# Reported to the applet, which enables the process actions from 2.2 on. The
# fallback speaks the same protocol, so it must report the same API version as
# the Rust agent (agent/rust/Cargo.toml).
VERSION = "2.2.0"
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

# Ports probed when looking for a local LLM server on the node.
LLM_PORT_CANDIDATES = (8000, 8001, 8002, 8080, 8081, 9000, 30000, 11434)
DISCOVERY_INTERVAL_S = 60.0
GIB = 1024.0 * 1024.0 * 1024.0

# vLLM metric names we consume. The ``vllm:`` prefix is used by vLLM >= 0.9.
VLLM_METRICS = (
    "vllm:num_requests_running",
    "vllm:num_requests_waiting",
    "vllm:kv_cache_usage_perc",
    "vllm:gpu_cache_usage_perc",  # older name, kept as a fallback
    "vllm:prompt_tokens_total",
    "vllm:generation_tokens_total",
    "vllm:time_to_first_token_seconds_sum",
    "vllm:time_to_first_token_seconds_count",
    "vllm:num_preemptions_total",
    "vllm:spec_decode_num_draft_tokens_total",
    "vllm:spec_decode_num_accepted_tokens_total",
)

# Client commands that read or signal processes on the node.
PROCESS_DEFAULT_LIMIT = 200
PROCESS_LIMIT_MIN = 1
PROCESS_LIMIT_MAX = 1000
PROCESS_CMD_MAX = 160  # a longer cmdline is cut here and marked with "…"
# Signals the applet is allowed to send: SIGTERM and SIGKILL, nothing else.
KILL_SIGNALS = (15, 9)


def log(message: str) -> None:
    """Minimal stderr logging; systemd collects it into the journal."""
    print(f"[megatron-sysmon] {message}", file=sys.stderr, flush=True)


def to_float(text: str) -> float:
    """Reads the leading number of a value such as ``11.56 W`` or ``[N/A]``."""
    number = ""
    for char in text:
        if char.isdigit() or char in "+-.eE":
            number += char
        elif number:
            break
    try:
        return float(number)
    except ValueError:
        return 0.0


def read_meminfo() -> dict:
    values = {}
    try:
        with open("/proc/meminfo", "r", encoding="ascii") as handle:
            for line in handle:
                key, _, rest = line.partition(":")
                values[key] = to_float(rest)
    except OSError:
        pass
    return values


def parse_proc_stat(text: str) -> tuple[str, str, int, float, float] | None:
    """Parses ``/proc/<pid>/stat`` into (name, state, ppid, utime, stime).

    The ``comm`` field is wrapped in parentheses and may itself contain spaces
    and parentheses, so the fields after it are taken from the LAST ')' of the
    line. Returns None for a line that does not carry the needed fields.
    """
    open_index = text.find("(")
    close_index = text.rfind(")")
    if open_index < 0 or close_index < open_index:
        return None
    name = text[open_index + 1:close_index]
    fields = text[close_index + 1:].split()
    if len(fields) < 13:
        return None
    try:
        return (name, fields[0], int(fields[1]), float(fields[11]), float(fields[12]))
    except ValueError:
        return None


class NodeCollector:
    """Samples /proc (cheap) and keeps the previous CPU counters."""

    def __init__(self) -> None:
        self.hostname = socket.gethostname()
        self.cpu_count = os.cpu_count() or 0
        self._previous_cpu: tuple[float, float] | None = None
        self._cpu_pct = 0.0

    def _cpu(self) -> float:
        try:
            with open("/proc/stat", "r", encoding="ascii") as handle:
                fields = handle.readline().split()
        except OSError:
            return self._cpu_pct
        if len(fields) < 6 or fields[0] != "cpu":
            return self._cpu_pct
        values = [float(value) for value in fields[1:]]
        idle = values[3] + (values[4] if len(values) > 4 else 0.0)
        total = sum(values[:8])
        if self._previous_cpu is not None:
            total_delta = total - self._previous_cpu[0]
            idle_delta = idle - self._previous_cpu[1]
            if total_delta > 0:
                self._cpu_pct = max(0.0, min(100.0, (total_delta - idle_delta) / total_delta * 100.0))
        self._previous_cpu = (total, idle)
        return self._cpu_pct

    def _uptime(self) -> float:
        try:
            with open("/proc/uptime", "r", encoding="ascii") as handle:
                return to_float(handle.read().split()[0])
        except (OSError, IndexError):
            return 0.0

    def sample(self) -> dict:
        meminfo = read_meminfo()
        total_kib = meminfo.get("MemTotal", 0.0)
        available_kib = meminfo.get("MemAvailable", 0.0)
        swap_total_kib = meminfo.get("SwapTotal", 0.0)
        swap_free_kib = meminfo.get("SwapFree", 0.0)
        total_gib = total_kib / (1024.0 * 1024.0)
        available_gib = available_kib / (1024.0 * 1024.0)
        used_gib = max(0.0, total_gib - available_gib)

        load1 = load5 = load15 = 0.0
        try:
            with open("/proc/loadavg", "r", encoding="ascii") as handle:
                fields = handle.read().split()
            load1, load5, load15 = (float(fields[0]), float(fields[1]), float(fields[2]))
        except (OSError, IndexError, ValueError):
            pass

        return {
            "cpu_pct": round(self._cpu(), 1),
            "cpu_count": self.cpu_count,
            "load1": round(load1, 2),
            "load5": round(load5, 2),
            "load15": round(load15, 2),
            "mem_total_gib": round(total_gib, 2),
            "mem_used_gib": round(used_gib, 2),
            "mem_avail_gib": round(available_gib, 2),
            "mem_pct": round(used_gib / total_gib * 100.0, 1) if total_gib > 0 else 0.0,
            "swap_total_gib": round(swap_total_kib / (1024.0 * 1024.0), 2),
            "swap_used_gib": round((swap_total_kib - swap_free_kib) / (1024.0 * 1024.0), 2),
        }


class ProcessCollector:
    """Reads the process table from ``/proc`` for the applet.

    ``cpu`` is the share of ONE core in percent and is derived from the jiffy
    deltas between two ``processes`` requests of this same agent process::

        cpu = delta_process_jiffies / delta_all_cpu_jiffies * cores * 100

    The all-CPU jiffies come from the ``cpu `` line of ``/proc/stat``. On the
    first request the deltas are unknown, so every value is 0.0. Processes that
    vanish while scanning are skipped silently.
    """

    def __init__(self) -> None:
        self.cores = os.cpu_count() or 0
        self._lock = threading.Lock()
        self._previous: tuple[float, dict[int, float]] | None = None
        self._user_names: dict[int, str] = {}

    @staticmethod
    def _limit(value) -> int:
        """Clamps the requested row count to the documented range."""
        if isinstance(value, bool):
            return PROCESS_DEFAULT_LIMIT
        try:
            number = int(value)
        except (TypeError, ValueError):
            return PROCESS_DEFAULT_LIMIT
        return max(PROCESS_LIMIT_MIN, min(PROCESS_LIMIT_MAX, number))

    @staticmethod
    def _read_text(path: str) -> str:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            return handle.read()

    @staticmethod
    def _total_jiffies() -> float:
        """Sum of the first eight numeric fields of the aggregate ``cpu `` line.

        That is user, nice, system, idle, iowait, irq, softirq and steal, the
        same form the node CPU share and the Rust agent use. ``guest`` and
        ``guest_nice`` are already contained in ``user`` and ``nice``, so adding
        them would double count the guest time.
        """
        fields = ProcessCollector._read_text("/proc/stat").splitlines()[0].split()
        if not fields or fields[0] != "cpu":
            raise OSError("unerwartetes Format in /proc/stat")
        values = []
        for field in fields[1:]:
            try:
                values.append(float(field))
            except ValueError:
                continue
        return float(sum(values[:8]))

    @staticmethod
    def _cmdline(path: str) -> str:
        with open(path, "rb") as handle:
            raw = handle.read()
        return raw.replace(b"\x00", b" ").decode("utf-8", errors="replace").strip()

    def _user(self, uid: int) -> str:
        """Resolves a uid once; unknown uids keep their numeric form."""
        name = self._user_names.get(uid)
        if name is None:
            try:
                name = pwd.getpwuid(uid).pw_name
            except (KeyError, OSError):
                name = str(uid)
            self._user_names[uid] = name
        return name

    def _scan(self) -> tuple[dict[int, float], list[dict]]:
        """Returns the jiffies per pid and one row per readable process."""
        jiffies: dict[int, float] = {}
        rows: list[dict] = []
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            pid = int(entry)
            base = f"/proc/{entry}"
            try:
                parsed = parse_proc_stat(self._read_text(f"{base}/stat"))
            except OSError:
                continue  # the process disappeared while scanning
            if parsed is None:
                continue
            name, state, ppid, utime, stime = parsed
            try:
                comm = self._read_text(f"{base}/comm").strip()
            except OSError:
                comm = ""
            if comm:
                name = comm
            uid = None
            mem_kb = 0
            try:
                status = self._read_text(f"{base}/status")
            except OSError:
                status = ""
            for line in status.splitlines():
                if line.startswith("Uid:"):
                    fields = line.split()
                    if len(fields) > 1:
                        try:
                            uid = int(fields[1])
                        except ValueError:
                            pass
                elif line.startswith("VmRSS:"):
                    fields = line.split()
                    if len(fields) > 1:
                        try:
                            mem_kb = int(fields[1])
                        except ValueError:
                            pass
            try:
                cmd = self._cmdline(f"{base}/cmdline")
            except OSError:
                cmd = ""
            if not cmd:
                # Kernel threads have an empty cmdline; mark them as such.
                cmd = f"[{name}]"
            if len(cmd) > PROCESS_CMD_MAX:
                # Cut at the character limit and mark the cut, like the Rust agent.
                cmd = cmd[:PROCESS_CMD_MAX] + "…"
            jiffies[pid] = utime + stime
            rows.append({
                "pid": pid,
                "ppid": ppid,
                "name": name,
                # An unreadable status line reports no user rather than a made-up one.
                "user": self._user(uid) if uid is not None else "-",
                "cpu": 0.0,
                "mem_kb": mem_kb,
                "state": state,
                "cmd": cmd,
            })
        return jiffies, rows

    def processes(self, limit) -> dict:
        """Builds the ``processes`` reply; the caller sends it as JSON."""
        requested = self._limit(limit)
        with self._lock:
            try:
                total_jiffies = self._total_jiffies()
                jiffies, rows = self._scan()
            except OSError as error:
                return {"ok": False, "action": "processes",
                        "error": f"/proc nicht lesbar: {error}"}

            previous = self._previous
            self._previous = (total_jiffies, jiffies)
            if previous is not None:
                previous_total, previous_jiffies = previous
                total_delta = total_jiffies - previous_total
                if total_delta > 0:
                    for row in rows:
                        pid = row["pid"]
                        delta = jiffies[pid] - previous_jiffies.get(pid, jiffies[pid])
                        if delta <= 0:
                            continue  # new process or counters went backwards
                        row["cpu"] = round(delta / total_delta * self.cores * 100.0, 1)

            # Busiest first; the pid keeps equal values deterministic.
            rows.sort(key=lambda row: (-row["cpu"], row["pid"]))
            return {
                "ok": True,
                "action": "processes",
                "ts": round(time.time(), 3),
                "total": len(rows),
                "cores": self.cores,
                "processes": rows[:requested],
            }

    def kill(self, pid, signal_number) -> dict:
        """Signals one process, refusing the documented cases in order."""
        try:
            target = int(pid)
        except (TypeError, ValueError):
            target = None
        try:
            number = int(signal_number)
        except (TypeError, ValueError):
            number = -1
        reported = target if target is not None else 0

        def refusal(reason: str) -> dict:
            return {"ok": False, "action": "kill", "pid": reported, "signal": number,
                    "error": reason}

        if number not in KILL_SIGNALS:
            return refusal("nur SIGTERM (15) und SIGKILL (9) sind erlaubt")
        if target is None:
            return refusal("ungültige PID")
        if target <= 1:
            return refusal("PID 1 kann nicht beendet werden")
        if target == os.getpid():
            return refusal("der Agent kann sich nicht selbst beenden")
        # Never signal the agent's own process group: that would take the agent
        # down with the target.
        if target == os.getpgrp():
            return refusal("der Agent kann sich nicht selbst beenden")
        try:
            os.kill(target, number)
        except ProcessLookupError:
            return refusal("Prozess existiert nicht mehr")
        except PermissionError:
            return refusal("keine Berechtigung (EPERM)")
        except OSError as error:
            return refusal(str(error)[:160])
        return {"ok": True, "action": "kill", "pid": target, "signal": number}

class GpuCollector:
    """Reads GPU values from one long-running ``nvidia-smi --loop-ms`` process."""

    QUERY = "name,utilization.gpu,utilization.memory,temperature.gpu,power.draw,clocks.current.sm"

    def __init__(self, interval_s: float) -> None:
        self.interval_ms = max(500, int(interval_s * 1000))
        self._lock = threading.Lock()
        self._state = {"available": False, "name": "", "util_pct": 0.0, "mem_util_pct": 0.0,
                       "temp_c": 0.0, "power_w": 0.0, "sm_mhz": 0.0}
        self._stop = threading.Event()

    def start(self) -> None:
        threading.Thread(target=self._run, name="gpu", daemon=True).start()

    def stop(self) -> None:
        self._stop.set()

    def sample(self) -> dict:
        with self._lock:
            return dict(self._state)

    def _run(self) -> None:
        command = ["nvidia-smi", f"--query-gpu={self.QUERY}", "--format=csv,noheader",
                   f"--loop-ms={self.interval_ms}"]
        while not self._stop.is_set():
            try:
                process = subprocess.Popen(
                    command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                    text=True, bufsize=1,
                )
            except (OSError, ValueError):
                log("nvidia-smi is not available; GPU values stay empty")
                return
            try:
                for line in process.stdout:  # type: ignore[union-attr]
                    fields = [field.strip() for field in line.split(",")]
                    if len(fields) < 6:
                        continue
                    state = {
                        "available": True,
                        "name": fields[0],
                        "util_pct": round(to_float(fields[1]), 1),
                        "mem_util_pct": round(to_float(fields[2]), 1),
                        "temp_c": round(to_float(fields[3]), 1),
                        "power_w": round(to_float(fields[4]), 2),
                        "sm_mhz": round(to_float(fields[5]), 1),
                    }
                    with self._lock:
                        self._state = state
                    if self._stop.is_set():
                        break
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
            if not self._stop.is_set():
                time.sleep(5)


def http_json(url: str, timeout: float) -> dict | None:
    """Reads a JSON document, returning None for any failure."""
    try:
        request = urllib.request.Request(url, headers={"Accept": "application/json"})
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8", errors="replace"))
    except Exception:  # noqa: BLE001 - discovery must never raise
        return None


def http_contains(url: str, needle: str, timeout: float, limit: int = 65536) -> bool:
    """Checks whether a text endpoint answers and contains a marker."""
    try:
        request = urllib.request.Request(url, headers={"Accept": "text/plain"})
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return needle in response.read(limit).decode("utf-8", errors="replace")
    except Exception:  # noqa: BLE001
        return False


def port_open(host: str, port: int, timeout: float = 0.3) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


class EngineDiscovery:
    """Finds local LLM servers so that no endpoint has to be configured by hand.

    The operator names the host; the agent probes the usual ports and reports
    every OpenAI-compatible server (vLLM, SGLang, ...) and every Ollama instance
    together with the served models. Discovery runs on a slow interval and only
    probes ports that accept a TCP connection, so an idle node costs almost
    nothing.
    """

    def __init__(self, host: str, ports, interval: float, enabled: bool = True,
                 fallback_metrics: str = "") -> None:
        self.host = host
        self.ports = tuple(ports)
        self.interval = interval
        self.enabled = enabled
        self.fallback_metrics = fallback_metrics
        self._lock = threading.Lock()
        self._engines: list[dict] = []
        self._stop = threading.Event()
        self._on_primary = None

    def start(self, on_primary=None) -> None:
        self._on_primary = on_primary
        threading.Thread(target=self._run, name="discover", daemon=True).start()

    def stop(self) -> None:
        self._stop.set()

    def engines(self) -> list[dict]:
        with self._lock:
            return [dict(engine) for engine in self._engines]

    def _classify(self, port: int) -> dict | None:
        endpoint = f"http://{self.host}:{port}"
        listing = http_json(f"{endpoint}/v1/models", timeout=2.0)
        if isinstance(listing, dict):
            models = [str(item.get("id")) for item in listing.get("data", []) if isinstance(item, dict)]
            is_vllm = http_contains(f"{endpoint}/metrics", "vllm:", timeout=2.0)
            return {
                "port": port,
                "kind": "vllm" if is_vllm else "openai",
                "endpoint": endpoint,
                "metrics": f"{endpoint}/metrics" if is_vllm else "",
                "models": models,
            }
        tags = http_json(f"{endpoint}/api/tags", timeout=2.0)
        if isinstance(tags, dict) and isinstance(tags.get("models"), list):
            models = [str(item.get("name")) for item in tags["models"] if isinstance(item, dict)]
            return {"port": port, "kind": "ollama", "endpoint": endpoint, "metrics": "", "models": models}
        return None

    def _discover(self) -> list[dict]:
        engines: list[dict] = []
        for port in self.ports:
            if not port_open(self.host, port):
                continue
            found = self._classify(port)
            if found:
                engines.append(found)
        fallback_port = 0
        if self.fallback_metrics:
            try:
                fallback_port = int(self.fallback_metrics.rsplit(":", 1)[1].split("/", 1)[0])
            except (IndexError, ValueError):
                fallback_port = 0
        if (self.fallback_metrics and fallback_port and port_open(self.host, fallback_port)
                and not any(engine.get("metrics") for engine in engines)):
            # Keep an explicitly configured metrics URL working even when the
            # server does not answer the model listing during discovery.
            engines.append({
                "port": fallback_port,
                "kind": "vllm",
                "endpoint": self.fallback_metrics.rsplit("/metrics", 1)[0],
                "metrics": self.fallback_metrics,
                "models": [],
            })
        return engines

    def _run(self) -> None:
        while not self._stop.is_set():
            engines = self._discover() if self.enabled else []
            if engines:
                engines[0]["primary"] = any(engine.get("metrics") for engine in engines)
            with self._lock:
                self._engines = engines
            primary = next((engine for engine in engines if engine.get("metrics")), None) or (engines[0] if engines else None)
            if self._on_primary:
                self._on_primary(primary)
            self._stop.wait(self.interval)


def classify_vllm_error(error: Exception) -> tuple[str, str]:
    """Classifies a failed metrics fetch.

    A headless tensor-parallel rank has no local API port, so a refused
    connection there is a normal state rather than a fault. Returns a short
    reason code for clients and a readable message for humans.
    """
    text = str(error)
    lowered = text.lower()
    if "refused" in lowered or "errno 111" in lowered:
        return "no-endpoint", "kein lokaler vLLM-Endpunkt (headless Rank?)"
    if "timed out" in lowered or "timeout" in lowered:
        return "timeout", "Zeitüberschreitung beim Lesen der vLLM-Metriken"
    return "error", text[:160]


class VllmCollector:
    """Scrapes the local vLLM ``/metrics`` endpoint and derives rates."""

    def __init__(self, url: str, interval_s: float) -> None:
        self.url = url
        self._url_lock = threading.Lock()
        self.interval_s = interval_s
        self._lock = threading.Lock()
        self._state: dict = {"available": False, "error": "noch nicht gelesen", "reason": "pending"}
        self._previous: tuple[float, dict[str, float]] | None = None
        self._stop = threading.Event()

    def start(self) -> None:
        threading.Thread(target=self._run, name="vllm", daemon=True).start()

    def stop(self) -> None:
        self._stop.set()

    def sample(self) -> dict:
        with self._lock:
            return dict(self._state)

    def set_url(self, url: str) -> None:
        """Follows the endpoint that discovery selected."""
        if not url:
            return
        with self._url_lock:
            changed = url != self.url
            self.url = url
        if changed:
            self._previous = None
            log(f"vLLM metrics endpoint is now {url}")

    def current_url(self) -> str:
        with self._url_lock:
            return self.url

    def _scrape(self) -> tuple[dict[str, float], str]:
        request = urllib.request.Request(self.current_url(), headers={"Accept": "text/plain"})
        with urllib.request.urlopen(request, timeout=5) as response:
            body = response.read().decode("utf-8", errors="replace")

        values: dict[str, float] = {}
        model = ""
        for line in body.splitlines():
            if line.startswith("#") or not line:
                continue
            name = line.split("{", 1)[0].split(" ", 1)[0]
            if name not in VLLM_METRICS:
                continue
            try:
                values[name] = float(line.rsplit(" ", 1)[1])
            except (IndexError, ValueError):
                continue
            if not model and 'model_name="' in line:
                model = line.split('model_name="', 1)[1].split('"', 1)[0]
        values["__model__"] = model  # type: ignore[assignment]
        return values, model

    def _run(self) -> None:
        while not self._stop.is_set():
            if not self.current_url():
                with self._lock:
                    self._state = {"available": False, "reason": "pending",
                                   "error": "kein lokaler LLM-Endpunkt gefunden"}
                # Short wait so a discovery hit is picked up immediately.
                self._stop.wait(min(0.5, self.interval_s))
                continue
            try:
                raw, model = self._scrape()
                now = time.monotonic()
                state = self._derive(raw, model, now)
                with self._lock:
                    self._state = state
            except Exception as error:  # noqa: BLE001 - any failure means "not available"
                reason, message = classify_vllm_error(error)
                with self._lock:
                    self._state = {"available": False, "error": message, "reason": reason}
                self._previous = None
            self._stop.wait(self.interval_s)

    def _derive(self, raw: dict, model: str, now: float) -> dict:
        def counter(name: str) -> float:
            return float(raw.get(name, 0.0))

        kv_key = ("vllm:kv_cache_usage_perc"
                  if "vllm:kv_cache_usage_perc" in raw else "vllm:gpu_cache_usage_perc")
        # vLLM documents this gauge as a fraction: 1.0 == 100 %.
        kv_pct = counter(kv_key) * 100.0

        state = {
            "available": True,
            "error": None,
            "model": model,
            "running": int(counter("vllm:num_requests_running")),
            "waiting": int(counter("vllm:num_requests_waiting")),
            "kv_cache_pct": round(kv_pct, 1),
            "gen_tokens_total": int(counter("vllm:generation_tokens_total")),
            "prompt_tokens_total": int(counter("vllm:prompt_tokens_total")),
            "preemptions_total": int(counter("vllm:num_preemptions_total")),
            "ttft_ms": 0.0,
            "gen_tok_s": 0.0,
            "prompt_tok_s": 0.0,
            "spec_accept_pct": None,
        }

        samples = counter("vllm:time_to_first_token_seconds_count")
        total_ttft = counter("vllm:time_to_first_token_seconds_sum")
        state["ttft_ms"] = round(total_ttft / samples * 1000.0, 1) if samples > 0 else 0.0

        drafts = counter("vllm:spec_decode_num_draft_tokens_total")
        accepted = counter("vllm:spec_decode_num_accepted_tokens_total")
        if drafts > 0:
            state["spec_accept_pct"] = round(accepted / drafts * 100.0, 1)

        previous = self._previous
        self._previous = (now, dict(raw))
        if previous is None:
            return state

        previous_at, previous_raw = previous
        seconds = now - previous_at
        if seconds <= 0.5:
            return state

        gen_delta = counter("vllm:generation_tokens_total") - previous_raw.get("vllm:generation_tokens_total", 0.0)
        prompt_delta = counter("vllm:prompt_tokens_total") - previous_raw.get("vllm:prompt_tokens_total", 0.0)
        accepted_delta = accepted - previous_raw.get("vllm:spec_decode_num_accepted_tokens_total", 0.0)
        drafts_delta = drafts - previous_raw.get("vllm:spec_decode_num_draft_tokens_total", 0.0)
        # A vLLM restart resets the counters; negative deltas are not a token rate.
        state["gen_tok_s"] = round(max(0.0, gen_delta) / seconds, 1)
        state["prompt_tok_s"] = round(max(0.0, prompt_delta) / seconds, 1)
        if drafts_delta > 0:
            state["spec_accept_pct"] = round(max(0.0, accepted_delta) / drafts_delta * 100.0, 1)
        return state


class Agent:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.node = NodeCollector()
        self.proc = ProcessCollector()
        self.gpu = GpuCollector(args.gpu_interval)
        self.vllm = VllmCollector(args.vllm_url, args.vllm_interval)
        self.discovery = EngineDiscovery(
            "127.0.0.1",
            LLM_PORT_CANDIDATES,
            args.discovery_interval,
            enabled=args.discover,
            fallback_metrics=args.vllm_url,
        )
        self.clients: list[Client] = []
        self.clients_lock = threading.Lock()
        self.snapshot_lock = threading.Lock()
        self.last_snapshot: dict = {}

    def start(self) -> None:
        self.gpu.start()
        if self.args.vllm_url or self.args.discover:
            self.vllm.start()

        def follow(primary: dict | None) -> None:
            if primary and primary.get("metrics"):
                self.vllm.set_url(primary["metrics"])

        self.discovery.start(follow)

    def snapshot(self) -> dict:
        document = {
            "agent": "megatron-sysmon",
            "version": VERSION,
            "host": self.node.hostname,
            "ts": round(time.time(), 3),
            "uptime_s": round(self.node._uptime(), 1),
            "node": self.node.sample(),
            "gpu": self.gpu.sample(),
            "vllm": self.vllm.sample() if (self.args.vllm_url or self.args.discover) else {
                "available": False,
                "reason": "disabled",
                "error": "LLM-Monitoring auf diesem Knoten deaktiviert",
            },
            "llm": {
                "autodiscover": bool(self.args.discover),
                "endpoint": self.vllm.current_url(),
                "engines": self.discovery.engines(),
            },
        }
        with self.snapshot_lock:
            self.last_snapshot = document
        return document

    def processes(self, limit) -> dict:
        """Reply for ``{"action":"processes","limit":<n>}``."""
        return self.proc.processes(limit)

    def kill(self, pid, signal_number) -> dict:
        """Reply for ``{"action":"kill","pid":<n>,"signal":<n>}``."""
        return self.proc.kill(pid, signal_number)

    def add_client(self, client: "Client") -> None:
        with self.clients_lock:
            self.clients.append(client)

    def remove_client(self, client: "Client") -> None:
        with self.clients_lock:
            if client in self.clients:
                self.clients.remove(client)

    def broadcast(self, payload: str) -> None:
        with self.clients_lock:
            clients = list(self.clients)
        for client in clients:
            if not client.send_text(payload):
                self.remove_client(client)

    def serve(self) -> None:
        self.start()
        server = self._listen()
        log(f"listening on {server.getsockname()[0]}:{self.args.port} "
            f"(push {self.args.push_interval}s, vLLM {self.args.vllm_url or 'off'})")

        threading.Thread(target=self._push_loop, name="push", daemon=True).start()

        while True:
            try:
                connection, address = server.accept()
            except OSError as error:
                log(f"accept failed: {error}")
                continue
            threading.Thread(target=self._handle, args=(connection, address), daemon=True).start()

    def _listen(self) -> socket.socket:
        """Listens on both families when possible.

        A dual-stack `::` socket also accepts IPv4 clients (bindv6only=0 is the
        Linux default), which matters on hosts that only resolve to IPv6. If IPv6
        is unavailable the agent falls back to IPv4 only.
        """
        want_ipv6 = self.args.bind in ("::", "0.0.0.0", "")
        if want_ipv6 and socket.has_ipv6:
            try:
                server = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
                server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                try:
                    server.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
                except OSError:
                    pass
                server.bind(("::", self.args.port))
                server.listen(8)
                return server
            except OSError as error:
                log(f"IPv6 bind failed ({error}); falling back to IPv4")
        server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind((self.args.bind if self.args.bind != "::" else "0.0.0.0", self.args.port))
        server.listen(8)
        return server

    def _push_loop(self) -> None:
        while True:
            payload = json.dumps(self.snapshot())
            self.broadcast(payload)
            time.sleep(self.args.push_interval)

    def _handle(self, connection: socket.socket, address: tuple) -> None:
        try:
            connection.settimeout(10)
            if not self._handshake(connection):
                connection.close()
                return
            connection.settimeout(None)
        except OSError:
            connection.close()
            return

        client = Client(connection)
        self.add_client(client)
        log(f"client connected: {address[0]}")
        client.send_text(json.dumps(self.snapshot()))
        try:
            client.read_loop(self)
        finally:
            self.remove_client(client)
            connection.close()
            log(f"client disconnected: {address[0]}")

    def _handshake(self, connection: socket.socket) -> bool:
        data = b""
        while b"\r\n\r\n" not in data and len(data) < 16384:
            chunk = connection.recv(4096)
            if not chunk:
                return False
            data += chunk
        head = data.split(b"\r\n\r\n", 1)[0].decode("latin-1")
        lines = head.split("\r\n")
        request_line = lines[0] if lines else ""
        headers = {}
        for line in lines[1:]:
            name, separator, value = line.partition(":")
            if separator:
                headers[name.strip().lower()] = value.strip()

        query = request_line.split(" ", 2)[1] if len(request_line.split(" ")) > 2 else ""
        if self.args.token:
            supplied = ""
            if "token=" in query:
                supplied = query.split("token=", 1)[1].split("&", 1)[0]
            supplied = headers.get("x-sysmon-token", supplied)
            if supplied != self.args.token:
                connection.sendall(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                log(f"rejected client without valid token from {request_line}")
                return False

        key = headers.get("sec-websocket-key")
        if not key or headers.get("upgrade", "").lower() != "websocket":
            connection.sendall(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            return False

        accept = base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest()).decode()
        response = (
            "HTTP/1.1 101 Switching Protocols\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        connection.sendall(response.encode("ascii"))
        return True


class Client:
    """One WebSocket client; the agent only ever sends text frames."""

    def __init__(self, connection: socket.socket) -> None:
        self.connection = connection
        self.lock = threading.Lock()
        self.alive = True

    def send_text(self, payload: str) -> bool:
        return self._send(0x1, payload.encode("utf-8"))

    def _send(self, opcode: int, payload: bytes) -> bool:
        header = bytearray([0x80 | opcode])
        length = len(payload)
        if length < 126:
            header.append(length)
        elif length < 65536:
            header.append(126)
            header += struct.pack("!H", length)
        else:
            header.append(127)
            header += struct.pack("!Q", length)
        try:
            with self.lock:
                self.connection.sendall(bytes(header) + payload)
            return True
        except OSError:
            self.alive = False
            return False

    def read_loop(self, agent: Agent) -> None:
        while self.alive:
            try:
                opcode, payload = self._read_frame()
            except OSError:
                return
            if opcode == 0x8:  # close
                self._send(0x8, b"")
                return
            if opcode == 0x9:  # ping
                self._send(0xA, payload)
                continue
            if opcode in (0x1, 0x2):
                try:
                    request = json.loads(payload.decode("utf-8", errors="replace"))
                except ValueError:
                    continue
                if not isinstance(request, dict):
                    continue
                if request.get("cmd") == "snapshot":
                    self.send_text(json.dumps(agent.snapshot()))
                    continue
                # Commands from the applet. Everything else stays unanswered,
                # exactly as before.
                action = request.get("action")
                if action == "processes":
                    self.send_text(json.dumps(agent.processes(request.get("limit"))))
                elif action == "kill":
                    self.send_text(json.dumps(agent.kill(request.get("pid"),
                                                         request.get("signal"))))

    def _read_frame(self) -> tuple[int, bytes]:
        first, second = self._read_exact(2)
        opcode = first & 0x0F
        masked = bool(second & 0x80)
        length = second & 0x7F
        if length == 126:
            length = struct.unpack("!H", self._read_exact(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._read_exact(8))[0]
        if length > 1_048_576:
            raise OSError("frame too large")
        mask = self._read_exact(4) if masked else b""
        payload = self._read_exact(length)
        if masked:
            payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        return opcode, payload

    def _read_exact(self, count: int) -> bytes:
        data = b""
        while len(data) < count:
            chunk = self.connection.recv(count - len(data))
            if not chunk:
                raise OSError("connection closed")
            data += chunk
        return data


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Megatron Sysmon Agent (WebSocket metrics)")
    parser.add_argument("--bind", default="0.0.0.0", help="address to listen on (default 0.0.0.0)")
    parser.add_argument("--port", type=int, default=8787, help="TCP port (default 8787)")
    parser.add_argument("--push-interval", type=float, default=2.0, help="seconds between pushes")
    parser.add_argument("--gpu-interval", type=float, default=3.0, help="nvidia-smi sample interval")
    parser.add_argument("--vllm-url", default="http://127.0.0.1:8000/metrics",
                        help="local vLLM metrics URL, empty string disables it")
    parser.add_argument("--vllm-interval", type=float, default=4.0, help="vLLM scrape interval")
    parser.add_argument("--discover", dest="discover", action="store_true", default=True,
                        help="find local LLM endpoints automatically (default)")
    parser.add_argument("--no-discover", dest="discover", action="store_false",
                        help="disable automatic endpoint discovery")
    parser.add_argument("--discovery-interval", type=float, default=DISCOVERY_INTERVAL_S,
                        help="seconds between discovery scans")
    parser.add_argument("--token", default="", help="optional shared token required from clients")
    parser.add_argument("--once", action="store_true", help="print one snapshot as JSON and exit")
    parser.add_argument("--version", action="version", version=f"megatron-sysmon-agent {VERSION}")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    agent = Agent(args)
    if args.once:
        agent.start()
        # Give the background collectors a moment to deliver their first values.
        time.sleep(max(1.5, args.gpu_interval * 0.6))
        print(json.dumps(agent.snapshot(), indent=2))
        return 0
    try:
        agent.serve()
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
