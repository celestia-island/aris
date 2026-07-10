//! evernight fixture — a minimal pure-Rust stand-in for the real
//! evernight protocol broker.
//!
//! The real evernight (https://github.com/celestia-island/evernight)
//! pulls in C libraries (libmodbus, libsqlite3, …) that require a
//! musl C cross-toolchain which is not available in every CI
//! environment. This fixture has **zero C dependencies**, so it
//! cross-compiles cleanly to every aris target triple via
//! `cargo build --target <triple>` with the self-contained musl
//! linker.
//!
//! The fixture prints the same banner the legacy shell-script stub
//! printed, so downstream installer tests that grep the output keep
//! working. It is deliberately tiny so the produced ELF is small
//! enough to commit as a test fixture.

use std::env;
use std::process::ExitCode;

const BANNER: &str = "\
evernight (fixture): a real cross-compiled ELF, not the production broker
  source:    tests/fixtures/evernight-fixture (pure Rust, no C deps)
  compiled:  fixture-build
";

fn main() -> ExitCode {
    let target = option_env!("FIXTURE_TARGET").unwrap_or("unknown");
    let args: Vec<String> = env::args().skip(1).collect();

    print!("{BANNER}");
    println!("  target:    {target}");
    println!("  args:      {}", args.join(" "));
    ExitCode::SUCCESS
}
