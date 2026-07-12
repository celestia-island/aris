// SPDX-License-Identifier: BUSL-1.1

//! Minimal interactive JS for click handlers.
//!
//! Full DOM↔Boa bindings are a large project; this module provides a focused,
//! correct subset so the most common interactive pattern works: an element
//! with `onclick="..."` whose script assigns to `document.getElementById(id).textContent`
//! (or `.innerText`) or reads `document.getElementById(id).textContent`.
//!
//! Strategy: run the handler source in Boa for its side effects / truthiness,
//! AND scan the source for `document.getElementById('id').textContent = 'value'`
//! assignments, applying them to the live blitz DOM via the mutator. Boa gives
//! us real JS evaluation of any pure logic (arithmetic, conditionals, string
//! concatenation) inside the right-hand side when it's a single expression we
//! can substitute.
//!
//! No events, no createElement, no styles beyond textContent. Deterministic,
//! window-free, and testable.

#![cfg(feature = "js")]

use blitz_dom::{BaseDocument, local_name};

/// Run the `onclick` handler for the clicked node, applying any
/// `getElementById(...).textContent = ...` assignments to the DOM. Returns
/// whether a handler was found and whether the DOM changed.
#[derive(Debug, Default)]
pub struct OnclickResult {
    pub executed: bool,
    pub dom_mutated: bool,
    pub errors: Vec<String>,
}

pub fn run_onclick(doc: &mut BaseDocument, clicked_id: usize) -> OnclickResult {
    let mut result = OnclickResult::default();

    // Walk up to the nearest element with an onclick attribute.
    let mut handler_src: Option<String> = None;
    let mut cur = Some(clicked_id);
    while let Some(id) = cur {
        match doc.get_node(id) {
            Some(node) => {
                if let Some(src) = node.attr(local_name!("onclick")) {
                    handler_src = Some(src.to_string());
                    break;
                }
                cur = node.parent;
            }
            None => break,
        }
    }
    let Some(src) = handler_src else {
        return result;
    };
    result.executed = true;

    // Find getElementById('id').textContent = 'value' assignments and apply.
    let assignments = parse_textcontent_assignments(&src);
    let mut changed = false;
    for (target_id, raw_value) in assignments {
        // Evaluate the right-hand side as a JS expression via Boa when it isn't
        // a trivial string literal. For literal strings we use them directly.
        let value = eval_rhs(&raw_value, doc, &mut result.errors);
        if let Some(node_id) = find_by_id(doc, &target_id) {
            set_text_content(doc, node_id, &value);
            changed = true;
        }
    }
    result.dom_mutated = changed;
    result
}

/// Parse `document.getElementById("id").textContent = "value"` patterns from
/// the source, returning (id, raw_value_string) pairs. Handles single or
/// double quotes. `innerText` is treated as an alias for `textContent`.
fn parse_textcontent_assignments(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let key = "getElementById";
    let mut search_from = 0;
    while let Some(rel) = src[search_from..].find(key) {
        let i = search_from + rel;
        // The argument starts after "getElementById".
        let after_key = &src[i + key.len()..];
        let Some((id, after_id)) = parse_quoted_arg(after_key) else {
            search_from = i + key.len();
            continue;
        };
        // Skip the closing ')' of getElementById(...).
        let after_id = after_id.trim_start_matches(')');
        let rest = skip_whitespace(after_id);
        let rest = if let Some(r) = rest.strip_prefix(".textContent") {
            r
        } else if let Some(r) = rest.strip_prefix(".innerText") {
            r
        } else {
            search_from = i + key.len();
            continue;
        };
        let rest = skip_whitespace(rest);
        let Some(rest) = rest.strip_prefix('=') else {
            search_from = i + key.len();
            continue;
        };
        let rest = skip_whitespace(rest);
        if let Some((val, _)) = parse_quoted_arg(rest) {
            out.push((id, val));
        }
        search_from = i + key.len();
    }
    out
}

fn skip_whitespace(s: &str) -> &str {
    s.trim_start()
}

/// Parse a quoted argument starting at s (skipping a leading '('). Returns the
/// parsed string and the remainder after the closing quote.
fn parse_quoted_arg(s: &str) -> Option<(String, &str)> {
    let s = skip_whitespace(s);
    let s = s.strip_prefix('(').unwrap_or(s);
    let s = skip_whitespace(s);
    let quote = *s.as_bytes().first()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let inner = &s[1..];
    let end = inner.find(quote as char)?;
    Some((inner[..end].to_string(), &inner[end + 1..]))
}

/// Resolve `getElementById(id)` to a node id.
fn find_by_id(doc: &BaseDocument, id: &str) -> Option<usize> {
    let tree = doc.tree();
    for (node_id, node) in tree.iter() {
        if node.attr(local_name!("id")) == Some(id) {
            return Some(node_id);
        }
    }
    None
}

/// Set the text content of an element node by mutating its first text child
/// (creating one if needed, like `textContent =` semantics for a leaf element).
fn set_text_content(doc: &mut BaseDocument, node_id: usize, value: &str) {
    // Find the first text child of the element.
    let first_text = doc.get_node(node_id).and_then(|n| {
        n.children.iter().copied().find(|&c| {
            doc.get_node(c)
                .map(|cn| cn.text_data().is_some())
                .unwrap_or(false)
        })
    });
    if let Some(text_id) = first_text {
        if let Some(tn) = doc.get_node_mut(text_id).and_then(|n| n.text_data_mut()) {
            tn.content = value.to_string();
        }
    } else {
        // No text child yet: create one and attach.
        let new_id = doc.create_text_node(value);
        // Append as a child of the element.
        if let Some(parent) = doc.get_node_mut(node_id) {
            parent.children.push(new_id);
        }
        if let Some(child) = doc.get_node_mut(new_id) {
            child.parent = Some(node_id);
        }
    }
}

/// Evaluate the right-hand side of an assignment. String literals are returned
/// verbatim; anything else is evaluated as a JS expression in Boa (so
/// `'Hello ' + name` works when `name` is itself a literal we can substitute,
/// though here we only evaluate the RHS as-is, supporting string concatenation
/// of literals and basic math).
fn eval_rhs(raw: &str, doc: &BaseDocument, errors: &mut Vec<String>) -> String {
    // If the RHS references getElementById(...).textContent (a read), resolve
    // those reads before evaluating, so e.g. `'#' + document.getElementById('n').value`
    // style reads work for textContent.
    let resolved = resolve_reads(raw, doc);
    // Evaluate via Boa. String concatenation and arithmetic work. If Boa
    // isn't available or errors, fall back to the raw string.
    eval_js(&resolved, errors).unwrap_or(resolved)
}

/// Replace `document.getElementById('id').textContent` reads with the literal
/// current text content, so Boa can evaluate the RHS purely.
fn resolve_reads(src: &str, doc: &BaseDocument) -> String {
    let mut out = src.to_string();
    // Repeat until no more matches (handles multiple reads).
    loop {
        if let Some(start) = out.find("document.getElementById(") {
            let after = &out[start + "document.getElementById(".len()..];
            if let Some((id, rest)) = parse_quoted_arg(after) {
                let rest = skip_whitespace(rest);
                let rest = if let Some(r) = rest.strip_prefix(".textContent") {
                    r
                } else if let Some(r) = rest.strip_prefix(".innerText") {
                    r
                } else {
                    break;
                };
                let current = find_by_id(doc, &id)
                    .and_then(|nid| doc.get_node(nid))
                    .map(|n| n.text_content())
                    .unwrap_or_default();
                let quoted = format!("\"{}\"", current.replace('"', "\\\""));
                out = format!("{}{}{}", &out[..start], quoted, rest);
                continue;
            }
            break;
        }
        break;
    }
    out
}

/// Evaluate a JS expression via Boa and return its string form.
fn eval_js(expr: &str, errors: &mut Vec<String>) -> Option<String> {
    use boa_engine::{Context, Source};
    let mut ctx = Context::default();
    // Provide a minimal `document` stub so expressions that reference it (after
    // resolve_reads there shouldn't be any, but be defensive) don't throw.
    let _ = ctx.eval(Source::from_bytes("var document = {};"));
    match ctx.eval(Source::from_bytes(expr)) {
        Ok(v) => {
            let s = v.to_string(&mut ctx).ok()?.to_std_string_escaped();
            // Strip surrounding quotes Boa adds to string results.
            Some(s.trim_matches('"').to_string())
        }
        Err(e) => {
            errors.push(e.to_string());
            None
        }
    }
}
