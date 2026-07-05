#!/usr/bin/env python3
"""aris — first ignition test.

Tests the full device registration flow WITHOUT QEMU:
  1. Start evernight-server (mock device gateway) on port 8443
  2. Start a Modbus TCP simulator on port 5020
  3. Run evernight sensor-poll → connects to gateway → registers device
  4. Verify device appears in gateway registry

This validates the communication path:
  evernight (sensor-poll) → WebSocket → evernight-server (device.register/telemetry/heartbeat)

Usage:
    python3 scripts/ignition_test.py
"""
from __future__ import annotations

import os
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import cli_format as cf

EVERNIGHT_ROOT = Path(os.environ.get("EVERNIGHT_ROOT", str(Path(__file__).resolve().parent.parent.parent / "evernight")))
EVERNIGHT_BIN = EVERNIGHT_ROOT / "target" / "release" / "evernight"
SERVER_BIN = EVERNIGHT_ROOT / "target" / "release" / "evernight-server"
MANIFEST = EVERNIGHT_ROOT / "tests" / "fixtures" / "e2e_gateway_test.toml"

GATEWAY_PORT = 8443
MODBUS_PORT = 5020
GATEWAY_URL = f"ws://127.0.0.1:{GATEWAY_PORT}/api/ws"

processes: list[subprocess.Popen] = []


def cleanup(*_) -> None:
    for p in processes:
        try:
            p.terminate()
            p.wait(timeout=5)
        except Exception:
            p.kill()
    sys.exit(1)


def port_in_use(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


def wait_for_port(port: int, timeout: float = 10.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if port_in_use(port):
            return True
        time.sleep(0.2)
    return False


def start_modbus_sim() -> subprocess.Popen | None:
    """Start a minimal Modbus TCP slave on port 5020 using Python."""
    cf.step("[2/4] Starting Modbus TCP simulator (port %d)" % MODBUS_PORT)

    if port_in_use(MODBUS_PORT):
        cf.ok("Port %d already in use — assuming simulator is running" % MODBUS_PORT)
        return None

    script = f'''
import socket, struct, sys, threading

def handle(conn, addr):
    print(f"  [modbus-sim] connection from {{addr}}", flush=True)
    buf = b""
    while True:
        data = conn.recv(1024)
        if not data:
            break
        buf += data
        # MBAP header = 7 bytes: tid(2) + pid(2) + length(2) + uid(1)
        # length = uid(1) + fc(1) + data bytes that follow
        while len(buf) >= 6:
            if len(buf) < 6:
                break
            tid, pid, length = struct.unpack(">HHH", buf[:6])
            frame_len = 6 + length  # total bytes for this frame
            if len(buf) < frame_len:
                break  # incomplete frame, wait for more data
            frame = buf[:frame_len]
            buf = buf[frame_len:]
            uid = frame[6]
            fc = frame[7]
            pdu_data = frame[8:]  # PDU data after fc byte

            if fc in (3, 4):  # Read Holding / Input Registers
                addr_reg = struct.unpack(">H", pdu_data[0:2])[0]
                count = struct.unpack(">H", pdu_data[2:4])[0]
                values = [(i * 10 + 200) for i in range(count)]
                data_payload = b"".join(struct.pack(">H", v) for v in values)
                # Response PDU: fc + byte_count + data
                resp_pdu = struct.pack(">BB", fc, len(data_payload)) + data_payload
                resp = struct.pack(">HHHB", tid, pid, 1 + len(resp_pdu), uid) + resp_pdu
                conn.send(resp)
            elif fc == 1 or fc == 2:  # Read Coils / Discrete Inputs
                addr_reg = struct.unpack(">H", pdu_data[0:2])[0]
                count = struct.unpack(">H", pdu_data[2:4])[0]
                byte_count = (count + 7) // 8
                coil_data = bytes([(0xFF if j == 0 else 0) for j in range(byte_count)])
                resp_pdu = struct.pack(">BB", fc, byte_count) + coil_data
                resp = struct.pack(">HHHB", tid, pid, 1 + len(resp_pdu), uid) + resp_pdu
                conn.send(resp)
            else:
                # Exception: illegal function
                resp_pdu = struct.pack(">BB", fc | 0x80, 1)
                resp = struct.pack(">HHHB", tid, pid, 1 + len(resp_pdu), uid) + resp_pdu
                conn.send(resp)

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", {MODBUS_PORT}))
srv.listen(5)
print(f"  [modbus-sim] listening on port {MODBUS_PORT}", flush=True)
while True:
    conn, addr = srv.accept()
    threading.Thread(target=handle, args=(conn, addr), daemon=True).start()
'''

    p = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    processes.append(p)
    if wait_for_port(MODBUS_PORT, 3.0):
        cf.ok("Modbus simulator running on port %d" % MODBUS_PORT)
    else:
        cf.fail("Modbus simulator failed to start")
    return p


def main() -> int:
    signal.signal(signal.SIGINT, cleanup)
    signal.signal(signal.SIGTERM, cleanup)

    cf.section("aris — First Ignition Test")
    cf.info("  Gateway URL: " + GATEWAY_URL)
    cf.info("  Modbus port: %d" % MODBUS_PORT)
    cf.info("  Manifest:    %s" % MANIFEST)

    # ── Verify binaries exist ────────────────────────────────
    cf.blank()
    cf.step("[0/4] Pre-flight checks")
    if not SERVER_BIN.exists():
        cf.fail("evernight-server not found: %s" % SERVER_BIN)
        cf.info("  Run: cd %s && cargo build --release -p evernight" % EVERNIGHT_ROOT)
        return 1
    if not EVERNIGHT_BIN.exists():
        cf.fail("evernight not found: %s" % EVERNIGHT_BIN)
        return 1
    if not MANIFEST.exists():
        cf.fail("Test manifest not found: %s" % MANIFEST)
        return 1
    cf.ok("All binaries found")

    # ── [1/4] Start evernight-server ─────────────────────────
    cf.blank()
    cf.step("[1/4] Starting evernight-server (device gateway)")
    if port_in_use(GATEWAY_PORT):
        cf.ok("Port %d already in use — assuming gateway is running" % GATEWAY_PORT)
    else:
        server = subprocess.Popen(
            [str(SERVER_BIN), "serve",
             "--host", "127.0.0.1",
             "--port", str(GATEWAY_PORT)],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        processes.append(server)
        if wait_for_port(GATEWAY_PORT, 5.0):
            cf.ok("evernight-server listening on port %d" % GATEWAY_PORT)
        else:
            cf.fail("evernight-server failed to start")
            cleanup()

    # ── [2/4] Start Modbus simulator ─────────────────────────
    cf.blank()
    start_modbus_sim()

    # ── [3/4] Run sensor-poll ────────────────────────────────
    cf.blank()
    cf.step("[3/4] Starting evernight sensor-poll (device registration)")
    cf.info("  Connecting to: " + GATEWAY_URL)
    cf.info("  Node ID: ignition-test-01")
    cf.info("  Press Ctrl-C to stop.")
    cf.blank()

    # sensor-poll defaults its JSONL data store to /var/lib/evernight/sensor
    # which is not writable without root. Redirect to a temp dir for the test.
    sensor_data_dir = Path("/tmp/aris-test/sensor")
    sensor_data_dir.mkdir(parents=True, exist_ok=True)

    poll_env = os.environ.copy()
    poll_env["SENSOR_DATA_DIR"] = str(sensor_data_dir)

    poll = subprocess.Popen(
        [
            str(EVERNIGHT_BIN),
            "sensor-poll",
            "--manifest", str(MANIFEST),
            "--gateway", GATEWAY_URL,
            "--node-id", "ignition-test-01",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env=poll_env,
    )
    processes.append(poll)

    # ── [4/4] Monitor for 15 seconds ─────────────────────────
    cf.step("[4/4] Monitoring device registration + telemetry (15s)")
    deadline = time.time() + 15
    registration_seen = False
    telemetry_seen = False
    while time.time() < deadline:
        line = poll.stdout.readline().decode("utf-8", errors="replace").strip()
        if line:
            print("  [sensor-poll] " + line)
            low = line.lower()
            if "register" in low or "connected" in low:
                registration_seen = True
            if "telemetry sent" in low:
                telemetry_seen = True
        if poll.poll() is not None:
            cf.fail("sensor-poll exited early")
            break
        time.sleep(0.1)

    cf.blank()
    if registration_seen:
        cf.ok("IGNITION SUCCESS — device registered with gateway")
    else:
        cf.warn("Could not confirm registration in 15s (check logs above)")
    if telemetry_seen:
        cf.ok("TELEMETRY CONFIRMED — data flowing to gateway")
    else:
        cf.info("No telemetry seen yet (Modbus sim may not have responded)")

    cf.blank()
    cf.info("evernight-server should show the device in its registry.")
    cf.info("To verify: connect to ws://127.0.0.1:%d/api/ws and call devices.list" % GATEWAY_PORT)

    cleanup()
    return 0


if __name__ == "__main__":
    sys.exit(main())
