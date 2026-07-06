#!/usr/bin/env python3
"""aris — thin compatibility shim over celestia-devtools' shared WSL2 env.

All platform/WSL/docker logic lives in the ``celestia_devtools`` package
(``celestia_devtools.env.host``, ``.docker``, ``.wsl_select``, ``.wsl_exec``).
This module re-exports those names so the existing call sites in aris' build
scripts (``import build_env; build_env.wsl_main_guard()`` etc.) continue to
work **without modification**.

The only project-specific bit is ``PASSTHROUGH_ENV`` — the set of env vars
aris propagates Windows → WSL during re-exec. kei has its own shim with a
different set.

Requires ``celestia-devtools`` to be installed (``pip install -e celestia-devtools``).
"""
from __future__ import annotations

from pathlib import Path

from celestia_devtools.env import docker as _docker
from celestia_devtools.env import host as _host
from celestia_devtools.env import wsl_exec as _wsl_exec
from celestia_devtools.env import wsl_select as _wsl_select

# ── Project-specific passthrough env ─────────────────────────────────────────
PASSTHROUGH_ENV = {
    "EVERNIGHT_ROOT", "KEI_ROOT", "ARCH_BUSYBOX", "SENSOR_DATA_DIR",
}


def _project_root() -> Path:
    """The aris project root (parent of scripts/utils/)."""
    return Path(__file__).resolve().parent.parent.parent


# ── Re-exports (public API — do not rename, callers depend on these) ─────────
detect_host_kind = _host.detect_host_kind
host_machine = _host.host_machine

docker_cmd = _docker.docker_cmd
ensure_podman_socket = _docker.ensure_podman_socket

list_wsl_distros = _wsl_select.list_wsl_distros
select_distro = _wsl_select.select_distro
_probe_distro_tools = _wsl_select.probe_distro_tools
_summarise_tools = _wsl_select.summarise_tools
_summarise_container = _wsl_select.summarise_container


def wsl_main_guard(wsl_hint: str = "") -> bool:
    """Entry-point guard: on Windows, re-exec into WSL. No-op elsewhere."""
    return _wsl_exec.main_guard(
        project_root=_project_root(),
        passthrough_env=PASSTHROUGH_ENV,
        wsl_hint=wsl_hint,
    )
