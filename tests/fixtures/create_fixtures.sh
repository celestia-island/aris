#!/bin/bash
# ============================================================
#  Create fixture binaries for testing.
#
#  Each fixture is a REAL cross-compiled ELF/PE/Mach-O produced by
#  `cargo build` from the pure-Rust project in
#  tests/fixtures/evernight-fixture/ — which has zero C
#  dependencies and so cross-compiles cleanly with the
#  self-contained musl / rust-lld linker configuration in
#  .cargo/config.toml.
#
#  If a Rust target triple is not installed (e.g. the Apple/Windows
#  targets on a Linux CI host) the script falls back to the legacy
#  shell-script stub so installer tests still have something to run.
#
#  NOTE: the real evernight broker (../evernight) cannot be used as a
#  fixture because it links C libraries (libmodbus, libsqlite3, …)
#  that require a musl C cross-toolchain not present in CI. The
#  aris-core supervisor itself cross-compiles to aarch64 musl without
#  any C toolchain — see `scripts/build.py` and the PLAN.
# ============================================================

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE_SRC="$ROOT/tests/fixtures/evernight-fixture"
FIXTURES_DIR="${1:-$ROOT/tests/fixtures/evernight-binaries}"
mkdir -p "${FIXTURES_DIR}"

# Map a fixture directory name to a Rust target triple.
# Only targets whose `rustup target list --installed` includes them
# get a real cross-compiled binary; the rest fall back to the stub.
declare -a TARGETS=(
  "x86_64-pc-windows-gnu"
  "x86_64-unknown-linux-musl"
  "aarch64-unknown-linux-musl"
  "x86_64-apple-darwin"
  "aarch64-apple-darwin"
)

echo "Creating fixture binaries in ${FIXTURES_DIR}..."

have_target() {
  rustup target list --installed 2>/dev/null | grep -qx "$1"
}

write_stub() {
  local target="$1"
  local outfile="$2"
  mkdir -p "$(dirname "${outfile}")"
  cat > "${outfile}" <<EOF
#!/bin/sh
# evernight — fixture binary for testing (shell-script fallback)
# The Rust target '${target}' is not installed on this host, so no
# real cross-compiled binary was produced. This stub prints the same
# banner so installer tests still pass.
echo "evernight (fixture): \$*"
echo "  target: ${target}"
echo "  compiled: fixture-build (stub)"
exit 0
EOF
  chmod +x "${outfile}"
  echo "  [stub] ${outfile}"
}

build_real() {
  local target="$1"
  local ext=""
  if [[ "${target}" == *"windows"* ]]; then
    ext=".exe"
  fi
  local outdir="${FIXTURES_DIR}/${target}/release"
  local outfile="${outdir}/evernight${ext}"
  mkdir -p "${outdir}"

  echo "  [cargo] building for ${target}..."
  if FIXTURE_TARGET="${target}" cargo build \
      --manifest-path "${FIXTURE_SRC}/Cargo.toml" \
      --target "${target}" \
      --release \
      >/tmp/aris-fixture-build.log 2>&1; then
    # Locate the built binary (cargo puts it in the fixture src's target dir)
    local built="${FIXTURE_SRC}/target/${target}/release/evernight${ext}"
    if [[ -f "${built}" ]]; then
      cp "${built}" "${outfile}"
      chmod +x "${outfile}"
      echo "  [ok]   ${outfile} ($(stat -c%s "${outfile}" 2>/dev/null || stat -f%z "${outfile}") bytes)"
    else
      echo "  [warn] build ok but binary not found at ${built}; writing stub"
      write_stub "${target}" "${outfile}"
    fi
  else
    echo "  [warn] cargo build for ${target} failed (see /tmp/aris-fixture-build.log); writing stub"
    write_stub "${target}" "${outfile}"
  fi
}

for target in "${TARGETS[@]}"; do
  if have_target "${target}"; then
    build_real "${target}"
  else
    stub_ext=""
    [[ "${target}" == *"windows"* ]] && stub_ext=".exe"
    write_stub "${target}" "${FIXTURES_DIR}/${target}/release/evernight${stub_ext}"
  fi
done

echo ""
echo "Fixtures created successfully."
