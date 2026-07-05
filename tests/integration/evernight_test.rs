//! evernight integration test (QEMU virtual hardware).
//!
//! Simulates Modbus devices via QEMU virtio-serial and
//! verifies evernight can read/write registers.

#[tokio::test]
#[ignore = "requires QEMU + prebuilt image"]
async fn modbus_read_register() {
    // TODO: start QEMU with Modbus simulator, verify evernight
    //       reads holding registers via TCP port forward
}

#[tokio::test]
#[ignore = "requires QEMU + prebuilt image"]
async fn evernight_registers_with_entelecheia() {
    // TODO: mock entelecheia server, verify device.register RPC
}
