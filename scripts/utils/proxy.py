"""Proxy configuration for network operations.

Reads HTTP_PROXY / HTTPS_PROXY from environment, defaults to
http://127.0.0.1:7890 (local dev proxy) if not set.

All scripts that perform network operations (git fetch, wget, cargo
download) should use get_proxy_env() to pass proxy settings to
subprocesses.
"""
from __future__ import annotations

import os

DEFAULT_PROXY = "http://127.0.0.1:7890"


def get_proxy_url() -> str:
    """Return the proxy URL from env or default."""
    return os.environ.get("HTTPS_PROXY") or os.environ.get("HTTP_PROXY") or DEFAULT_PROXY


def get_proxy_env() -> dict[str, str]:
    """Return an environment dict with proxy variables set.

    Merge with os.environ.copy() for subprocess calls:
        env = {**os.environ.copy(), **get_proxy_env()}
        subprocess.run([...], env=env)
    """
    proxy = get_proxy_url()
    return {
        "HTTP_PROXY": proxy,
        "HTTPS_PROXY": proxy,
        "http_proxy": proxy,
        "https_proxy": proxy,
        "ALL_PROXY": proxy,
        "all_proxy": proxy,
    }


def get_git_proxy_args() -> list[str]:
    """Return git -c args for proxy configuration."""
    proxy = get_proxy_url()
    return [
        "-c", f"http.proxy={proxy}",
        "-c", f"https.proxy={proxy}",
    ]
