//! Interactive DOM↔Boa JS bridge for click handlers.
//!
//! When a click resolves to an element carrying an `onclick` attribute, the
//! handler runs in a Boa context with a `document` object and element handles
//! backed by the live blitz DOM. Supported operations:
//!
//!   - `document.getElementById(id)` → element handle (or null)
//!   - `document.createElement(tag)` → new element handle (not yet attached)
//!   - `document.querySelector(sel)` → element handle by tag / `#id` / `.cls`
//!   - `el.textContent = "..."` (set) and `el.setAttribute(name, value)`
//!   - `el.appendChild(child)` — attaches a created element under a live parent
//!
//! Implementation: Boa native functions cannot borrow `&mut BaseDocument`, so
//! they record `Op`s into a shared `Gc<GcRefCell<Bridge>>`. After the script
//! finishes, the ops are replayed against the live DOM. Element handles are JS
//! objects carrying an `_arisId` (a node id for live elements) and an optional
//! `_pending` key (for created-but-not-yet-appended elements).

#![cfg(feature = "js")]

use std::collections::HashMap;

use blitz_dom::{BaseDocument, local_name};
use boa_engine::{Context, JsObject, JsResult, JsValue, NativeFunction, Source};
use boa_gc::{Finalize, Gc, GcRefCell, Trace};

#[derive(Debug, Default)]
pub struct OnclickResult {
    pub executed: bool,
    pub dom_mutated: bool,
    pub errors: Vec<String>,
}

/// Shared bridge state captured by every Boa native closure.
#[derive(Default, Trace, Finalize)]
struct Bridge {
    /// Snapshot of element id → node id, taken at install time.
    ids: HashMap<String, u32>,
    /// Recorded DOM operations, replayed after the script.
    ops: Vec<Op>,
    /// Monotonic counter for created-element handles.
    next_pending: u32,
    /// Stashed created elements keyed by pending id: (tag, text, attrs).
    pending: HashMap<u32, (String, String, Vec<(String, String)>)>,
    /// querySelector index: tag/class/id → first node id.
    query_by_tag: HashMap<String, u32>,
    query_by_class: HashMap<String, u32>,
    query_by_id: HashMap<String, u32>,
}

#[derive(Clone, Debug, Trace, Finalize)]
enum Op {
    SetText {
        node_id: u32,
        value: String,
    },
    SetAttr {
        node_id: u32,
        name: String,
        value: String,
    },
    /// Append a created element (looked up by pending id) under parent node id.
    AppendChild {
        parent_id: u32,
        pending_id: u32,
    },
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

    let (by_tag, by_class, by_id_q) = collect_query_index(doc);
    let bridge: Gc<GcRefCell<Bridge>> = Gc::new(GcRefCell::new(Bridge {
        ids: collect_ids(doc),
        ops: Vec::new(),
        next_pending: 0,
        pending: HashMap::new(),
        query_by_tag: by_tag,
        query_by_class: by_class,
        query_by_id: by_id_q,
    }));
    let mut ctx = Context::default();

    if let Err(e) = install_document_global(&mut ctx, Gc::clone(&bridge)) {
        result.errors.push(format!("document setup: {e}"));
        return result;
    }

    // Rewrite the source so `.textContent = v` / `.style.cssText = v` map onto
    // the handle's methods (setText / setAttribute), which use the verified
    // NativeFunction path.
    let rewritten = rewrite_source(&src);
    match ctx.eval(Source::from_bytes(&rewritten)) {
        Ok(_) => {}
        Err(e) => result.errors.push(e.to_string()),
    }

    // Replay pending ops against the live DOM.
    let ops: Vec<Op> = { bridge.borrow().ops.clone() };
    let pending: HashMap<u32, (String, String, Vec<(String, String)>)> =
        { bridge.borrow().pending.clone() };
    let mut changed = false;
    for op in ops {
        if apply_op(doc, op, &pending) {
            changed = true;
        }
    }
    result.dom_mutated = changed;
    result
}

fn apply_op(
    doc: &mut BaseDocument,
    op: Op,
    pending: &HashMap<u32, (String, String, Vec<(String, String)>)>,
) -> bool {
    match &op {
        Op::SetText { node_id, value } => {
            set_text_content(doc, *node_id as usize, value);
            true
        }
        Op::SetAttr {
            node_id,
            name,
            value,
        } => {
            set_attribute(doc, *node_id as usize, name, value);
            true
        }
        Op::AppendChild {
            parent_id,
            pending_id,
        } => {
            if let Some((tag, text, attrs)) = pending.get(pending_id) {
                create_and_append(doc, *parent_id as usize, tag, text, attrs);
                true
            } else {
                false
            }
        }
    }
}

// ── Live-DOM helpers ───────────────────────────────────────

fn collect_ids(doc: &BaseDocument) -> HashMap<String, u32> {
    let mut r = HashMap::new();
    let tree = doc.tree();
    for (id, node) in tree.iter() {
        if let Some(id_attr) = node.attr(local_name!("id")) {
            r.insert(id_attr.to_string(), id as u32);
        }
    }
    r
}

fn set_text_content(doc: &mut BaseDocument, node_id: usize, value: &str) {
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
        let new_id = doc.create_text_node(value);
        if let Some(parent) = doc.get_node_mut(node_id) {
            parent.children.push(new_id);
        }
        if let Some(child) = doc.get_node_mut(new_id) {
            child.parent = Some(node_id);
        }
    }
}

fn set_attribute(doc: &mut BaseDocument, node_id: usize, name: &str, value: &str) {
    let qname = blitz_dom::QualName::new(None, blitz_dom::ns!(html), name.into());
    doc.mutate().set_attribute(node_id, qname, value);
}

fn create_and_append(
    doc: &mut BaseDocument,
    parent_id: usize,
    tag: &str,
    text: &str,
    attrs: &[(String, String)],
) {
    use blitz_dom::{ElementData, NodeData, QualName};
    let attrs_vec = attrs
        .iter()
        .map(|(k, v)| blitz_dom::Attribute {
            name: QualName::new(None, blitz_dom::ns!(html), k.as_str().into()),
            value: v.as_str().into(),
        })
        .collect();
    let el = ElementData::new(
        QualName::new(None, blitz_dom::ns!(html), tag.into()),
        attrs_vec,
    );
    let new_id = doc.create_node(NodeData::Element(el));
    if let Some(parent) = doc.get_node_mut(parent_id) {
        parent.children.push(new_id);
    }
    if let Some(child) = doc.get_node_mut(new_id) {
        child.parent = Some(parent_id);
    }
    if !text.is_empty() {
        let text_id = doc.create_text_node(text);
        if let Some(el) = doc.get_node_mut(new_id) {
            el.children.push(text_id);
        }
        if let Some(tn) = doc.get_node_mut(text_id) {
            tn.parent = Some(new_id);
        }
    }
}

// ── Boa global installation ────────────────────────────────

fn install_document_global(ctx: &mut Context, bridge: Gc<GcRefCell<Bridge>>) -> JsResult<()> {
    use boa_engine::object::ObjectInitializer;

    let get_by_id = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let id = arg_string(args, 0);
            if let Some(&nid) = b.borrow().ids.get(&id) {
                Ok(make_element_handle(ctx, Gc::clone(b), nid, None)?.into())
            } else {
                Ok(JsValue::null())
            }
        },
        Gc::clone(&bridge),
    );

    let create_el = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let tag = arg_string(args, 0);
            let pid = {
                let mut bb = b.borrow_mut();
                let pid = bb.next_pending;
                bb.next_pending += 1;
                bb.pending.insert(pid, (tag, String::new(), Vec::new()));
                pid
            };
            Ok(make_element_handle(ctx, Gc::clone(b), 0, Some(pid))?.into())
        },
        Gc::clone(&bridge),
    );

    let query_sel = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let sel = arg_string(args, 0);
            let bb = b.borrow();
            let nid = if let Some(id) = sel.strip_prefix('#') {
                bb.query_by_id.get(id).copied()
            } else if let Some(cls) = sel.strip_prefix('.') {
                bb.query_by_class.get(cls).copied()
            } else {
                bb.query_by_tag.get(&sel).copied()
            };
            drop(bb);
            if let Some(nid) = nid {
                Ok(make_element_handle(ctx, Gc::clone(b), nid, None)?.into())
            } else {
                Ok(JsValue::null())
            }
        },
        Gc::clone(&bridge),
    );

    let document = ObjectInitializer::new(ctx)
        .function(get_by_id, boa_engine::js_string!("getElementById"), 1)
        .function(create_el, boa_engine::js_string!("createElement"), 1)
        .function(query_sel, boa_engine::js_string!("querySelector"), 1)
        .build();

    let global = ctx.global_object();
    let _ = global.insert_property(
        boa_engine::js_string!("document"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(document)
            .writable(true)
            .enumerable(false)
            .configurable(true)
            .build(),
    );
    Ok(())
}

fn arg_string(args: &[JsValue], idx: usize) -> String {
    args.get(idx)
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_default()
}

/// Build three selector index maps (tag → first node id, class → first node id,
/// id → node id) for querySelector.
fn collect_query_index(
    doc: &BaseDocument,
) -> (
    HashMap<String, u32>,
    HashMap<String, u32>,
    HashMap<String, u32>,
) {
    let mut by_tag = HashMap::new();
    let mut by_class = HashMap::new();
    let mut by_id = HashMap::new();
    let tree = doc.tree();
    for (id, node) in tree.iter() {
        if let Some(el) = node.element_data() {
            let tag = format!("{:?}", el.name.local)
                .trim_start_matches("Atom('")
                .trim_end_matches("' type=static)")
                .to_string();
            by_tag.entry(tag).or_insert(id as u32);
            if let Some(cls) = node.attr(local_name!("class")) {
                for c in cls.split_whitespace() {
                    by_class.entry(c.to_string()).or_insert(id as u32);
                }
            }
        }
        if let Some(id_attr) = node.attr(local_name!("id")) {
            by_id.entry(id_attr.to_string()).or_insert(id as u32);
        }
    }
    (by_tag, by_class, by_id)
}

/// Build a JS element-handle object. Exposes methods (not accessors) so we
/// can use the verified ObjectInitializer::function(NativeFunction) path:
///   - setText(value)        → sets textContent
///   - setAttribute(n, v)
///   - appendChild(child)
/// The onclick source is rewritten (see `rewrite_source`) so
/// `el.textContent = 'x'` becomes `el.setText('x')`, etc.
fn make_element_handle(
    ctx: &mut Context,
    bridge: Gc<GcRefCell<Bridge>>,
    nid: u32,
    pending: Option<u32>,
) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;
    use boa_engine::property::Attribute;

    let mut init = ObjectInitializer::new(ctx);
    init.property(
        boa_engine::js_string!("_arisId"),
        JsValue::new(nid),
        Attribute::all(),
    );
    if let Some(pid) = pending {
        init.property(
            boa_engine::js_string!("_pending"),
            JsValue::new(pid),
            Attribute::all(),
        );
    }

    // setText(value)
    let set_text = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let value = arg_string(args, 0);
            let handle_id = read_handle_id(this);
            let pid = read_pending(this);
            if let Some(pid) = pid {
                if let Some(e) = b.borrow_mut().pending.get_mut(&pid) {
                    e.1 = value;
                }
            } else if let Some(nid) = handle_id {
                b.borrow_mut().ops.push(Op::SetText {
                    node_id: nid,
                    value,
                });
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(set_text, boa_engine::js_string!("setText"), 1);

    // setAttribute(name, value)
    let set_attr = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let name = arg_string(args, 0);
            let value = arg_string(args, 1);
            let handle_id = read_handle_id(this);
            let pid = read_pending(this);
            if let Some(pid) = pid {
                if let Some(e) = b.borrow_mut().pending.get_mut(&pid) {
                    e.2.push((name, value));
                }
            } else if let Some(nid) = handle_id {
                b.borrow_mut().ops.push(Op::SetAttr {
                    node_id: nid,
                    name,
                    value,
                });
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(set_attr, boa_engine::js_string!("setAttribute"), 2);

    // appendChild(child)
    let append = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let child = args.first().cloned().unwrap_or(JsValue::null());
            let parent_id = read_handle_id(this);
            let child_pending = read_pending(&child);
            if let (Some(parent_id), Some(child_pending)) = (parent_id, child_pending) {
                b.borrow_mut().ops.push(Op::AppendChild {
                    parent_id,
                    pending_id: child_pending,
                });
            }
            Ok(child)
        },
        Gc::clone(&bridge),
    );
    init.function(append, boa_engine::js_string!("appendChild"), 1);

    Ok(init.build())
}

/// Rewrite an onclick source so `el.textContent = v` becomes `el.setText(v)`,
/// and `el.style.cssText = v` becomes `el.setAttribute('style', v)`, so the
/// method-based handle API can service them.
fn rewrite_source(src: &str) -> String {
    let mut out = src.replace(".textContent =", ".setText(");
    // crude: replace the trailing of those assignments — setText(value) needs a
    // closing paren instead of the `;`. We do a second pass converting the
    // pattern `.setText( VALUE;` → `.setText(VALUE);`. This is best-effort.
    out = close_paren_after(&out, ".setText(");
    // style.cssText = v  →  setAttribute('style', v)
    out = rewrite_style_csstext(&out);
    out
}

/// After rewriting `.textContent = X` to `.setText(X`, close the call:
/// `.setText(X;` → `.setText(X);`. Handles single-line assignments.
fn close_paren_after(s: &str, marker: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(marker) {
        out.push_str(&rest[..pos + marker.len()]);
        let after = &rest[pos + marker.len()..];
        // Find the end of this statement (next `;` or newline).
        let end = after.find([';', '\n']).unwrap_or(after.len());
        out.push_str(&after[..end]);
        // Insert a closing paren before the `;`/newline.
        out.push(')');
        if end < after.len() {
            out.push_str(&after[end..end + 1]);
            rest = &after[end + 1..];
        } else {
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// Rewrite `.style.cssText = 'value'` → `.setAttribute('style','value')`.
fn rewrite_style_csstext(s: &str) -> String {
    let marker = ".style.cssText";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(marker) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + marker.len()..];
        let after_eq = after.trim_start();
        let after_eq = after_eq.strip_prefix('=').unwrap_or(after_eq);
        let after_eq = after_eq.trim_start();
        // Read a quoted value.
        if let Some((val, _tail)) = parse_quoted(after_eq) {
            out.push_str(&format!(".setAttribute('style','{}')", val));
        }
        rest = "";
    }
    out.push_str(rest);
    out
}

fn parse_quoted(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let q = *s.as_bytes().first()?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    let inner = &s[1..];
    let end = inner.find(q as char)?;
    Some((inner[..end].to_string(), &inner[end + 1..]))
}

fn read_handle_id(v: &JsValue) -> Option<u32> {
    let obj = v.as_object()?;
    let id = obj
        .get(boa_engine::js_string!("_arisId"), &mut Context::default())
        .ok()?;
    id.as_number().map(|n| n as u32).filter(|&n| n != 0)
}

fn read_pending(v: &JsValue) -> Option<u32> {
    let obj = v.as_object()?;
    let p = obj
        .get(boa_engine::js_string!("_pending"), &mut Context::default())
        .ok()?;
    p.as_number().map(|n| n as u32)
}
