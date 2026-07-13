// SPDX-License-Identifier: BUSL-1.1

//! Boa JS engine integration for aris-render.
//!
//! Provides minimal JavaScript execution for `<script>` tags in HTML.
//! Uses Boa (pure-Rust ECMAScript engine) to replace SpiderMonkey.

use std::collections::HashMap;

/// Result of executing page scripts.
#[derive(Debug, Default)]
pub struct JsExecResult {
    pub scripts_executed: usize,
    pub errors: Vec<String>,
    pub document_props: HashMap<String, String>,
    pub console_output: Vec<String>,
}

/// Extracts `<script>` blocks from HTML and executes them with Boa.
pub fn execute_scripts(html: &str) -> JsExecResult {
    let mut result = JsExecResult::default();
    let scripts = extract_script_blocks(html);
    result.scripts_executed = scripts.len();

    if scripts.is_empty() {
        return result;
    }

    let mut ctx = boa_engine::Context::default();

    // Set up document and console as global variables using simple eval
    let init_js = r#"
        var __doc_props = {};
        var document = {
            setTitle: function(t) { __doc_props.title = t; },
            write: function(html) { __doc_props.body = (__doc_props.body || "") + html; },
            title: ""
        };
        var console = {
            log: function() {
                var args = Array.prototype.slice.call(arguments);
                var msg = args.join(" ");
                if (typeof __doc_props !== 'undefined') {
                    __doc_props.__console = (__doc_props.__console || []);
                    __doc_props.__console.push(msg);
                }
            }
        };
    "#;

    if let Err(e) = ctx.eval(boa_engine::Source::from_bytes(init_js)) {
        result.errors.push(format!("Init error: {}", e));
        return result;
    }

    for (i, script) in scripts.iter().enumerate() {
        match ctx.eval(boa_engine::Source::from_bytes(script)) {
            Ok(_) => {
                tracing::debug!("Script {} executed ({} bytes)", i, script.len());
            }
            Err(e) => {
                let msg = format!("Script {} error: {}", i, e);
                tracing::warn!("{}", msg);
                result.errors.push(msg);
            }
        }
    }

    // Extract document properties
    if let Ok(val) = ctx.eval(boa_engine::Source::from_bytes(
        "JSON.stringify(__doc_props)"
    )) {
        let json_str = val.to_string(&mut ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default();
        if let Some(title) = extract_json_string(&json_str, "title") {
            result.document_props.insert("title".to_string(), title);
        }
        if let Some(body) = extract_json_string(&json_str, "body") {
            result.document_props.insert("body".to_string(), body);
        }
    }

    // Extract console output
    if let Ok(val) = ctx.eval(boa_engine::Source::from_bytes(
        "JSON.stringify(__doc_props.__console || [])"
    )) {
        let json_str = val.to_string(&mut ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default();
        result.console_output = parse_json_string_array(&json_str);
        for line in &result.console_output {
            eprintln!("[Boa console.log] {}", line);
        }
    }

    result
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    if let Some(start) = json.find(&pattern) {
        let rest = &json[start + pattern.len()..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

fn parse_json_string_array(json: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut in_string = false;
    let mut current = String::new();
    let mut escaped = false;

    for ch in json.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' && in_string {
            escaped = true;
        } else if ch == '"' {
            if in_string {
                result.push(std::mem::take(&mut current));
            }
            in_string = !in_string;
        } else if in_string {
            current.push(ch);
        }
    }

    result
}

/// Extract the source text of all inline `<script>` blocks from `html`.
/// Excludes external scripts (with `src=`). Public so other crates (e.g. the
/// persistent JS runtime) can run scripts themselves.
pub fn extract_scripts(html: &str) -> Vec<String> {
    extract_script_blocks(html)
}

fn extract_script_blocks(html: &str) -> Vec<String> {
    let mut scripts = Vec::new();
    let mut remaining = html;

    loop {
        let open = match remaining.find("<script") {
            Some(pos) => pos,
            None => break,
        };
        remaining = &remaining[open..];

        let tag_end = match remaining.find('>') {
            Some(pos) => pos,
            None => break,
        };
        remaining = &remaining[tag_end + 1..];

        let close = match remaining.find("</script>") {
            Some(pos) => pos,
            None => break,
        };

        let script_content = &remaining[..close];
        let trimmed = script_content.trim();
        if !trimmed.is_empty() {
            scripts.push(trimmed.to_string());
        }

        remaining = &remaining[close + 9..];
    }

    scripts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_script_blocks() {
        let html = r#"<html><body>
            <script>var x = 1;</script>
            <p>hello</p>
            <script>console.log("hi");</script>
        </body></html>"#;
        let scripts = extract_script_blocks(html);
        assert_eq!(scripts.len(), 2);
    }

    #[test]
    fn test_execute_simple_js() {
        let html = r#"<script>document.setTitle("Test Page")</script>"#;
        let result = execute_scripts(html);
        assert_eq!(result.scripts_executed, 1);
        assert!(result.errors.is_empty(), "Errors: {:?}", result.errors);
        assert_eq!(
            result.document_props.get("title"),
            Some(&"Test Page".to_string())
        );
    }

    #[test]
    fn test_execute_arithmetic() {
        let html = r#"<script>var result = 2 + 3; console.log(result.toString())</script>"#;
        let result = execute_scripts(html);
        assert_eq!(result.scripts_executed, 1);
        assert!(result.errors.is_empty(), "Errors: {:?}", result.errors);
        assert_eq!(result.console_output.len(), 1);
        assert!(result.console_output[0].contains("5"));
    }
}
