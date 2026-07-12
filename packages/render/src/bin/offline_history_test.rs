// Offline regression test for navigation history (back/forward/reload).
//
// Exercises BrowserState's history machinery directly: load three pages,
// then go back, forward, and back again, asserting the can_go_back /
// can_go_forward flags and the queued loads after each step. No window.
//
//   cargo run -p aris-render --features "desktop winit" --bin offline_history_test

use std::sync::Arc;

use aris_render::browser::BrowserState;

fn main() {
    aris_render::init_logging();
    let state = Arc::new(BrowserState::new());

    // about: loads run inline (no network), so history is populated synchronously.
    state.navigate_input("about:blank");
    state.navigate_input("about:about");
    state.navigate_input("data:text/html,<p>three</p>");

    // Each navigate_input queues a load; commit_load records history when the
    // loop processes them. Simulate the loop draining + committing.
    for load in state.drain_loads() {
        state.commit_load(load.url);
    }
    println!("current url: {:?}", state.current_url());
    println!(
        "can_go_back={} can_go_forward={}",
        state.can_go_back(),
        state.can_go_forward()
    );

    assert!(state.current_url().is_some(), "should have a current URL");
    assert!(
        state.can_go_back(),
        "should be able to go back after 3 loads"
    );
    assert!(
        !state.can_go_forward(),
        "should NOT be able to go forward at the tip"
    );

    // Go back once: should queue a load and enable forward.
    assert!(state.go_back(), "go_back should succeed");
    let loads = state.drain_loads();
    assert_eq!(loads.len(), 1, "go_back should queue exactly one load");
    println!("go_back queued: {}", loads[0].url);
    for load in loads {
        state.commit_load(load.url);
    }
    assert!(state.can_go_back(), "still able to go back");
    assert!(state.can_go_forward(), "now able to go forward");

    // Go forward: back to the tip.
    assert!(state.go_forward(), "go_forward should succeed");
    let loads = state.drain_loads();
    assert_eq!(loads.len(), 1, "go_forward should queue exactly one load");
    println!("go_forward queued: {}", loads[0].url);
    for load in loads {
        state.commit_load(load.url);
    }
    assert!(state.can_go_back(), "can go back again");
    assert!(!state.can_go_forward(), "at the tip, can't go forward");

    // Reload re-queues the current URL.
    assert!(state.reload(), "reload should succeed");
    let loads = state.drain_loads();
    assert_eq!(loads.len(), 1, "reload should queue exactly one load");
    println!("reload queued: {}", loads[0].url);

    // can_go_back at the very root (after going back to the start).
    while state.go_back() {
        for load in state.drain_loads() {
            state.commit_load(load.url);
        }
    }
    assert!(!state.can_go_back(), "at the root, can't go back");

    println!("OK: history back/forward/reload all work");
}
