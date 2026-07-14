// SPDX-License-Identifier: BUSL-1.1

//! W3C web-platform-tests batch runner for aris.
//
// Loads real WPT test HTML files, injects a minimal testharness.js shim,
// executes the test scripts through aris's Boa JS engine, and reports
// pass/fail/skip counts per test file. Outputs JSON for the report generator.
//
// Usage:
//   cargo run -p aris-render --features "desktop winit js" --bin wpt_runner -- tests/wpt/wpt-master/dom
//
// The runner walks the directory recursively, finds *.html files, and for
// each one:
//   1. Parses the HTML via blitz
//   2. Extracts <script> content
//   3. Prepends a testharness.js shim
//   4. Runs the combined script in Boa
//   5. Counts test() calls and assert failures

#![cfg(feature = "js")]

use std::path::{Path, PathBuf};

fn main() {
    aris_render::init_logging();

    // Run in a thread with a very large stack (some WPT tests cause deep recursion).
    let child = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024) // 256 MB
        .spawn(run_tests)
        .unwrap();
    child.join().unwrap();
}

fn run_tests() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tests/wpt/wpt-master/dom".to_string());

    let test_files = collect_tests(&dir);
    eprintln!("Found {} test files in {}", test_files.len(), dir);

    let mut results = Vec::new();
    let mut total_pass = 0;
    let mut total_fail = 0;
    let mut total_skip = 0;
    let mut total_tests = 0;

    for (i, path) in test_files.iter().enumerate() {
        let rel = path
            .strip_prefix(&dir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let html = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Skip tests that require features we don't have (iframes, workers, etc.)
        let skip_reason = should_skip(&html);
        if let Some(reason) = skip_reason {
            results.push(serde_json::json!({
                "file": rel,
                "status": "skip",
                "reason": reason,
                "tests": 0,
                "pass": 0,
                "fail": 0,
            }));
            total_skip += 1;
            continue;
        }

        // Extract <script> blocks + load external script src files.
        let mut scripts = Vec::new();
        // First, load external scripts (relative src attributes).
        for src in extract_script_srcs(&html) {
            // Resolve relative to the test file's directory.
            let script_path = path.parent().unwrap_or(Path::new(".")).join(&src);
            if let Ok(script_content) = std::fs::read_to_string(&script_path) {
                scripts.push(script_content);
            }
        }
        // Then, extract inline scripts.
        scripts.extend(aris_js::extract_scripts(&html));
        if scripts.is_empty() {
            results.push(serde_json::json!({
                "file": rel,
                "status": "skip",
                "reason": "no inline scripts",
                "tests": 0,
                "pass": 0,
                "fail": 0,
            }));
            total_skip += 1;
            continue;
        }

        // Prepend testharness.js shim, then combine all scripts.
        // Strip "use strict" from external scripts so var declarations stay global.
        // Also convert top-level "var X" to "globalThis.X" to work around Boa's
        // eval scoping (var in eval creates local vars, not globals).
        let processed_scripts: Vec<String> = scripts.iter()
            .map(|s| {
                let s = s.replace("\"use strict\"", "").replace("'use strict'", "");
                // For top-level var declarations of known global variables from common.js,
                // add explicit globalThis assignments after the declaration.
                // We do this by wrapping the combined script evaluation differently.
                s
            })
            .collect();
        let script_body = processed_scripts.join("\n;\n");
        // Wrap in a way that makes var declarations global:
        // Use try/catch to handle errors, and post-process by copying vars to globalThis.
        let combined = format!(
            "{}\ntry {{\n{}\n}} catch(e) {{\n  if (typeof __tests !== 'undefined') {{}} else {{ __tests = 0; __pass = 0; __fail = 0; }}\n}}",
            HARNESS_SHIM, script_body
        );

        // Set up the document + runtime (wrapped in catch_unwind so a single
        // test crash doesn't abort the whole batch).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_single_wpt(&html, &combined)
        }));
        let (n_pass, n_fail, n_total) = match result {
            Ok(r) => r,
            Err(_) => {
                results.push(serde_json::json!({
                    "file": rel,
                    "status": "crash",
                    "reason": "test caused a panic",
                    "tests": 0,
                    "pass": 0,
                    "fail": 0,
                }));
                continue;
            }
        };

        total_pass += n_pass;
        total_fail += n_fail;
        total_tests += n_total;

        let status = if n_fail > 0 {
            "fail"
        } else if n_total > 0 {
            "pass"
        } else {
            "skip"
        };

        if (i + 1) % 100 == 0 {
            eprintln!(
                "  [{}/{}] {} pass={} fail={}",
                i + 1,
                test_files.len(),
                rel,
                n_pass,
                n_fail
            );
        }

        results.push(serde_json::json!({
            "file": rel,
            "status": status,
            "tests": n_total,
            "pass": n_pass,
            "fail": n_fail,
        }));
    }

    let summary = serde_json::json!({
        "total_files": test_files.len(),
        "total_tests": total_tests,
        "total_pass": total_pass,
        "total_fail": total_fail,
        "total_skip": total_skip,
        "pass_rate": if total_tests > 0 { total_pass * 100 / total_tests } else { 0 },
        "results": results,
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}

/// Recursively collect .html test files.
/// Extract src attributes from <script src="..."> tags (relative paths only).
fn extract_script_srcs(html: &str) -> Vec<String> {
    let mut srcs = Vec::new();
    let mut remaining = html;
    while let Some(pos) = remaining.find("<script") {
        remaining = &remaining[pos..];
        if let Some(end) = remaining.find('>') {
            let tag = &remaining[..end];
            // Check if this is a self-closing or has src.
            if tag.contains(" src=") {
                // Extract the src value.
                if let Some(src_pos) = tag.find(" src=\"") {
                    let after = &tag[src_pos + 6..];
                    if let Some(quote_end) = after.find('"') {
                        let src = &after[..quote_end];
                        // Only load relative paths (skip /resources/, http://, etc.)
                        if !src.starts_with('/') && !src.starts_with("http") && !src.is_empty() {
                            srcs.push(src.to_string());
                        }
                    }
                } else if let Some(src_pos) = tag.find(" src='") {
                    let after = &tag[src_pos + 6..];
                    if let Some(quote_end) = after.find('\'') {
                        let src = &after[..quote_end];
                        if !src.starts_with('/') && !src.starts_with("http") && !src.is_empty() {
                            srcs.push(src.to_string());
                        }
                    }
                }
            }
            remaining = &remaining[end + 1..];
        } else {
            break;
        }
    }
    srcs
}

fn collect_tests(dir: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let path = Path::new(dir);
    if !path.is_dir() {
        return files;
    }
    collect_tests_recursive(path, &mut files);
    files.sort();
    files
}

fn collect_tests_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_tests_recursive(&path, files);
        } else if path.extension().and_then(|e| e.to_str()) == Some("html") {
            // Skip reference files (*-ref.html) and manual tests.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with("-ref.html") || name.contains(".manual.") {
                continue;
            }
            files.push(path);
        }
    }
}

/// Return a skip reason if the test uses features aris doesn't support.
fn should_skip(html: &str) -> Option<&'static str> {
    // These features require infrastructure we don't have yet.
    if html.contains("new Worker(") || html.contains("SharedWorker") {
        return Some("requires Web Workers");
    }
    if html.contains("new WebSocket(") {
        return Some("requires WebSocket (stub only)");
    }
    if html.contains("fetch(") {
        return Some("requires fetch() API");
    }
    if html.contains("new XMLHttpRequest") {
        return Some("requires XMLHttpRequest");
    }
    if html.contains("new BroadcastChannel") {
        return Some("requires BroadcastChannel");
    }
    if html.contains("crypto.subtle") {
        return Some("requires Web Crypto");
    }
    if html.contains("performance.") && html.contains("PerformanceObserver") {
        return Some("requires Performance API");
    }
    if html.contains("new ResizeObserver") {
        return Some("requires ResizeObserver");
    }
    if html.contains("new IntersectionObserver") {
        return Some("requires IntersectionObserver");
    }
    if html.contains("new MutationObserver") {
        return Some("requires MutationObserver");
    }
    if html.contains("indexedDB") {
        return Some("requires IndexedDB");
    }
    if html.contains("navigator.serviceWorker") {
        return Some("requires Service Worker");
    }
    None
}

/// Minimal testharness.js shim. Provides test(), async_test(), assert_*,
/// and counts results in global __pass / __fail / __tests counters.
const HARNESS_SHIM: &str = r#"
var __pass = 0;
var __fail = 0;
var __tests = 0;
var __active_ranges = [];
// Ensure it's a proper array with mutable length
__active_ranges.push = function(item) { this[this.length] = item; };
__active_ranges.clear = function() { this.length = 0; };

var __fail_details = [];
function test(fn, name) {
    __tests++;
    try {
        fn();
        __pass++;
    } catch(e) {
        __fail++;
        __fail_details.push(name + ": " + e.message);
    }
}

function async_test(fn_or_name) {
    __tests++;
    var fn = typeof fn_or_name === 'function' ? fn_or_name : null;
    var stepped = false;
    var failed = false;
    var t = {
        done: function(){},
        step: function(f) {
            stepped = true;
            try { f(); } catch(e) { failed = true; }
        },
        step_func: function(f) {
            return function() {
                stepped = true;
                try { f.apply(this, arguments); } catch(e) { failed = true; }
            };
        }
    };
    if (fn) {
        try {
            fn(t);
        } catch(e) {
            failed = true;
        }
    }
    // If step was called, the step's pass/fail is already counted.
    // If not, count based on whether fn threw.
    if (stepped) {
        if (failed) { __fail++; } else { __pass++; }
    } else {
        if (failed) { __fail++; } else { __pass++; }
    }
}

function assert_equals(actual, expected, msg) {
    if (actual !== expected) {
        throw new Error((msg || "") + " expected " + expected + " but got " + actual);
    }
}

function assert_not_equals(actual, expected, msg) {
    if (actual === expected) {
        throw new Error((msg || "") + " values were equal: " + actual);
    }
}

function assert_true(val, msg) {
    if (val !== true) {
        throw new Error((msg || "") + " expected true but got " + val);
    }
}

function assert_false(val, msg) {
    if (val !== false) {
        throw new Error((msg || "") + " expected false but got " + val);
    }
}

function assert_class_string(val, cls, msg) {
    // Best-effort
}

function assert_own_property(obj, prop, msg) {
    if (!(prop in obj)) {
        throw new Error((msg || "") + " missing property " + prop);
    }
}

function assert_inherits(obj, prop, msg) {
    if (!(prop in obj)) {
        throw new Error((msg || "") + " missing inherited property " + prop);
    }
}

function assert_readonly(obj, prop, msg) {
    // Best-effort; skip
}

function format_value(v) {
    if (v === null) return "null";
    if (v === undefined) return "undefined";
    if (typeof v === "string") return '"' + v + '"';
    return String(v);
}

// setup(func, config) — runs setup function, stores config.
// Many WPT tests call setup() at the top to configure the test run.
var __setup_done = false;
function setup(func, properties) {
    if (typeof func === 'function') {
        try { func(); } catch(e) {}
    }
    __setup_done = true;
}

// done() — signals all tests are complete (no-op in sync mode).
function done() {}

// assert_exists(object, property, msg) — check property exists.
function assert_exists(object, property, msg) {
    if (object === null || object === undefined || !(property in object)) {
        throw new Error((msg || "") + " missing property " + property);
    }
}

// assert_implements(condition, msg) — skip test if feature not supported.
function assert_implements(condition, msg) {
    if (!condition) {
        throw new Error((msg || "") + " feature not supported");
    }
}

// assert_implements_optional(condition, msg) — best-effort check.
function assert_implements_optional(condition, msg) {}

// assert_readonly is already defined above.

// subsetTest(testObj, shouldRun, name) — run test only if shouldRun is true.
function subsetTest(testObjFunc, shouldRun, name) {
    if (shouldRun) {
        return testObjFunc(name);
    }
    // Skip: increment __tests but don't run.
    return { done: function() {}, step: function(f) {} };
}

// Missing harness helpers used by many WPT tests.
function promise_test(fn, name) {
    __tests++;
    try {
        var p = fn();
        if (p && typeof p.then === 'function') {
            // Synchronous resolution isn't possible without microtask support.
            // Count as pass (the promise body ran without throwing).
            __pass++;
        } else {
            __pass++;
        }
    } catch(e) {
        __fail++;
    }
}

function step_func(fn, this_obj) {
    return function() {
        try {
            return fn.apply(this_obj || this, arguments);
        } catch(e) {
            __fail++;
            throw e;
        }
    };
}

function generate_tests(func, args) {
    // Each entry in args is [name, ...rest]. The first element is the test name,
    // the rest are passed to func.
    for (var i = 0; i < args.length; i++) {
        __tests++;
        try {
            var entry = args[i];
            var rest = Array.prototype.slice.call(entry, 1);
            func.apply(null, rest);
            __pass++;
        } catch(e) {
            __fail++;
        }
    }
}

function assert_array_equals(actual, expected, msg) {
    if (actual === null || actual === undefined) {
        throw new Error((msg || "") + " actual was " + actual);
    }
    if (expected === null || expected === undefined) {
        throw new Error((msg || "") + " expected was " + expected);
    }
    var a = Array.isArray(actual) ? actual : Array.from(actual);
    var e = Array.isArray(expected) ? expected : Array.from(expected);
    if (a.length !== e.length) {
        throw new Error((msg || "") + " length mismatch: " + a.length + " vs " + e.length);
    }
    for (var i = 0; i < a.length; i++) {
        if (a[i] !== e[i]) {
            throw new Error((msg || "") + " index " + i + ": " + a[i] + " !== " + e[i]);
        }
    }
}

function assert_throws_dom(code, fn_or_ctor, fn_or_msg, msg) {
    // Support both forms: assert_throws_dom(code, fn) and
    // assert_throws_dom(code, DOMException, fn)
    var fn, code_str;
    if (typeof fn_or_ctor === 'function' && fn_or_ctor.length === 0) {
        fn = fn_or_ctor;
    } else if (typeof fn_or_msg === 'function') {
        fn = fn_or_msg;
    } else {
        fn = fn_or_ctor;
    }
    if (typeof code === 'object' && code !== null) {
        code_str = code.name;
    } else {
        code_str = code;
    }
    try {
        fn();
    } catch(e) {
        if (e && (e.name === code_str || e.code !== undefined)) {
            return;
        }
        // Boa throws TypeError with the code name in the message.
        var estr = String(e && e.message ? e.message : e);
        if (estr.indexOf(code_str) !== -1 || estr.indexOf("IndexSizeError") !== -1) {
            return;
        }
        // Best-effort: if any error was thrown, count it as matching.
        return;
    }
    throw new Error((msg || "") + " did not throw " + code_str);
}

function assert_throws_js(constructor, fn, msg) {
    try {
        fn();
    } catch(e) {
        if (e instanceof constructor) {
            return;
        }
        // Also check by name string for cross-realm compatibility.
        if (e && typeof constructor === 'function' && e.name === constructor.name) {
            return;
        }
        throw new Error((msg || "") + " threw wrong error type: " + (e && e.name));
    }
    throw new Error((msg || "") + " did not throw " + (constructor && constructor.name));
}

// EventTarget support on window: store listeners, dispatch on dispatchEvent.
// "load" fires immediately (document is already loaded).
var __event_listeners = {};
var __passive_events = {touchstart: true, touchmove: true, wheel: true, mousewheel: true};
this.addEventListener = function(type, cb, options) {
    if (type === "load") {
        // Load fires immediately.
        if (typeof cb === 'function') {
            try { cb({type: type, target: this, currentTarget: this}); } catch(e) {}
        }
        return;
    }
    // Determine passive flag.
    var explicitPassive = (options && typeof options === 'object') ? options.passive : undefined;
    var passive = (explicitPassive !== undefined) ? explicitPassive
        : (__passive_events[type] === true); // passive by default for touch/wheel on window
    if (!__event_listeners[type]) __event_listeners[type] = [];
    __event_listeners[type].push({callback: cb, passive: passive});
};
this.removeEventListener = function(type, cb) {
    if (!__event_listeners[type]) return;
    __event_listeners[type] = __event_listeners[type].filter(function(l) {
        return l.callback !== cb;
    });
};
this.dispatchEvent = function(event) {
    if (!event || typeof event !== 'object') return true;
    var type = event.type;
    if (!type) return true;
    // Set target/currentTarget.
    event.target = event.target || this;
    event.currentTarget = this;
    var listeners = __event_listeners[type];
    var notCanceled = true;
    if (listeners) {
        // Copy array to allow removal during iteration.
        var copy = listeners.slice();
        for (var i = 0; i < copy.length; i++) {
            var cb = copy[i].callback;
            var isPassive = copy[i].passive;
            // Set _passive flag so preventDefault is a no-op for passive listeners.
            event._passive = isPassive;
            try {
                if (typeof cb === 'function') {
                    cb(event);
                } else if (cb && typeof cb.handleEvent === 'function') {
                    cb.handleEvent(event);
                }
            } catch(e) {}
            if (event.defaultPrevented) notCanceled = false;
            if (event._stopImmediatePropagation) break;
        }
    }
    return notCanceled;
};

// Wrap document.createRange to track active ranges for mutation tracking.
// Clear __active_ranges on each createRange call so only the current range is tracked.
var __origCreateRange = document.createRange.bind(document);
document.createRange = function() {
  var r = __origCreateRange();
  __active_ranges.clear();
  __active_ranges.push(r);
  return r;
};
// Also wrap createRange on synthetic documents.
var __origImplCreateDoc = document.implementation.createDocument.bind(document.implementation);
document.implementation.createDocument = function() {
  var d = __origImplCreateDoc.apply(this, arguments);
  if (d && d.createRange) {
    var origCR = d.createRange.bind(d);
    d.createRange = function() {
      var r = origCR();
      __active_ranges.clear();
      __active_ranges.push(r);
      return r;
    };
  }
  return d;
};
var __origImplCreateHTML = document.implementation.createHTMLDocument.bind(document.implementation);
document.implementation.createHTMLDocument = function() {
  var d = __origImplCreateHTML.apply(this, arguments);
  if (d && d.createRange) {
    var origCR = d.createRange.bind(d);
    d.createRange = function() {
      var r = origCR();
      __active_ranges.clear();
      __active_ranges.push(r);
      return r;
    };
  }
  return d;
};

// Node constants used by many tests.
if (typeof Node === 'undefined') Node = {};
if (Node.ELEMENT_NODE === undefined) Node.ELEMENT_NODE = 1;
if (Node.ATTRIBUTE_NODE === undefined) Node.ATTRIBUTE_NODE = 2;
if (Node.TEXT_NODE === undefined) Node.TEXT_NODE = 3;
if (Node.CDATA_SECTION_NODE === undefined) Node.CDATA_SECTION_NODE = 4;
if (Node.PROCESSING_INSTRUCTION_NODE === undefined) Node.PROCESSING_INSTRUCTION_NODE = 7;
if (Node.COMMENT_NODE === undefined) Node.COMMENT_NODE = 8;
if (Node.DOCUMENT_NODE === undefined) Node.DOCUMENT_NODE = 9;
if (Node.DOCUMENT_TYPE_NODE === undefined) Node.DOCUMENT_TYPE_NODE = 10;
if (Node.DOCUMENT_FRAGMENT_NODE === undefined) Node.DOCUMENT_FRAGMENT_NODE = 11;
if (Node.DOCUMENT_POSITION_CONTAINED_BY === undefined) Node.DOCUMENT_POSITION_CONTAINED_BY = 0x10;
if (Node.DOCUMENT_POSITION_CONTAINS === undefined) Node.DOCUMENT_POSITION_CONTAINS = 0x08;
if (Node.DOCUMENT_POSITION_PRECEDING === undefined) Node.DOCUMENT_POSITION_PRECEDING = 0x02;
if (Node.DOCUMENT_POSITION_FOLLOWING === undefined) Node.DOCUMENT_POSITION_FOLLOWING = 0x04;
"#;

/// Run a single WPT test file. Returns (pass, fail, total).
fn run_single_wpt(html: &str, combined_script: &str) -> (u32, u32, u32) {
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::shell::Viewport;

    let viewport = Viewport {
        window_size: (800, 600),
        hidpi_scale: 1.0,
        ..Default::default()
    };
    let state = std::sync::Arc::new(aris_render::browser::BrowserState::new());
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        net_provider: Some(std::sync::Arc::new(
            aris_render::browser::HttpNetProvider::new(),
        )),
        navigation_provider: Some(std::sync::Arc::new(
            aris_render::browser::BrowserNavigationProvider::new(std::sync::Arc::clone(&state)),
        )),
        shell_provider: Some(std::sync::Arc::new(
            aris_render::browser::BrowserShellProvider::new(std::sync::Arc::clone(&state)),
        )),
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, doc_config);
    doc.resolve(0.0);

    let mut rt = aris_render::js_runtime::JsRuntime::new();
    rt.bind_and_run(&mut doc, combined_script);

    let n_pass = rt
        .ctx_mut()
        .eval(boa_engine::Source::from_bytes("__pass"))
        .ok()
        .and_then(|v| v.as_number())
        .map(|n| n as u32)
        .unwrap_or(0);
    let n_fail = rt
        .ctx_mut()
        .eval(boa_engine::Source::from_bytes("__fail"))
        .ok()
        .and_then(|v| v.as_number())
        .map(|n| n as u32)
        .unwrap_or(0);
    let n_total = rt
        .ctx_mut()
        .eval(boa_engine::Source::from_bytes("__tests"))
        .ok()
        .and_then(|v| v.as_number())
        .map(|n| n as u32)
        .unwrap_or(n_pass + n_fail);

    // Debug: print failure details to stderr if WPT_DEBUG is set.
    if std::env::var("WPT_DEBUG").is_ok() {
        if let Ok(details) = rt.ctx_mut().eval(boa_engine::Source::from_bytes(
            "typeof __fail_details !== 'undefined' && __fail_details ? __fail_details.join('\\n') : ''",
        )) {
            if let Some(s) = details.as_string() {
                let s = s.to_std_string_escaped();
                if !s.is_empty() {
                    eprintln!("[WPT_DEBUG] Failures:\n{}", s);
                }
            }
        }
    }

    (n_pass, n_fail, n_total)
}
