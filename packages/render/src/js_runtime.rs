// SPDX-License-Identifier: BUSL-1.1

//! Persistent Boa JS runtime for a document, supporting `addEventListener`.
//!
//! The runtime owns a single Boa [`Context`] that lives for the document's
//! lifetime, so listeners registered during `<script>` execution (and element
//! handles from `getElementById`) survive to be fired on click.
//!
//! Listener handlers are stashed as named globals on the context (e.g.
//! `__aris_listener_<node>_<idx>`), which keeps them GC-rooted without us
//! holding raw `JsFunction`s across calls. The registry maps node id → list of
//! global names.
//!
//! Scope: `addEventListener('click', fn)`, `getElementById`, `createElement`,
//! `appendChild`, `setAttribute`, `textContent` — the same bridge operations as
//! `js_interactive`, but persistent so `<script>`-registered listeners fire.

#![cfg(feature = "js")]

use std::collections::HashMap;

use boa_engine::property::Attribute;

// Thread-local canvas buffer map: Boa closures can't capture non-Trace types,
// so canvas buffers are accessed via this thread_local. The render loop is
// single-threaded, so this is safe.
thread_local! {
    pub(crate) static CANVASES: std::cell::RefCell<HashMap<u32, crate::canvas::Canvas2D>> =
        std::cell::RefCell::new(HashMap::new());
    static NEXT_CANVAS_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Allocate a new canvas id and create the buffer.
fn alloc_canvas(width: u32, height: u32) -> u32 {
    NEXT_CANVAS_ID.with(|c| {
        let id = c.get();
        c.set(id + 1);
        CANVASES.with(|cs| {
            cs.borrow_mut()
                .insert(id, crate::canvas::Canvas2D::new(width, height));
        });
        id
    })
}

use blitz_dom::{BaseDocument, local_name};
use boa_engine::{Context, JsObject, JsResult, JsValue, NativeFunction, Source};
use boa_gc::{Finalize, Gc, GcRefCell, Trace};

/// A persistent JS runtime bound to one document.
pub struct JsRuntime {
    ctx: Context,
    /// node id → list of global function names registered as listeners.
    listeners: HashMap<u32, Vec<String>>,
    /// Shared bridge state used while a script or listener runs.
    bridge: Gc<GcRefCell<Bridge>>,
    /// Monotonic counter for listener global names.
    next_listener: u32,
    /// Pending timers (setTimeout/setInterval). Each has a fire time, source,
    /// and optional repeat interval.
    timers: Vec<Timer>,
    /// Monotonic counter for timer global names.
    next_timer: u32,
}

/// A pending timer.
struct Timer {
    fire_at: std::time::Instant,
    source: String,
    interval: Option<std::time::Duration>,
}

#[derive(Default, Trace, Finalize)]
struct Bridge {
    ids: HashMap<String, u32>,
    ops: Vec<Op>,
    next_pending: u32,
    pending: HashMap<u32, (String, String, Vec<(String, String)>)>,
    query_by_tag: HashMap<String, u32>,
    query_by_class: HashMap<String, u32>,
    query_by_id: HashMap<String, u32>,
    /// Snapshot of node properties: node_id → (tag_name, text_content, id_attr, class_attr, node_type)
    node_props: HashMap<u32, NodePropSnapshot>,
    new_listeners: Vec<(u32, String)>,
    new_timers: Vec<(String, u64, Option<u64>)>,
}

/// Snapshot of a blitz node's properties for JS-side access.
#[derive(Clone, Trace, Finalize)]
struct NodePropSnapshot {
    tag_name: String,
    text_content: String,
    id: String,
    class_name: String,
    node_type: u32,
    attrs: Vec<(String, String)>,
    /// Child node IDs (in document order).
    child_ids: Vec<u32>,
    /// Parent node ID (0 = no parent / root).
    parent_id: u32,
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
    AppendChild {
        parent_id: u32,
        pending_id: u32,
    },
}

impl JsRuntime {
    pub fn new() -> Self {
        let mut ctx = Context::default();
        let bridge = Gc::new(GcRefCell::new(Bridge::default()));
        let _ = install_document(&mut ctx, Gc::clone(&bridge));
        install_console(&mut ctx);
        install_window(&mut ctx, String::new());
        install_timers(&mut ctx, &bridge);
        install_event_api(&mut ctx);
        install_dom_globals(&mut ctx);
        install_webrtc_stubs(&mut ctx);
        Self {
            ctx,
            listeners: HashMap::new(),
            bridge,
            next_listener: 0,
            timers: Vec::new(),
            next_timer: 0,
        }
    }

    /// (Re)bind this runtime to a fresh document: rebuild the id snapshot, the
    /// query index, clear listeners, and (re)install the document global. Then
    /// run the given `<script>` source, applying any recorded ops and harvesting
    /// newly-registered listeners.
    pub fn bind_and_run(&mut self, doc: &mut BaseDocument, script_src: &str) {
        self.bind_and_run_with_url(doc, script_src, "")
    }

    /// As [`bind_and_run`] but also installs/reinstalls the `window` global
    /// with `location.href` set to the current document URL.
    pub fn bind_and_run_with_url(&mut self, doc: &mut BaseDocument, script_src: &str, url: &str) {
        // Drain pending ops from any prior run first (none expected here).
        self.apply_ops(doc);
        // Refresh the id snapshot + query index by rebuilding the bridge.
        let (by_tag, by_class, by_id_q) = collect_query_index(doc);
        let doc_snapshot = collect_node_props(doc);
        {
            let mut b = self.bridge.borrow_mut();
            b.ids = collect_ids(doc);
            b.node_props = doc_snapshot.clone();
            b.query_by_tag = by_tag;
            b.query_by_class = by_class;
            b.query_by_id = by_id_q;
            b.ops.clear();
            b.pending.clear();
            b.next_pending = 0;
        }
        self.listeners.clear();
        self.timers.clear();
        // Reinstall the document global (id snapshot now lives in the bridge).
        let _ = install_document(&mut self.ctx, Gc::clone(&self.bridge));
        install_window(&mut self.ctx, url.to_string());

        // Populate document.documentElement / body / head with real element
        // handles from the bridge snapshots.
        self.populate_doc_elements(&doc_snapshot);

        let _ = url;
        if !script_src.trim().is_empty() {
            let _ = self.ctx.eval(Source::from_bytes(script_src));
        }
        self.apply_ops(doc);
        self.harvest_listeners();
        self.harvest_timers();
    }

    /// Fire any click listeners attached to `node_id`. Each listener function
    /// is invoked with the current element handle as `this`. Recorded ops are
    /// applied after all listeners run.
    pub fn fire_click(&mut self, doc: &mut BaseDocument, node_id: u32) {
        self.apply_ops(doc);

        // 1. Inline onclick attribute: evaluate it in the runtime.
        let onclick_src = doc.get_node(node_id as usize).and_then(|n| {
            n.attr(blitz_dom::local_name!("onclick"))
                .map(|s| s.to_string())
        });
        if let Some(src) = onclick_src {
            self.bind_and_run(doc, &src);
        }

        // 2. Registered addEventListener listeners.
        let names = match self.listeners.get(&node_id) {
            Some(n) => n.clone(),
            None => {
                self.apply_ops(doc);
                self.harvest_listeners();
                self.harvest_timers();
                return;
            }
        };
        for name in names {
            // Look up the stashed global function and call it with a fresh
            // element handle as `this`.
            let this =
                match make_element_handle(&mut self.ctx, Gc::clone(&self.bridge), node_id, None) {
                    Ok(o) => o,
                    Err(_) => continue,
                };
            let key = boa_engine::js_string!(name.clone());
            let func_val = match self.ctx.global_object().get(key.clone(), &mut self.ctx) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(func) = func_val.as_object() else {
                continue;
            };
            if !func.is_callable() {
                continue;
            }
            let _ = func.call(&JsValue::from(this), &[], &mut self.ctx);
        }
        self.apply_ops(doc);
        self.harvest_listeners();
        self.harvest_timers();
    }

    /// Move pending ops from the bridge into the document.
    fn apply_ops(&self, doc: &mut BaseDocument) {
        let ops: Vec<Op> = { self.bridge.borrow().ops.clone() };
        let pending: HashMap<u32, (String, String, Vec<(String, String)>)> =
            { self.bridge.borrow().pending.clone() };
        for op in ops {
            apply_op(doc, op, &pending);
        }
    }

    /// Pull newly-registered listeners out of the bridge into self.listeners.
    /// The handler objects were already stashed as named globals during
    /// addEventListener; here we just record node → name.
    fn harvest_listeners(&mut self) {
        let new = std::mem::take(&mut self.bridge.borrow_mut().new_listeners);
        for (node_id, name) in new {
            self.listeners.entry(node_id).or_default().push(name);
        }
    }

    /// Move pending timers from the bridge into self.timers, setting their fire
    /// time relative to now.
    fn harvest_timers(&mut self) {
        let new = std::mem::take(&mut self.bridge.borrow_mut().new_timers);
        let now = std::time::Instant::now();
        for (name, delay_ms, interval_ms) in new {
            let delay = std::time::Duration::from_millis(delay_ms);
            let interval = interval_ms.map(std::time::Duration::from_millis);
            self.timers.push(Timer {
                fire_at: now + delay,
                source: name,
                interval,
            });
        }
    }

    /// Fire any expired timers. Returns true if any timer fired (DOM may have
    /// changed). setInterval timers are rescheduled.
    pub fn poll_timers(&mut self, doc: &mut BaseDocument) -> bool {
        if self.timers.is_empty() {
            return false;
        }
        let now = std::time::Instant::now();
        let mut fired = false;
        let mut reschedule = Vec::new();
        // Collect expired timers (drain in reverse to keep indices valid).
        let mut i = 0;
        while i < self.timers.len() {
            if self.timers[i].fire_at <= now {
                let timer = self.timers.remove(i);
                // Look up and call the stashed global function.
                let key = boa_engine::js_string!(timer.source.clone());
                let func_val = self.ctx.global_object().get(key, &mut self.ctx);
                if let Ok(v) = func_val
                    && let Some(func) = v.as_object()
                    && func.is_callable()
                {
                    let _ = func.call(&JsValue::undefined(), &[], &mut self.ctx);
                    fired = true;
                }
                // Reschedule if this was a setInterval.
                if let Some(interval) = timer.interval {
                    reschedule.push(Timer {
                        fire_at: now + interval,
                        source: timer.source,
                        interval: Some(interval),
                    });
                }
            } else {
                i += 1;
            }
        }
        self.timers.extend(reschedule);
        // Apply any ops the timer callbacks recorded.
        if fired {
            self.apply_ops(doc);
            self.harvest_listeners();
            self.harvest_timers();
            self.harvest_timers();
        }
        fired
    }

    pub fn ctx_mut(&mut self) -> &mut Context {
        &mut self.ctx
    }

    pub fn bridge(&self) -> &Gc<GcRefCell<Bridge>> {
        &self.bridge
    }

    /// Find html/body/head elements in node_props and populate document's
    /// documentElement/body/head properties with real element handles.
    fn populate_doc_elements(&mut self, snapshot: &HashMap<u32, NodePropSnapshot>) {
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };

        // Find html, body, head node IDs.
        let mut html_id: Option<u32> = None;
        let mut head_id: Option<u32> = None;
        let mut body_id: Option<u32> = None;
        for (&nid, s) in snapshot.iter() {
            let tag_lower = s.tag_name.to_lowercase();
            if tag_lower == "html" && html_id.is_none() {
                html_id = Some(nid);
            } else if tag_lower == "head" && head_id.is_none() {
                head_id = Some(nid);
            } else if tag_lower == "body" && body_id.is_none() {
                body_id = Some(nid);
            }
        }

        let doc_val = self
            .ctx
            .global_object()
            .get(boa_engine::js_string!("document"), &mut self.ctx)
            .ok();
        let Some(doc_obj) = doc_val.as_ref().and_then(|v| v.as_object()).cloned() else {
            return;
        };

        let bridge = Gc::clone(&self.bridge);

        // Helper: create + populate handle for a node.
        let mk = |nid: u32, snap: &NodePropSnapshot, ctx: &mut Context| {
            let handle =
                make_element_handle(ctx, Gc::clone(&bridge), nid, None).unwrap_or_else(|_| {
                    boa_engine::object::JsObject::with_object_proto(ctx.intrinsics())
                });
            populate_props(&handle, snap, ctx);
            handle.into()
        };

        if let Some(nid) = html_id {
            if let Some(s) = snapshot.get(&nid) {
                let handle = mk(nid, s, &mut self.ctx);
                let _ =
                    doc_obj.insert_property(boa_engine::js_string!("documentElement"), pd(handle));
            }
        }
        if let Some(nid) = head_id {
            if let Some(s) = snapshot.get(&nid) {
                let handle = mk(nid, s, &mut self.ctx);
                let _ = doc_obj.insert_property(boa_engine::js_string!("head"), pd(handle));
            }
        }
        if let Some(nid) = body_id {
            if let Some(s) = snapshot.get(&nid) {
                let handle = mk(nid, s, &mut self.ctx);
                let _ = doc_obj.insert_property(boa_engine::js_string!("body"), pd(handle));
            }
        }
    }

    /// Count red pixels across all canvas buffers (for testing).
    /// Count canvases that have content (for testing).
    pub fn canvas_has_content(&self) -> bool {
        CANVASES.with(|cs| cs.borrow().values().any(|c| c.has_content()))
    }
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn apply_op(
    doc: &mut BaseDocument,
    op: Op,
    pending: &HashMap<u32, (String, String, Vec<(String, String)>)>,
) {
    match &op {
        Op::SetText { node_id, value } => set_text_content(doc, *node_id as usize, value),
        Op::SetAttr {
            node_id,
            name,
            value,
        } => set_attribute(doc, *node_id as usize, name, value),
        Op::AppendChild {
            parent_id,
            pending_id,
        } => {
            if let Some((tag, text, attrs)) = pending.get(pending_id) {
                create_and_append(doc, *parent_id as usize, tag, text, attrs);
            }
        }
    }
}

// ── DOM helpers (shared with js_interactive) ───────────────

fn collect_ids(doc: &BaseDocument) -> HashMap<String, u32> {
    let mut r = HashMap::new();
    for (id, node) in doc.tree().iter() {
        if let Some(id_attr) = node.attr(local_name!("id")) {
            r.insert(id_attr.to_string(), id as u32);
        }
    }
    r
}

/// Snapshot all node properties into a map for JS-side access.
fn collect_node_props(doc: &BaseDocument) -> HashMap<u32, NodePropSnapshot> {
    let mut r = HashMap::new();
    for (id, node) in doc.tree().iter() {
        let tag = node
            .element_data()
            .map(|e| {
                format!("{:?}", e.name.local)
                    .trim_start_matches("Atom('")
                    .trim_end_matches("' type=static)")
                    .to_uppercase()
            })
            .unwrap_or_default();
        let text = node.text_content();
        let id_attr = node.attr(local_name!("id")).unwrap_or("").to_string();
        let class = node.attr(local_name!("class")).unwrap_or("").to_string();
        let node_type = if node.element_data().is_some() {
            1
        } else if node.text_data().is_some() {
            3
        } else {
            0
        };
        let attrs: Vec<(String, String)> = node
            .element_data()
            .map(|e| {
                e.attrs
                    .iter()
                    .map(|a| (a.name.local.to_string(), a.value.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let child_ids: Vec<u32> = node.children.iter().map(|&c| c as u32).collect();
        let parent_id = node.parent.map(|p| p as u32).unwrap_or(0);
        r.insert(
            id as u32,
            NodePropSnapshot {
                tag_name: tag,
                text_content: text,
                id: id_attr,
                class_name: class,
                node_type,
                attrs,
                child_ids,
                parent_id,
            },
        );
    }
    r
}

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
    for (id, node) in doc.tree().iter() {
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

// ── Boa document/handle installation ───────────────────────

/// Strip HTML tags from a string, leaving plain text.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn arg_string(args: &[JsValue], idx: usize) -> String {
    args.get(idx)
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_default()
}

/// Populate a JS element handle with real properties from a node snapshot.
fn populate_props(obj: &JsObject, s: &NodePropSnapshot, ctx: &mut Context) {
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let s_str = |v: &str| JsValue::from(boa_engine::js_string!(v.to_string()));

    let _ = obj.insert_property(boa_engine::js_string!("tagName"), pd(s_str(&s.tag_name)));
    let _ = obj.insert_property(boa_engine::js_string!("nodeName"), pd(s_str(&s.tag_name)));
    let _ = obj.insert_property(
        boa_engine::js_string!("localName"),
        pd(s_str(&s.tag_name.to_lowercase())),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("textContent"),
        pd(s_str(&s.text_content)),
    );
    let _ = obj.insert_property(boa_engine::js_string!("data"), pd(s_str(&s.text_content)));
    let _ = obj.insert_property(boa_engine::js_string!("id"), pd(s_str(&s.id)));
    let _ = obj.insert_property(
        boa_engine::js_string!("className"),
        pd(s_str(&s.class_name)),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("nodeType"),
        pd(JsValue::from(s.node_type)),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("length"),
        pd(JsValue::from(s.text_content.chars().count() as u32)),
    );

    // Store all attributes as JS properties.
    for (k, v) in &s.attrs {
        let _ = obj.insert_property(boa_engine::js_string!(k.clone()), pd(s_str(v)));
    }

    // Build childNodes array with real child handles.
    let child_arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    for (i, &cid) in s.child_ids.iter().enumerate() {
        // Create a lightweight handle for each child — we can't call
        // make_element_handle here (it needs the bridge), so create a
        // minimal object with _arisId and basic properties.
        let child_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ =
            child_obj.insert_property(boa_engine::js_string!("_arisId"), pd(JsValue::from(cid)));
        // Set nodeType based on whether it's a text node (we don't have the
        // snapshot here, but tag_name empty means text node).
        let _ = child_obj.insert_property(
            boa_engine::js_string!("_childIndex"),
            pd(JsValue::from(i as u32)),
        );
        let _ = child_arr.insert_property(i as u32, pd(child_obj.into()));
    }
    let _ = obj.insert_property(boa_engine::js_string!("childNodes"), pd(child_arr.into()));
    let _ = obj.insert_property(
        boa_engine::js_string!("childElementCount"),
        pd(JsValue::from(s.child_ids.len() as u32)),
    );
    // firstChild / lastChild
    if let Some(&first) = s.child_ids.first() {
        let fc = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = fc.insert_property(boa_engine::js_string!("_arisId"), pd(JsValue::from(first)));
        let _ = obj.insert_property(boa_engine::js_string!("firstChild"), pd(fc.into()));
    } else {
        let _ = obj.insert_property(boa_engine::js_string!("firstChild"), pd(JsValue::null()));
    }
    if let Some(&last) = s.child_ids.last() {
        let lc = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = lc.insert_property(boa_engine::js_string!("_arisId"), pd(JsValue::from(last)));
        let _ = obj.insert_property(boa_engine::js_string!("lastChild"), pd(lc.into()));
    } else {
        let _ = obj.insert_property(boa_engine::js_string!("lastChild"), pd(JsValue::null()));
    }

    // parentNode / parentElement
    if s.parent_id > 0 {
        let pn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = pn.insert_property(
            boa_engine::js_string!("_arisId"),
            pd(JsValue::from(s.parent_id)),
        );
        let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(pn.clone().into()));
        let _ = obj.insert_property(boa_engine::js_string!("parentElement"), pd(pn.into()));
    } else {
        let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
        let _ = obj.insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
    }
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

#[allow(clippy::too_many_arguments)]
/// Install a `window` global with `location` (href getter) and `alert`.
/// Install `setTimeout` and `setInterval`. The callback is stashed as a named
/// global and the timer metadata is recorded in the bridge's `new_timers` for
/// the runtime to harvest and poll.
fn install_timers(ctx: &mut Context, bridge: &Gc<GcRefCell<Bridge>>) {
    let set_timeout = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let Some(cb) = args.first().and_then(|v| v.as_object()) else {
                return Ok(JsValue::from(0));
            };
            let delay = args
                .get(1)
                .and_then(|v| v.as_number())
                .map(|n| n.max(0.0) as u64)
                .unwrap_or(0);
            // Stash the callback as a named global so it survives past this call.
            let name = {
                let mut bb = b.borrow_mut();
                bb.next_pending += 1;
                format!("__aris_timer_{}", bb.next_pending)
            };
            let _ = ctx.global_object().insert_property(
                boa_engine::js_string!(name.clone()),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(cb.clone()))
                    .enumerable(false)
                    .writable(true)
                    .configurable(true)
                    .build(),
            );
            b.borrow_mut().new_timers.push((name, delay, None));
            Ok(JsValue::from(0))
        },
        Gc::clone(bridge),
    );

    let set_interval = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let Some(cb) = args.first().and_then(|v| v.as_object()) else {
                return Ok(JsValue::from(0));
            };
            let delay = args
                .get(1)
                .and_then(|v| v.as_number())
                .map(|n| n.max(1.0) as u64)
                .unwrap_or(1);
            let name = {
                let mut bb = b.borrow_mut();
                bb.next_pending += 1;
                format!("__aris_timer_{}", bb.next_pending)
            };
            let _ = ctx.global_object().insert_property(
                boa_engine::js_string!(name.clone()),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(cb.clone()))
                    .enumerable(false)
                    .writable(true)
                    .configurable(true)
                    .build(),
            );
            b.borrow_mut().new_timers.push((name, delay, Some(delay)));
            Ok(JsValue::from(0))
        },
        Gc::clone(bridge),
    );

    let global = ctx.global_object();
    let _ = global.insert_property(
        boa_engine::js_string!("setTimeout"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_timeout).build())
            .writable(true)
            .enumerable(false)
            .configurable(true)
            .build(),
    );
    let _ = global.insert_property(
        boa_engine::js_string!("setInterval"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_interval).build(),
            )
            .writable(true)
            .enumerable(false)
            .configurable(true)
            .build(),
    );
}

fn install_window(ctx: &mut Context, url: String) {
    use boa_engine::object::ObjectInitializer;

    // Build the location object with an href property.
    let location = ObjectInitializer::new(ctx)
        .property(
            boa_engine::js_string!("href"),
            JsValue::from(boa_engine::js_string!(url.as_str())),
            Attribute::all(),
        )
        .build();

    // alert(message) — logs the message (a real modal would block; we trace).
    let alert = NativeFunction::from_copy_closure_with_captures(
        |_this, args, _caps, ctx| {
            let msg = args
                .first()
                .and_then(|v| v.to_string(ctx).ok().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            tracing::info!("[js alert] {}", msg);
            Ok(JsValue::undefined())
        },
        (),
    );

    let window = ObjectInitializer::new(ctx)
        .property(
            boa_engine::js_string!("location"),
            location,
            Attribute::all(),
        )
        .function(alert, boa_engine::js_string!("alert"), 1)
        .build();

    let global = ctx.global_object();
    let _ = global.insert_property(
        boa_engine::js_string!("window"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(window)
            .writable(true)
            .enumerable(false)
            .configurable(true)
            .build(),
    );
}

/// Install a `console` global with `log`/`warn`/`error`/`info` methods that
/// Install the Event API: Event constructor, CustomEvent, EventTarget,
/// and the dispatchEvent method on element handles.
fn install_event_api(ctx: &mut Context) {
    // Event constructor: new Event(type, {bubbles, cancelable, composed})
    let event_ctor = NativeFunction::from_copy_closure(|_this, args, ctx| {
        let type_ = arg_string(args, 0);
        let bubbles = args
            .get(1)
            .and_then(|o| o.as_object())
            .and_then(|o| o.get(boa_engine::js_string!("bubbles"), ctx).ok())
            .and_then(|v| v.as_boolean())
            .unwrap_or(false);
        let cancelable = args
            .get(1)
            .and_then(|o| o.as_object())
            .and_then(|o| o.get(boa_engine::js_string!("cancelable"), ctx).ok())
            .and_then(|v| v.as_boolean())
            .unwrap_or(false);
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = obj.insert_property(
            boa_engine::js_string!("type"),
            pd(JsValue::from(boa_engine::js_string!(type_))),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("bubbles"),
            pd(JsValue::from(bubbles)),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("cancelable"),
            pd(JsValue::from(cancelable)),
        );
        let _ = obj.insert_property(boa_engine::js_string!("composed"), pd(JsValue::from(false)));
        let _ = obj.insert_property(
            boa_engine::js_string!("defaultPrevented"),
            pd(JsValue::from(false)),
        );
        let _ = obj.insert_property(boa_engine::js_string!("target"), pd(JsValue::null()));
        let _ = obj.insert_property(boa_engine::js_string!("currentTarget"), pd(JsValue::null()));
        let _ = obj.insert_property(boa_engine::js_string!("timeStamp"), pd(JsValue::from(0u32)));
        let _ = obj.insert_property(
            boa_engine::js_string!("isTrusted"),
            pd(JsValue::from(false)),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("eventPhase"),
            pd(JsValue::from(0u32)),
        );
        // Methods
        let noop_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("preventDefault"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_fn).build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let noop_fn2 = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("stopPropagation"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_fn2).build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let noop_fn3 = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("stopImmediatePropagation"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_fn3).build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let init_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("initEvent"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), init_fn).build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("Event"), 1, event_ctor);

    // CustomEvent constructor: extends Event with detail.
    let custom_ctor = NativeFunction::from_copy_closure(|_this, args, ctx| {
        // Build a base Event first, then add detail.
        let type_ = arg_string(args, 0);
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = obj.insert_property(
            boa_engine::js_string!("type"),
            pd(JsValue::from(boa_engine::js_string!(type_))),
        );
        let _ = obj.insert_property(boa_engine::js_string!("bubbles"), pd(JsValue::from(false)));
        let _ = obj.insert_property(
            boa_engine::js_string!("cancelable"),
            pd(JsValue::from(false)),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("defaultPrevented"),
            pd(JsValue::from(false)),
        );
        let _ = obj.insert_property(boa_engine::js_string!("target"), pd(JsValue::null()));
        let _ = obj.insert_property(
            boa_engine::js_string!("detail"),
            pd(args.get(1).cloned().unwrap_or(JsValue::null())),
        );
        let noop_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("preventDefault"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_fn).build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let noop_fn2 = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("stopPropagation"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_fn2).build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("CustomEvent"), 1, custom_ctor);

    // EventTarget constructor: returns an object with addEventListener,
    // removeEventListener, and dispatchEvent.
    let et_ctor = NativeFunction::from_copy_closure(|_this, _args, ctx| {
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        // addEventListener
        let add_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("addEventListener"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), add_fn).build(),
            )),
        );
        // removeEventListener
        let rm_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("removeEventListener"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), rm_fn).build(),
            )),
        );
        // dispatchEvent
        let disp_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(true)));
        let _ = obj.insert_property(
            boa_engine::js_string!("dispatchEvent"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), disp_fn).build(),
            )),
        );
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("EventTarget"), 0, et_ctor);

    // Event constants on the Event constructor object itself.
    // (Already set as properties on each Event instance above.)
}

/// Install DOM-level globals: DOMException, document.implementation,
/// document.createTextNode, document.createComment, document.createDocumentFragment,
/// instanceof constructors, and other DOM infrastructure.
fn install_dom_globals(ctx: &mut Context) {
    fn pd(val: JsValue) -> boa_engine::property::PropertyDescriptor {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    }

    // DOMException constructor — creates a throwable error object.
    let dom_exc_ctor = NativeFunction::from_copy_closure(|_this, args, ctx| {
        let msg = arg_string(args, 0);
        let name = args
            .get(1)
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_else(|| "Error".to_string());
        let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = obj.insert_property(
            boa_engine::js_string!("name"),
            pd(JsValue::from(boa_engine::js_string!(name))),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("message"),
            pd(JsValue::from(boa_engine::js_string!(msg))),
        );
        let _ = obj.insert_property(boa_engine::js_string!("code"), pd(JsValue::from(0u32)));
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("DOMException"), 2, dom_exc_ctor);

    // DOMException static constants.
    let dom_exc_obj = ctx
        .global_object()
        .get(boa_engine::js_string!("DOMException"), &mut *ctx)
        .ok();
    if let Some(de) = dom_exc_obj.as_ref().and_then(|v| v.as_object()) {
        for (name, code) in [
            ("INDEX_SIZE_ERR", 1u32),
            ("DOMSTRING_SIZE_ERR", 2),
            ("HIERARCHY_REQUEST_ERR", 3),
            ("WRONG_DOCUMENT_ERR", 4),
            ("INVALID_CHARACTER_ERR", 5),
            ("NO_DATA_ALLOWED_ERR", 6),
            ("NO_MODIFICATION_ALLOWED_ERR", 7),
            ("NOT_FOUND_ERR", 8),
            ("NOT_SUPPORTED_ERR", 9),
            ("INUSE_ATTRIBUTE_ERR", 10),
            ("INVALID_STATE_ERR", 11),
            ("SYNTAX_ERR", 12),
            ("INVALID_MODIFICATION_ERR", 13),
            ("NAMESPACE_ERR", 14),
            ("INVALID_ACCESS_ERR", 15),
            ("VALIDATION_ERR", 16),
            ("TYPE_MISMATCH_ERR", 17),
            ("SECURITY_ERR", 18),
            ("NETWORK_ERR", 19),
            ("ABORT_ERR", 20),
            ("URL_MISMATCH_ERR", 21),
            ("QUOTA_EXCEEDED_ERR", 22),
            ("TIMEOUT_ERR", 23),
            ("INVALID_NODE_TYPE_ERR", 24),
            ("DATA_CLONE_ERR", 25),
        ] {
            let _ = de.insert_property(boa_engine::js_string!(name), pd(JsValue::from(code)));
        }
    }

    // document.implementation — object with hasFeature (always true), createDocument, createHTMLDocument.
    let impl_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let has_feature = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(true)));
    let _ = impl_obj.insert_property(
        boa_engine::js_string!("hasFeature"),
        pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), has_feature).build(),
        )),
    );
    let create_doc = NativeFunction::from_copy_closure(|_t, _a, ctx| {
        // Return a minimal document object.
        let d = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        Ok(d.into())
    });
    let _ = impl_obj.insert_property(
        boa_engine::js_string!("createDocument"),
        pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_doc).build(),
        )),
    );
    let create_html_doc = NativeFunction::from_copy_closure(|_t, _a, ctx| {
        let d = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        Ok(d.into())
    });
    let _ = impl_obj.insert_property(
        boa_engine::js_string!("createHTMLDocument"),
        pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_html_doc).build(),
        )),
    );
    let create_dt = NativeFunction::from_copy_closure(|_t, _a, ctx| {
        let d = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        Ok(d.into())
    });
    let _ = impl_obj.insert_property(
        boa_engine::js_string!("createDocumentType"),
        pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_dt).build(),
        )),
    );

    // Set document.implementation on the document global.
    let doc = ctx
        .global_object()
        .get(boa_engine::js_string!("document"), &mut *ctx)
        .ok();
    if let Some(doc_obj) = doc.as_ref().and_then(|v| v.as_object()) {
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("implementation"),
            pd(impl_obj.into()),
        );

        // document.createTextNode
        let create_text = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let text = arg_string(args, 0);
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ =
                obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32))); // TEXT_NODE
            let _ = obj.insert_property(
                boa_engine::js_string!("data"),
                pd(JsValue::from(boa_engine::js_string!(text.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                pd(JsValue::from(boa_engine::js_string!(text.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeValue"),
                pd(JsValue::from(boa_engine::js_string!(text))),
            );
            let _ = obj.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeName"),
                pd(JsValue::from(boa_engine::js_string!("#text"))),
            );
            let _ =
                obj.insert_property(boa_engine::js_string!("tagName"), pd(JsValue::undefined()));
            // CharacterData methods
            for (mname, mfn) in build_character_data_methods() {
                let _ = obj.insert_property(
                    boa_engine::js_string!(mname),
                    pd(JsValue::from(
                        boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), mfn).build(),
                    )),
                );
            }
            // setText for consistency
            let set_text = NativeFunction::from_copy_closure(|this, args, ctx| {
                let v = arg_string(args, 0);
                if let Some(o) = this.as_object() {
                    let _ = o.insert_property(
                        boa_engine::js_string!("data"),
                        pd(JsValue::from(boa_engine::js_string!(v.clone()))),
                    );
                    let _ = o.insert_property(
                        boa_engine::js_string!("textContent"),
                        pd(JsValue::from(boa_engine::js_string!(v))),
                    );
                }
                Ok(JsValue::undefined())
            });
            let _ = obj.insert_property(
                boa_engine::js_string!("setText"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_text).build(),
                )),
            );
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createTextNode"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_text).build(),
            )),
        );

        // document.createComment
        let create_comment = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let text = arg_string(args, 0);
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ =
                obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(8u32))); // COMMENT_NODE
            let _ = obj.insert_property(
                boa_engine::js_string!("data"),
                pd(JsValue::from(boa_engine::js_string!(text))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeName"),
                pd(JsValue::from(boa_engine::js_string!("#comment"))),
            );
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createComment"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_comment).build(),
            )),
        );

        // document.createDocumentFragment
        let create_frag = NativeFunction::from_copy_closure(|_t, _a, ctx| {
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ =
                obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(11u32))); // DOCUMENT_FRAGMENT_NODE
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeName"),
                pd(JsValue::from(boa_engine::js_string!("#document-fragment"))),
            );
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createDocumentFragment"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_frag).build(),
            )),
        );

        // document.documentElement, document.body, document.head — these are
        // populated lazily by the bridge's node_props snapshots. We install
        // getters that look up the bridge for html/body/head nodes.
        // For now, set them to null; they'll be overwritten by populate_doc_props
        // in bind_and_run if the document has the relevant elements.
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("documentElement"),
            pd(JsValue::null()),
        );
        let _ = doc_obj.insert_property(boa_engine::js_string!("body"), pd(JsValue::null()));
        let _ = doc_obj.insert_property(boa_engine::js_string!("head"), pd(JsValue::null()));
        // document.URL / document.documentURI
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("URL"),
            pd(JsValue::from(boa_engine::js_string!("about:blank"))),
        );
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("documentURI"),
            pd(JsValue::from(boa_engine::js_string!("about:blank"))),
        );
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("compatMode"),
            pd(JsValue::from(boa_engine::js_string!("CSS1Compat"))),
        );
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("contentType"),
            pd(JsValue::from(boa_engine::js_string!("text/html"))),
        );
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("characterSet"),
            pd(JsValue::from(boa_engine::js_string!("UTF-8"))),
        );
    }

    // Global constructors for instanceof checks: Node, Element, Text, Comment, DocumentFragment.
    for (name, nt) in [
        ("Node", 0u32),
        ("Element", 1),
        ("Text", 3),
        ("Comment", 8),
        ("DocumentFragment", 11),
    ] {
        let ctor_fn = NativeFunction::from_copy_closure(move |_t, _a, ctx| {
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(nt)));
            Ok(obj.into())
        });
        let _ = ctx.register_global_callable(boa_engine::js_string!(name), 0, ctor_fn);
    }
}

/// Build CharacterData methods as a list of (name, NativeFunction).
fn build_character_data_methods() -> Vec<(&'static str, NativeFunction)> {
    let append = NativeFunction::from_copy_closure(|this, args, ctx| {
        let v = arg_string(args, 0);
        if let Some(o) = this.as_object() {
            let old = o
                .get(boa_engine::js_string!("data"), ctx)
                .unwrap_or(JsValue::undefined());
            let old_str = old
                .as_string()
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let new = format!("{}{}", old_str, v);
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = o.insert_property(
                boa_engine::js_string!("data"),
                pd(JsValue::from(boa_engine::js_string!(new.clone()))),
            );
            let _ = o.insert_property(
                boa_engine::js_string!("textContent"),
                pd(JsValue::from(boa_engine::js_string!(new))),
            );
        }
        Ok(JsValue::undefined())
    });
    let delete_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        if let Some(o) = this.as_object() {
            let old = o
                .get(boa_engine::js_string!("data"), ctx)
                .unwrap_or(JsValue::undefined());
            let old_str = old
                .as_string()
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let mut chars: Vec<char> = old_str.chars().collect();
            let end = (offset + count).min(chars.len());
            chars.drain(offset..end);
            let new: String = chars.into_iter().collect();
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = o.insert_property(
                boa_engine::js_string!("data"),
                pd(JsValue::from(boa_engine::js_string!(new.clone()))),
            );
            let _ = o.insert_property(
                boa_engine::js_string!("textContent"),
                pd(JsValue::from(boa_engine::js_string!(new))),
            );
        }
        Ok(JsValue::undefined())
    });
    let insert_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let data = arg_string(args, 1);
        if let Some(o) = this.as_object() {
            let old = o
                .get(boa_engine::js_string!("data"), ctx)
                .unwrap_or(JsValue::undefined());
            let old_str = old
                .as_string()
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let off = offset.min(old_str.len());
            let new = format!("{}{}{}", &old_str[..off], data, &old_str[off..]);
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = o.insert_property(
                boa_engine::js_string!("data"),
                pd(JsValue::from(boa_engine::js_string!(new.clone()))),
            );
            let _ = o.insert_property(
                boa_engine::js_string!("textContent"),
                pd(JsValue::from(boa_engine::js_string!(new))),
            );
        }
        Ok(JsValue::undefined())
    });
    let replace_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let data = arg_string(args, 2);
        if let Some(o) = this.as_object() {
            let old = o
                .get(boa_engine::js_string!("data"), ctx)
                .unwrap_or(JsValue::undefined());
            let old_str = old
                .as_string()
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let mut chars: Vec<char> = old_str.chars().collect();
            let end = (offset + count).min(chars.len());
            let data_chars: Vec<char> = data.chars().collect();
            chars.splice(offset..end, data_chars);
            let new: String = chars.into_iter().collect();
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = o.insert_property(
                boa_engine::js_string!("data"),
                pd(JsValue::from(boa_engine::js_string!(new.clone()))),
            );
            let _ = o.insert_property(
                boa_engine::js_string!("textContent"),
                pd(JsValue::from(boa_engine::js_string!(new))),
            );
        }
        Ok(JsValue::undefined())
    });
    let substring_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        if let Some(o) = this.as_object() {
            let old = o
                .get(boa_engine::js_string!("data"), ctx)
                .unwrap_or(JsValue::undefined());
            let old_str = old
                .as_string()
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let chars: Vec<char> = old_str.chars().collect();
            let end = (offset + count).min(chars.len());
            let sub: String = chars[offset.min(chars.len())..end].iter().collect();
            return Ok(JsValue::from(boa_engine::js_string!(sub)));
        }
        Ok(JsValue::from(boa_engine::js_string!("")))
    });
    vec![
        ("appendData", append),
        ("deleteData", delete_d),
        ("insertData", insert_d),
        ("replaceData", replace_d),
        ("substringData", substring_d),
    ]
}

/// forward to tracing.
fn install_console(ctx: &mut Context) {
    use boa_engine::object::ObjectInitializer;
    let mk = |level: &'static str| {
        NativeFunction::from_copy_closure_with_captures(
            move |_this, args, _caps, ctx| {
                let parts: Vec<String> = args
                    .iter()
                    .map(|v| {
                        v.to_string(ctx)
                            .map(|s| s.to_std_string_escaped())
                            .unwrap_or_default()
                    })
                    .collect();
                let msg = parts.join(" ");
                match level {
                    "warn" => tracing::warn!("[js] {}", msg),
                    "error" => tracing::error!("[js] {}", msg),
                    _ => tracing::info!("[js] {}", msg),
                }
                Ok(JsValue::undefined())
            },
            (),
        )
    };
    let console = ObjectInitializer::new(ctx)
        .function(mk("log"), boa_engine::js_string!("log"), 1)
        .function(mk("info"), boa_engine::js_string!("info"), 1)
        .function(mk("warn"), boa_engine::js_string!("warn"), 1)
        .function(mk("error"), boa_engine::js_string!("error"), 1)
        .build();
    let global = ctx.global_object();
    let _ = global.insert_property(
        boa_engine::js_string!("console"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(console)
            .writable(true)
            .enumerable(false)
            .configurable(true)
            .build(),
    );
}

fn install_document(ctx: &mut Context, bridge: Gc<GcRefCell<Bridge>>) -> JsResult<()> {
    use boa_engine::object::ObjectInitializer;

    // getElementById — reads the id snapshot, populates element handle with props.
    let get_by_id = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let id = arg_string(args, 0);
            let nid = b.borrow().ids.get(&id).copied();
            if let Some(nid) = nid {
                let snap = b.borrow().node_props.get(&nid).cloned();
                let handle = make_element_handle(ctx, Gc::clone(b), nid, None)?;
                if let Some(s) = snap {
                    populate_props(&handle, &s, ctx);
                }
                Ok(handle.into())
            } else {
                Ok(JsValue::null())
            }
        },
        Gc::clone(&bridge),
    );

    // createElement
    let create_el = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let tag = arg_string(args, 0);
            // Special-case <canvas>: allocate via thread_local and return a handle.
            if tag == "canvas" {
                let cid = alloc_canvas(300, 150);
                return Ok(make_canvas_handle(ctx, cid)?.into());
            }
            let pid = {
                let mut bb = b.borrow_mut();
                let pid = bb.next_pending;
                bb.next_pending += 1;
                bb.pending
                    .insert(pid, (tag.clone(), String::new(), Vec::new()));
                pid
            };
            let handle = make_element_handle(ctx, Gc::clone(b), 0, Some(pid))?;
            // Populate with proper tag name so createElement('div').tagName === 'DIV'.

            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let upper = tag.to_uppercase();
            let _ = handle.insert_property(
                boa_engine::js_string!("tagName"),
                pd(JsValue::from(boa_engine::js_string!(upper.clone()))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("nodeName"),
                pd(JsValue::from(boa_engine::js_string!(upper))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("localName"),
                pd(JsValue::from(boa_engine::js_string!(tag.to_lowercase()))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("namespaceURI"),
                pd(JsValue::from(boa_engine::js_string!(
                    "http://www.w3.org/1999/xhtml"
                ))),
            );
            Ok(handle.into())
        },
        Gc::clone(&bridge),
    );

    // querySelector
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
                let snap = b.borrow().node_props.get(&nid).cloned();
                let handle = make_element_handle(ctx, Gc::clone(b), nid, None)?;
                if let Some(s) = snap {
                    populate_props(&handle, &s, ctx);
                }
                Ok(handle.into())
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

/// Build a JS canvas handle object: has `width`, `height`, and `getContext('2d')`.
/// Canvas buffers are in the thread_local CANVASES map.
fn make_canvas_handle(ctx: &mut Context, canvas_id: u32) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;

    let get_context = NativeFunction::from_copy_closure(move |_this, args, ctx| {
        let kind = arg_string(args, 0);
        if kind == "webgl" || kind == "experimental-webgl" {
            return Ok(make_webgl_stub(ctx)?.into());
        }
        if kind != "2d" {
            return Ok(JsValue::null());
        }
        Ok(make_context_2d(ctx, canvas_id)?.into())
    });

    let obj = ObjectInitializer::new(ctx)
        .property(
            boa_engine::js_string!("width"),
            JsValue::from(300),
            Attribute::all(),
        )
        .property(
            boa_engine::js_string!("height"),
            JsValue::from(150),
            Attribute::all(),
        )
        .function(get_context, boa_engine::js_string!("getContext"), 1)
        .build();
    Ok(obj)
}

/// Build a JS CanvasRenderingContext2D object bound to canvas_id's buffer
/// (accessed via thread_local CANVASES).
fn make_context_2d(ctx: &mut Context, canvas_id: u32) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;

    let fill_rect = NativeFunction::from_copy_closure(move |_this, args, _ctx| {
        let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
        let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
        let w = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0);
        let h = args.get(3).and_then(|v| v.as_number()).unwrap_or(0.0);
        CANVASES.with(|cs| {
            if let Some(canvas) = cs.borrow_mut().get_mut(&canvas_id) {
                canvas.fill_rect(x, y, w, h);
            }
        });
        Ok(JsValue::undefined())
    });

    let clear_rect = NativeFunction::from_copy_closure(move |_this, args, _ctx| {
        let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
        let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
        let w = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0);
        let h = args.get(3).and_then(|v| v.as_number()).unwrap_or(0.0);
        CANVASES.with(|cs| {
            if let Some(canvas) = cs.borrow_mut().get_mut(&canvas_id) {
                canvas.clear_rect(x, y, w, h);
            }
        });
        Ok(JsValue::undefined())
    });

    let stroke_rect = NativeFunction::from_copy_closure(move |_this, args, _ctx| {
        let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
        let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
        let w = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0);
        let h = args.get(3).and_then(|v| v.as_number()).unwrap_or(0.0);
        CANVASES.with(|cs| {
            if let Some(canvas) = cs.borrow_mut().get_mut(&canvas_id) {
                canvas.stroke_rect(x, y, w, h);
            }
        });
        Ok(JsValue::undefined())
    });

    // fillStyle: getter returns "#rrggbb", setter parses a CSS color.
    let fs_setter = NativeFunction::from_copy_closure(move |_this, args, _ctx| {
        let color = arg_string(args, 0);
        CANVASES.with(|cs| {
            if let Some(canvas) = cs.borrow_mut().get_mut(&canvas_id) {
                canvas.set_fill_style(&color);
            }
        });
        Ok(JsValue::undefined())
    });

    let obj = ObjectInitializer::new(ctx)
        .function(fill_rect, boa_engine::js_string!("fillRect"), 4)
        .function(clear_rect, boa_engine::js_string!("clearRect"), 4)
        .function(stroke_rect, boa_engine::js_string!("strokeRect"), 4)
        .build();
    let fs_set_fn = fs_setter.to_js_function(ctx.realm());
    let _ = obj.define_property_or_throw(
        boa_engine::js_string!("fillStyle"),
        boa_engine::property::PropertyDescriptor::builder()
            .set(fs_set_fn)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
        ctx,
    );
    Ok(obj)
}

/// Build a WebGL no-op stub context. All GL methods are no-ops; constants are
/// set to plausible values. Lets WebGL-using scripts run without crashing.
fn make_webgl_stub(ctx: &mut Context) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;

    let mut init = ObjectInitializer::new(ctx);
    for (name, val) in [
        ("DEPTH_BUFFER_BIT", 256u32),
        ("COLOR_BUFFER_BIT", 16384),
        ("TRIANGLES", 4),
        ("ARRAY_BUFFER", 34962),
        ("STATIC_DRAW", 35044),
        ("VERTEX_SHADER", 35633),
        ("FRAGMENT_SHADER", 35632),
        ("COMPILE_STATUS", 35713),
        ("LINK_STATUS", 35714),
        ("TEXTURE_2D", 3553),
        ("RGBA", 6408),
        ("FLOAT", 5126),
        ("NO_ERROR", 0),
    ] {
        init.property(
            boa_engine::js_string!(name),
            JsValue::from(val),
            Attribute::all(),
        );
    }
    // All GL methods as no-ops (create a fresh NativeFunction per call since
    // NativeFunction is not Copy).
    for name in [
        "activeTexture",
        "attachShader",
        "bindBuffer",
        "bindTexture",
        "blendFunc",
        "bufferData",
        "clear",
        "clearColor",
        "compileShader",
        "createProgram",
        "cullFace",
        "deleteBuffer",
        "deleteProgram",
        "deleteShader",
        "deleteTexture",
        "depthFunc",
        "depthMask",
        "disable",
        "disableVertexAttribArray",
        "drawArrays",
        "drawElements",
        "enable",
        "enableVertexAttribArray",
        "finish",
        "flush",
        "generateMipmap",
        "getProgramInfoLog",
        "getShaderInfoLog",
        "getShaderSource",
        "linkProgram",
        "pixelStorei",
        "shaderSource",
        "texImage2D",
        "texParameteri",
        "texSubImage2D",
        "uniform1f",
        "uniform1i",
        "uniform2f",
        "uniform3f",
        "uniform4f",
        "uniformMatrix4fv",
        "useProgram",
        "vertexAttribPointer",
        "viewport",
        "scissor",
        "getParameter",
        "getExtension",
        "getContextAttributes",
    ] {
        let nf = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
        init.function(nf, boa_engine::js_string!(name), 0);
    }
    // Methods returning specific values.
    let get_error = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::from(0u32)));
    init.function(get_error, boa_engine::js_string!("getError"), 0);
    let get_loc = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::from(0i32)));
    init.function(get_loc, boa_engine::js_string!("getAttribLocation"), 2);
    init.function(
        NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(0i32))),
        boa_engine::js_string!("getUniformLocation"),
        2,
    );
    let get_bool = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::from(true)));
    init.function(get_bool, boa_engine::js_string!("getShaderParameter"), 2);
    init.function(
        NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(true))),
        boa_engine::js_string!("getProgramParameter"),
        2,
    );
    // create* return truthy stub objects.
    for name in ["createShader", "createBuffer", "createTexture"] {
        let nf = NativeFunction::from_copy_closure(|_this, _args, ctx| {
            Ok(boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()).into())
        });
        init.function(nf, boa_engine::js_string!(name), 1);
    }
    init.property(
        boa_engine::js_string!("drawingBufferWidth"),
        JsValue::from(300),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("drawingBufferHeight"),
        JsValue::from(150),
        Attribute::all(),
    );
    Ok(init.build())
}

/// Install WebRTC + navigator stubs so pages using these APIs don't crash.
fn install_webrtc_stubs(ctx: &mut Context) {
    use boa_engine::object::ObjectInitializer;

    // RTCPeerConnection: constructor returning an object with no-op methods.
    let rtc_ctor = NativeFunction::from_copy_closure(|_this, _args, ctx| {
        let mut init = ObjectInitializer::new(ctx);
        for name in [
            "createOffer",
            "createAnswer",
            "setLocalDescription",
            "setRemoteDescription",
            "addIceCandidate",
            "createDataChannel",
            "getStats",
            "close",
            "addTrack",
        ] {
            let nf = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
            init.function(nf, boa_engine::js_string!(name), 0);
        }
        init.property(
            boa_engine::js_string!("localDescription"),
            JsValue::null(),
            Attribute::all(),
        );
        Ok(init.build().into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("RTCPeerConnection"), 0, rtc_ctor);

    // navigator with userAgent + mediaDevices.getUserMedia.
    let get_user_media =
        NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
    let media_devices = ObjectInitializer::new(ctx)
        .function(get_user_media, boa_engine::js_string!("getUserMedia"), 1)
        .build();
    let navigator = ObjectInitializer::new(ctx)
        .property(
            boa_engine::js_string!("mediaDevices"),
            media_devices,
            Attribute::all(),
        )
        .property(
            boa_engine::js_string!("userAgent"),
            JsValue::from(boa_engine::js_string!("aris/0.1")),
            Attribute::all(),
        )
        .build();
    let global = ctx.global_object();
    let _ = global.insert_property(
        boa_engine::js_string!("navigator"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(navigator)
            .writable(true)
            .enumerable(false)
            .configurable(true)
            .build(),
    );

    // WebSocket stub.
    let ws_ctor = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
    let _ = ctx.register_global_callable(boa_engine::js_string!("WebSocket"), 0, ws_ctor);
}

fn make_element_handle(
    ctx: &mut Context,
    bridge: Gc<GcRefCell<Bridge>>,
    nid: u32,
    pending: Option<u32>,
) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;

    // Pre-create objects that need ctx.intrinsics() before borrowing ctx mutably.
    let empty_arr1 = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let empty_arr2 = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
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

    // ── Read-only properties from snapshot ──
    // These are populated when the handle is created from a live node
    // (via getElementById/querySelector). For pending/created elements,
    // they start empty and are updated as ops are applied.
    init.property(
        boa_engine::js_string!("nodeType"),
        JsValue::from(1),
        Attribute::all(),
    ); // ELEMENT_NODE
    init.property(
        boa_engine::js_string!("nodeName"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("tagName"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("namespaceURI"),
        JsValue::from(boa_engine::js_string!("http://www.w3.org/1999/xhtml")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("prefix"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("localName"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("id"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("className"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("nodeValue"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("textContent"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("innerHTML"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("outerHTML"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("parentNode"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("parentElement"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("firstChild"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("lastChild"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("nextSibling"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("previousSibling"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("ownerDocument"),
        JsValue::null(),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("childNodes"),
        JsValue::from(empty_arr1),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("children"),
        JsValue::from(empty_arr2),
        Attribute::all(),
    );
    // CharacterData props
    init.property(
        boa_engine::js_string!("data"),
        JsValue::from(boa_engine::js_string!("")),
        Attribute::all(),
    );
    init.property(
        boa_engine::js_string!("length"),
        JsValue::from(0u32),
        Attribute::all(),
    );

    // ── Standard DOM methods ──

    // getContext (canvas)
    let get_ctx = NativeFunction::from_copy_closure(|_this, args, ctx| {
        let kind = arg_string(args, 0);
        if kind == "webgl" || kind == "experimental-webgl" {
            return Ok(make_webgl_stub(ctx)?.into());
        }
        if kind != "2d" {
            return Ok(JsValue::null());
        }
        let cid = read_handle_id(_this).unwrap_or(0);
        CANVASES.with(|cs| {
            cs.borrow_mut()
                .entry(cid)
                .or_insert_with(|| crate::canvas::Canvas2D::new(300, 150));
        });
        Ok(make_context_2d(ctx, cid)?.into())
    });
    init.function(get_ctx, boa_engine::js_string!("getContext"), 1);

    // setText / textContent setter
    let set_text = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let v = arg_string(args, 0);
            if let Some(nid) = read_handle_id(this) {
                b.borrow_mut().ops.push(Op::SetText {
                    node_id: nid,
                    value: v.clone(),
                });
            }
            // Also update the JS-visible textContent property.
            let _ = this.as_object().map(|o| {
                o.insert_property(
                    boa_engine::js_string!("textContent"),
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(JsValue::from(boa_engine::js_string!(v)))
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build(),
                )
            });
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(set_text, boa_engine::js_string!("setText"), 1);

    // setHTML / innerHTML setter (strips tags)
    let set_html = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let text = strip_tags(&arg_string(args, 0));
            if let Some(nid) = read_handle_id(this) {
                b.borrow_mut().ops.push(Op::SetText {
                    node_id: nid,
                    value: text.clone(),
                });
            }
            let _ = this.as_object().map(|o| {
                o.insert_property(
                    boa_engine::js_string!("textContent"),
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(JsValue::from(boa_engine::js_string!(text)))
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build(),
                )
            });
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(set_html, boa_engine::js_string!("setHTML"), 1);

    // setAttribute
    let set_attr = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let name = arg_string(args, 0);
            let value = arg_string(args, 1);
            if let Some(nid) = read_handle_id(this) {
                b.borrow_mut().ops.push(Op::SetAttr {
                    node_id: nid,
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            // Update JS-visible id/className if applicable.
            if name == "id" {
                let _ = this.as_object().map(|o| {
                    o.insert_property(
                        boa_engine::js_string!("id"),
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(JsValue::from(boa_engine::js_string!(value.clone())))
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build(),
                    )
                });
            } else if name == "class" {
                let _ = this.as_object().map(|o| {
                    o.insert_property(
                        boa_engine::js_string!("className"),
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(JsValue::from(boa_engine::js_string!(value)))
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build(),
                    )
                });
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(set_attr, boa_engine::js_string!("setAttribute"), 2);

    // getAttribute — reads from JS-visible properties (snapshot from creation).
    let get_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0);
        let obj = this.as_object();
        let v = match &obj {
            Some(o) => o
                .get(boa_engine::js_string!(name.clone()), ctx)
                .unwrap_or(JsValue::null()),
            None => JsValue::null(),
        };
        if !v.is_undefined() && !v.is_null() {
            return Ok(v);
        }
        // Fallback: check id/class special cases.
        if name == "id" {
            if let Some(o) = &obj {
                return Ok(o
                    .get(boa_engine::js_string!("id"), ctx)
                    .unwrap_or(JsValue::null()));
            }
        } else if name == "class" {
            if let Some(o) = &obj {
                return Ok(o
                    .get(boa_engine::js_string!("className"), ctx)
                    .unwrap_or(JsValue::null()));
            }
        }
        Ok(JsValue::null())
    });
    init.function(get_attr, boa_engine::js_string!("getAttribute"), 1);

    // hasAttribute
    let has_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0);
        let obj = this.as_object();
        let v = match &obj {
            Some(o) => o
                .get(boa_engine::js_string!(name), ctx)
                .unwrap_or(JsValue::null()),
            None => JsValue::null(),
        };
        Ok(JsValue::from(!v.is_null() && !v.is_undefined()))
    });
    init.function(has_attr, boa_engine::js_string!("hasAttribute"), 1);

    // removeAttribute
    let remove_attr = NativeFunction::from_copy_closure(|this, args, _ctx| {
        let _name = arg_string(args, 0);
        Ok(JsValue::undefined())
    });
    init.function(remove_attr, boa_engine::js_string!("removeAttribute"), 1);

    // addEventListener
    let add_listener = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let kind = arg_string(args, 0);
            if kind != "click" {
                return Ok(JsValue::undefined());
            }
            let handle_id = read_handle_id(this);
            if let (Some(nid), Some(handler)) = (handle_id, args.get(1)) {
                let name = format!("__aris_listener_obj_{}", {
                    let mut bb = b.borrow_mut();
                    bb.next_pending += 1;
                    bb.next_pending
                });
                let _ = _ctx.global_object().insert_property(
                    boa_engine::js_string!(name.clone()),
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(handler.clone())
                        .writable(true)
                        .enumerable(false)
                        .configurable(true)
                        .build(),
                );
                b.borrow_mut().new_listeners.push((nid, name));
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(add_listener, boa_engine::js_string!("addEventListener"), 2);

    // dispatchEvent(event) — calls listeners registered for event.type.
    let dispatch = NativeFunction::from_copy_closure(|this, args, ctx| {
        let event = args.first().cloned().unwrap_or(JsValue::null());
        // Get event type to find matching listeners.
        let event_type = event
            .as_object()
            .and_then(|o| o.get(boa_engine::js_string!("type"), ctx).ok())
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        // Set event.target = this.
        if let Some(ev_obj) = event.as_object() {
            let _ = ev_obj.insert_property(
                boa_engine::js_string!("target"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(this.clone())
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
        }
        // For now, we look for event handler properties (onclick, onload, etc.)
        // and call them. addEventListener-registered handlers are fired via
        // fire_click for click events.
        if !event_type.is_empty() {
            let handler_prop = format!("on{}", event_type);
            if let Some(this_obj) = this.as_object() {
                if let Ok(handler) = this_obj.get(boa_engine::js_string!(handler_prop), ctx) {
                    if let Some(fn_obj) = handler.as_object() {
                        if fn_obj.is_callable() {
                            let _ = fn_obj.call(this, &[event.clone()], ctx);
                        }
                    }
                }
            }
        }
        Ok(JsValue::from(true))
    });
    init.function(dispatch, boa_engine::js_string!("dispatchEvent"), 1);

    // appendChild
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

    // removeChild — returns the removed child (no-op on blitz side for now).
    let remove_child = NativeFunction::from_copy_closure(|_this, args, _ctx| {
        Ok(args.first().cloned().unwrap_or(JsValue::null()))
    });
    init.function(remove_child, boa_engine::js_string!("removeChild"), 1);

    // insertBefore — returns the inserted node (simplified).
    let insert_before = NativeFunction::from_copy_closure(|_this, args, _ctx| {
        Ok(args.first().cloned().unwrap_or(JsValue::null()))
    });
    init.function(insert_before, boa_engine::js_string!("insertBefore"), 2);

    // cloneNode(deep) — returns a shallow copy.
    let clone = NativeFunction::from_copy_closure(|this, _args, ctx| {
        let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        // Copy _arisId so the clone can still read the same blitz node.
        let id = this
            .as_object()
            .and_then(|o| o.get(boa_engine::js_string!("_arisId"), ctx).ok())
            .unwrap_or(JsValue::null());
        let _ = obj.insert_property(
            boa_engine::js_string!("_arisId"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(id)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        Ok(obj.into())
    });
    init.function(clone, boa_engine::js_string!("cloneNode"), 0);

    // contains(node) — check if this node contains another.
    let contains_fn =
        NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::from(false)));
    init.function(contains_fn, boa_engine::js_string!("contains"), 1);

    // closest(selector) — returns null (no CSS selector engine on handles).
    let closest_fn = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::null()));
    init.function(closest_fn, boa_engine::js_string!("closest"), 1);

    // matches(selector) — returns false.
    let matches_fn =
        NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::from(false)));
    init.function(matches_fn, boa_engine::js_string!("matches"), 1);

    // getElementsByTagName — returns empty array.
    let by_tag = NativeFunction::from_copy_closure(|_this, _args, ctx| {
        Ok(boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()).into())
    });
    init.function(by_tag, boa_engine::js_string!("getElementsByTagName"), 1);

    // getElementsByClassName — returns empty array.
    let by_class = NativeFunction::from_copy_closure(|_this, _args, ctx| {
        Ok(boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()).into())
    });
    init.function(
        by_class,
        boa_engine::js_string!("getElementsByClassName"),
        1,
    );

    // querySelector / querySelectorAll on element — returns null / empty.
    let qs = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::null()));
    init.function(qs, boa_engine::js_string!("querySelector"), 1);
    let qsa = NativeFunction::from_copy_closure(|_this, _args, ctx| {
        Ok(boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()).into())
    });
    init.function(qsa, boa_engine::js_string!("querySelectorAll"), 1);

    // ── CharacterData methods ──
    let append_data = NativeFunction::from_copy_closure(|this, args, ctx| {
        let v = arg_string(args, 0);
        let old = this
            .as_object()
            .and_then(|o| o.get(boa_engine::js_string!("data"), ctx).ok())
            .unwrap_or(JsValue::undefined());
        let old_str = old
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        let new = format!("{}{}", old_str, v);
        this.as_object().map(|o| {
            o.insert_property(
                boa_engine::js_string!("data"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(new.clone())))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            )
        });
        this.as_object().map(|o| {
            o.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(new)))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            )
        });
        Ok(JsValue::undefined())
    });
    init.function(append_data, boa_engine::js_string!("appendData"), 1);

    let delete_data = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let old = this
            .as_object()
            .and_then(|o| o.get(boa_engine::js_string!("data"), ctx).ok())
            .unwrap_or(JsValue::undefined());
        let old_str = old
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        let mut chars: Vec<char> = old_str.chars().collect();
        let end = (offset + count).min(chars.len());
        chars.drain(offset..end);
        let new: String = chars.into_iter().collect();
        this.as_object().map(|o| {
            o.insert_property(
                boa_engine::js_string!("data"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(new.clone())))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            )
        });
        this.as_object().map(|o| {
            o.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(new)))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            )
        });
        Ok(JsValue::undefined())
    });
    init.function(delete_data, boa_engine::js_string!("deleteData"), 2);

    let insert_data = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let data = arg_string(args, 1);
        let old = this
            .as_object()
            .and_then(|o| o.get(boa_engine::js_string!("data"), ctx).ok())
            .unwrap_or(JsValue::undefined());
        let old_str = old
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        let new = format!(
            "{}{}{}",
            &old_str[..offset.min(old_str.len())],
            data,
            &old_str[offset.min(old_str.len())..]
        );
        this.as_object().map(|o| {
            o.insert_property(
                boa_engine::js_string!("data"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(new.clone())))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            )
        });
        this.as_object().map(|o| {
            o.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(new)))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            )
        });
        Ok(JsValue::undefined())
    });
    init.function(insert_data, boa_engine::js_string!("insertData"), 2);

    let replace_data = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let data = arg_string(args, 2);
        let old = this
            .as_object()
            .and_then(|o| o.get(boa_engine::js_string!("data"), ctx).ok())
            .unwrap_or(JsValue::undefined());
        let old_str = old
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        let mut chars: Vec<char> = old_str.chars().collect();
        let end = (offset + count).min(chars.len());
        let data_chars: Vec<char> = data.chars().collect();
        chars.splice(offset..end, data_chars);
        let new: String = chars.into_iter().collect();
        this.as_object().map(|o| {
            o.insert_property(
                boa_engine::js_string!("data"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(new.clone())))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            )
        });
        this.as_object().map(|o| {
            o.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(new)))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            )
        });
        Ok(JsValue::undefined())
    });
    init.function(replace_data, boa_engine::js_string!("replaceData"), 3);

    let substring_data = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let old = this
            .as_object()
            .and_then(|o| o.get(boa_engine::js_string!("data"), ctx).ok())
            .unwrap_or(JsValue::undefined());
        let old_str = old
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        let chars: Vec<char> = old_str.chars().collect();
        let end = (offset + count).min(chars.len());
        let sub: String = chars[offset.min(chars.len())..end].iter().collect();
        Ok(JsValue::from(boa_engine::js_string!(sub)))
    });
    init.function(substring_data, boa_engine::js_string!("substringData"), 2);

    Ok(init.build())
}
