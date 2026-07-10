//! Hardware test — requires physical NanoPi R3S connected.
//!
//! Run with: just hw-test

#[test]
#[ignore = "requires physical NanoPi R3S"]
fn dual_ethernet_link_up() {
    // Verify eth0 and eth1 both report link up
}

#[test]
#[ignore = "requires physical NanoPi R3S"]
fn gpio_export_read() {
    // Verify GPIO control works via sysfs or gpiochip
}
