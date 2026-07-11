// Minimal test: just write to stderr and exit
fn main() {
    // Write directly to fd 2 using raw syscall to avoid any Rust std overhead
    let msg = b"HELLO FROM KEI_UI\n";
    unsafe {
        let _ = libc::write(2, msg.as_ptr() as *const _, msg.len());
    }
    // Exit immediately
    std::process::exit(0);
}
