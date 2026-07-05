//! QEMU-based boot smoke test.
//!
//! Builds and boots the firmware image in QEMU arm64 virt machine.
//! Verifies that aris-core starts and evernight is visible.

#[tokio::test]
#[ignore = "requires QEMU + prebuilt image"]
async fn boot_smoke_test() {
    // TODO: spawn QEMU with the firmware image, wait for boot
    //       complete message on serial console, verify aris-core
    //       PID 1 is running
}

#[tokio::test]
#[ignore = "requires QEMU + prebuilt image"]
async fn evernight_daemon_starts() {
    // TODO: verify evernight daemon starts after aris-core init
}
