#!/usr/bin/env python3
"""
Test 2: USB gadget configfs script simulation.

Tests aris-usb-gadget against a mock configfs environment. The script
supports ARIS_CONFIGFS and ARIS_UDC_DIR env-var overrides, so we can redirect
all paths to a temp directory without modifying the script.
"""

import os
import shutil
import subprocess
import sys
import tempfile

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
GADGET_SCRIPT = os.path.join(SCRIPT_DIR, "..", "..", "overlay",
                            "nanopi-r3s", "usr", "sbin", "aris-usb-gadget")


class MockEnv:
    """Mock configfs + UDC environment."""

    def __init__(self):
        self.root = tempfile.mkdtemp(prefix="aris-gadget-")
        self.configfs = os.path.join(self.root, "config")
        self.udc_dir = os.path.join(self.root, "udc")
        self.gadget_dir = os.path.join(self.configfs, "usb_gadget", "aris_gadget")
        self.ms_file = os.path.join(self.root, "installer.img")

        os.makedirs(os.path.join(self.configfs, "usb_gadget"))
        os.makedirs(self.udc_dir)
        with open(os.path.join(self.udc_dir, "mock_udc"), "w") as f:
            f.write("mock_udc\n")
        with open(self.ms_file, "wb") as f:
            f.write(b"\x00" * 1024)

        self.base_env = {
            "ARIS_CONFIGFS": self.configfs,
            "ARIS_UDC_DIR": self.udc_dir,
            "GADGET_MS_FILE": self.ms_file,
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        }

    def run(self, args, extra=None):
        env = {**os.environ, **self.base_env}
        if extra:
            env.update(extra)
        return subprocess.run(
            ["sh", GADGET_SCRIPT] + args,
            capture_output=True, text=True, timeout=10, env=env,
        )

    def exists(self):
        return os.path.isdir(self.gadget_dir)

    def attr(self, name):
        p = os.path.join(self.gadget_dir, name)
        if os.path.isfile(p):
            with open(p) as f:
                return f.read().strip()
        return ""

    def functions(self):
        d = os.path.join(self.gadget_dir, "functions")
        return sorted(x for x in os.listdir(d) if os.path.isdir(os.path.join(d, x))) if os.path.isdir(d) else []

    def config_links(self):
        d = os.path.join(self.gadget_dir, "configs", "c.1")
        return sorted(x for x in os.listdir(d) if os.path.islink(os.path.join(d, x))) if os.path.isdir(d) else []

    def strings(self):
        d = os.path.join(self.gadget_dir, "strings", "0x409")
        out = {}
        if os.path.isdir(d):
            for k in ["manufacturer", "product", "serialnumber"]:
                p = os.path.join(d, k)
                if os.path.isfile(p):
                    with open(p) as f:
                        out[k] = f.read().strip()
        return out

    def cleanup(self):
        shutil.rmtree(self.root, ignore_errors=True)


def main():
    fails = []
    print("Test 2: USB Gadget configfs simulation")
    print("=" * 50)

    mock = MockEnv()
    try:
        # 2a: start
        print("\n[2a] Start...")
        r = mock.run(["start"], {
            "GADGET_VID": "0x1d6b", "GADGET_PID": "0x0104",
            "GADGET_MANUFACTURER": "test-celestia", "GADGET_PRODUCT": "Test GW",
            "GADGET_SERIAL": "T0001",
        })
        ok = r.returncode == 0
        if not ok:
            print(f"  stderr: {r.stderr[:300]}")
            fails.append("start failed")
        print(f"  [{'ok' if ok else '!!'}] exit={r.returncode}, gadget={'yes' if mock.exists() else 'no'}")

        # 2b: IDs
        print("\n[2b] USB IDs...")
        for name, want in [("idVendor","0x1d6b"),("idProduct","0x0104")]:
            got = mock.attr(name)
            ok = got == want
            if not ok:

                fails.append(f"{name}: {got}≠{want}")
            print(f"  [{'ok' if ok else '!!'}] {name}={got}")

        # 2c: strings
        print("\n[2c] Strings...")
        st = mock.strings()
        for k, want in [("manufacturer","test-celestia"),("product","Test GW"),("serialnumber","T0001")]:
            got = st.get(k,"")
            ok = got == want
            if not ok:

                fails.append(f"{k}: {got}≠{want}")
            print(f"  [{'ok' if ok else '!!'}] {k}={got}")

        # 2d: functions
        print("\n[2d] Functions...")
        fns = mock.functions()
        for fn in ["mass_storage.0", "ncm.0"]:
            ok = fn in fns
            if not ok:

                fails.append(f"missing {fn}")
            print(f"  [{'ok' if ok else '!!'}] {fn}")
        print(f"  all: {fns}")

        # 2e: mass storage backing
        print("\n[2e] MS backing...")
        p = os.path.join(mock.gadget_dir, "functions", "mass_storage.0", "lun.0", "file")
        if os.path.isfile(p):
            with open(p) as f:
                val = f.read().strip()
            ok = val == mock.ms_file
            if not ok:

                fails.append("ms file mismatch")
            print(f"  [{'ok' if ok else '!!'}] file={os.path.basename(val)}")
        else:
            fails.append("lun.0/file missing")
            print("  [!!] lun.0/file missing")

        # 2f: links
        print("\n[2f] Config links...")
        lks = mock.config_links()
        for fn in ["mass_storage.0", "ncm.0"]:
            ok = fn in lks
            if not ok:

                fails.append(f"{fn} not linked")
            print(f"  [{'ok' if ok else '!!'}] {fn}")

        # 2g: UDC
        print("\n[2g] UDC binding...")
        udc = mock.attr("UDC")
        ok = bool(udc)
        if not ok:

            fails.append("UDC not bound")
        print(f"  [{'ok' if ok else '!!'}] UDC={udc}")

        # 2h: status
        print("\n[2h] Status...")
        r = mock.run(["status"])
        for fn in ["mass_storage", "ncm"]:
            ok = fn in r.stdout
            if not ok:

                fails.append(f"status: {fn} missing")
            print(f"  [{'ok' if ok else '!!'}] mentions {fn}")

        # 2i: stop
        print("\n[2i] Stop...")
        r = mock.run(["stop"])
        ok = r.returncode == 0
        if not ok:

            fails.append("stop failed")
        print(f"  [{'ok' if ok else '!!'}] exit={r.returncode}")

        # 2j: idempotent start
        print("\n[2j] Idempotency...")
        mock.run(["start"])
        r2 = mock.run(["start"])
        ok = r2.returncode == 0 and mock.exists()
        if not ok:

            fails.append("restart failed")
        print(f"  [{'ok' if ok else '!!'}] restarted")

    finally:
        mock.cleanup()

    print("\n" + "=" * 50)
    if fails:
        print(f"FAIL: {len(fails)} issues:")
        for f in fails:
            print(f"  - {f}")
        return 1
    print("PASS: All gadget tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
