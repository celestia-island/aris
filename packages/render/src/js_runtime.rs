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
use boa_engine::object::builtins::JsArray;
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
        install_dom_globals(&mut self.ctx);
        install_event_api(&mut self.ctx);
        install_window(&mut self.ctx, url.to_string());

        // Populate document.documentElement / body / head with real element
        // handles from the bridge snapshots.
        self.populate_doc_elements(&doc_snapshot);

        let _ = url;
        if !script_src.trim().is_empty() {
            match self.ctx.eval(Source::from_bytes(script_src)) {
                Ok(_v) => {}
                Err(e) => {
                    eprintln!("[aris-js] eval error: {}", e.to_string());
                }
            }
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
        let Some(doc_obj) = doc_val.as_ref().and_then(|v| v.as_object()) else {
            return;
        };

        let bridge = Gc::clone(&self.bridge);

        // Helper: create + populate handle for a node, and set its prototype.
        let mk = |nid: u32, snap: &NodePropSnapshot, ctx: &mut Context| {
            let handle =
                make_element_handle(ctx, Gc::clone(&bridge), nid, None).unwrap_or_else(|_| {
                    boa_engine::object::JsObject::with_object_proto(ctx.intrinsics())
                });
            populate_props(&handle, snap, ctx);
            // Set prototype so instanceof works (e.g. body instanceof HTMLBodyElement).
            set_element_prototype(&handle, &snap.tag_name, ctx);
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
                // Debug format: Atom('div' type=inline) or Atom('div' type=static)
                // Extract the tag name between Atom(' and '
                let dbg = format!("{:?}", e.name.local);
                if let Some(rest) = dbg.strip_prefix("Atom('") {
                    if let Some(end) = rest.find('\'') {
                        rest[..end].to_uppercase()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
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
            let dbg = format!("{:?}", el.name.local);
            let tag = if let Some(rest) = dbg.strip_prefix("Atom('") {
                if let Some(end) = rest.find('\'') { rest[..end].to_string() } else { String::new() }
            } else { String::new() };
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

/// Check if a string is a valid XML Name (first char).
/// NameStartChar ::= ":" | [A-Z] | "_" | [a-z] | [#xC0-#xD6] | [#xD8-#xF6] | [#xF8-#x2FF] | ...
fn is_valid_name_first(s: &str) -> bool {
    let first = match s.chars().next() {
        Some(c) => c,
        None => return false,
    };
    first.is_ascii_alphabetic() || first == '_' || first == ':'
    // Extended Unicode ranges omitted for simplicity; WPT tests use ASCII.
}

/// Validate that a string is a valid Name production (all characters valid).
fn is_valid_name(s: &str) -> bool {
    if s.is_empty() || !is_valid_name_first(s) {
        return false;
    }
    for c in s.chars().skip(1) {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '-' || c == '.') {
            return false;
        }
    }
    true
}

/// Set the prototype of an element handle to the corresponding HTMLxxxElement.prototype,
/// so that `elem instanceof HTMLxxxElement` works.
fn set_element_prototype(handle: &JsObject, tag: &str, ctx: &mut Context) {
    let ctor_name = tag_to_html_element_class(tag);
    if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!(ctor_name), ctx) {
        if let Some(ctor_obj) = ctor_val.as_object() {
            if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                if let Some(proto) = proto_val.as_object() {
                    let _ = handle.set_prototype(Some(proto));
                }
            }
        }
    }
}

/// Map an HTML tag name to its corresponding HTMLxxxElement constructor name.
fn tag_to_html_element_class(tag: &str) -> String {
    // Known element interfaces per HTML spec.
    match tag.to_lowercase().as_str() {
        "a" => "HTMLAnchorElement",
        "area" => "HTMLAreaElement",
        "audio" => "HTMLAudioElement",
        "br" => "HTMLBRElement",
        "base" => "HTMLBaseElement",
        "body" => "HTMLBodyElement",
        "button" => "HTMLButtonElement",
        "canvas" => "HTMLCanvasElement",
        "dl" => "HTMLDListElement",
        "data" => "HTMLDataElement",
        "datalist" => "HTMLDataListElement",
        "details" => "HTMLDetailsElement",
        "dialog" => "HTMLDialogElement",
        "dir" => "HTMLDirectoryElement",
        "div" => "HTMLDivElement",
        "embed" => "HTMLEmbedElement",
        "fieldset" => "HTMLFieldSetElement",
        "font" => "HTMLFontElement",
        "form" => "HTMLFormElement",
        "frame" => "HTMLFrameElement",
        "frameset" => "HTMLFrameSetElement",
        "hr" => "HTMLHRElement",
        "head" => "HTMLHeadElement",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "HTMLHeadingElement",
        "html" => "HTMLHtmlElement",
        "iframe" => "HTMLIFrameElement",
        "img" => "HTMLImageElement",
        "input" => "HTMLInputElement",
        "li" => "HTMLLIElement",
        "label" => "HTMLLabelElement",
        "legend" => "HTMLLegendElement",
        "link" => "HTMLLinkElement",
        "map" => "HTMLMapElement",
        "menu" => "HTMLMenuElement",
        "meta" => "HTMLMetaElement",
        "meter" => "HTMLMeterElement",
        "ins" | "del" => "HTMLModElement",
        "ol" => "HTMLOListElement",
        "object" => "HTMLObjectElement",
        "optgroup" => "HTMLOptGroupElement",
        "option" => "HTMLOptionElement",
        "output" => "HTMLOutputElement",
        "p" => "HTMLParagraphElement",
        "param" => "HTMLParamElement",
        "picture" => "HTMLPictureElement",
        "pre" => "HTMLPreElement",
        "progress" => "HTMLProgressElement",
        "blockquote" | "q" => "HTMLQuoteElement",
        "script" => "HTMLScriptElement",
        "select" => "HTMLSelectElement",
        "slot" => "HTMLSlotElement",
        "source" => "HTMLSourceElement",
        "span" => "HTMLSpanElement",
        "style" => "HTMLStyleElement",
        "caption" => "HTMLTableCaptionElement",
        "th" | "td" => "HTMLTableCellElement",
        "col" | "colgroup" => "HTMLTableColElement",
        "table" => "HTMLTableElement",
        "tr" => "HTMLTableRowElement",
        "thead" | "tbody" | "tfoot" => "HTMLTableSectionElement",
        "template" => "HTMLTemplateElement",
        "textarea" => "HTMLTextAreaElement",
        "time" => "HTMLTimeElement",
        "title" => "HTMLTitleElement",
        "track" => "HTMLTrackElement",
        "ul" => "HTMLUListElement",
        "video" => "HTMLVideoElement",
        _ => "HTMLElement",
    }
    .to_string()
}

fn arg_string(args: &[JsValue], idx: usize) -> String {
    args.get(idx)
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_default()
}

/// Convert a JsValue to u32 using JS ToUint32 semantics (wraps negative numbers).
fn js_to_uint32(v: &JsValue) -> u32 {
    v.as_number()
        .map(|n| {
            // JS ToUint32: modulo 2^32
            let n = n.trunc();
            let n = n % 4294967296.0;
            let n = if n < 0.0 { n + 4294967296.0 } else { n };
            n as u32
        })
        .unwrap_or(0)
}

/// Like arg_string but converts any JsValue to string via JS String() semantics.
/// undefined → "undefined", null → "null", numbers → their string form.
fn arg_to_string(args: &[JsValue], idx: usize, ctx: &mut Context) -> String {
    match args.get(idx) {
        Some(v) => match v.to_string(ctx) {
            Ok(s) => s.to_std_string_escaped(),
            Err(_) => String::new(),
        },
        None => String::new(),
    }
}

/// Populate a JS element handle with real properties from a node snapshot.
fn populate_props(obj: &JsObject, s: &NodePropSnapshot, ctx: &mut Context) {
    // Determine if parent is body/html/documentElement by checking document properties.
    let parent_is_body = {
        let doc_val = ctx.global_object().get(boa_engine::js_string!("document"), ctx).ok();
        doc_val.as_ref()
            .and_then(|v| v.as_object())
            .and_then(|d| d.get(boa_engine::js_string!("body"), ctx).ok())
            .and_then(|b| b.as_object())
            .and_then(|b| b.get(boa_engine::js_string!("_arisId"), ctx).ok())
            .and_then(|v| v.as_number())
            .map(|id| id as u32 == s.parent_id)
            .unwrap_or(false)
    };
    let parent_is_html = {
        let doc_val = ctx.global_object().get(boa_engine::js_string!("document"), ctx).ok();
        doc_val.as_ref()
            .and_then(|v| v.as_object())
            .and_then(|d| d.get(boa_engine::js_string!("documentElement"), ctx).ok())
            .and_then(|b| b.as_object())
            .and_then(|b| b.get(boa_engine::js_string!("_arisId"), ctx).ok())
            .and_then(|v| v.as_number())
            .map(|id| id as u32 == s.parent_id)
            .unwrap_or(false)
    };
    let snapshot_parent_tag = if parent_is_body { "body" } else if parent_is_html { "html" } else { "" };
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
    // prefix and namespaceURI for elements (null for HTML elements without colons).
    let _ = obj.insert_property(
        boa_engine::js_string!("prefix"),
        pd(JsValue::null()),
    );
    let ns = if s.node_type == 1 {
        JsValue::from(boa_engine::js_string!("http://www.w3.org/1999/xhtml"))
    } else {
        JsValue::null()
    };
    let _ = obj.insert_property(boa_engine::js_string!("namespaceURI"), pd(ns));
    // baseURI and isConnected for all nodes.
    let _ = obj.insert_property(
        boa_engine::js_string!("baseURI"),
        pd(JsValue::from(boa_engine::js_string!("about:blank"))),
    );
    // isConnected: true for nodes in the document tree (simplified: true for elements with valid id).
    let _ = obj.insert_property(
        boa_engine::js_string!("isConnected"),
        pd(JsValue::from(s.node_type > 0)),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("length"),
        pd(JsValue::from(s.text_content.chars().count() as u32)),
    );

    // Store all attributes as JS properties.
    for (k, v) in &s.attrs {
        let _ = obj.insert_property(boa_engine::js_string!(k.clone()), pd(s_str(v)));
    }

    // Build NamedNodeMap (element.attributes): an array-like of Attr objects
    // with .name, .value, .prefix, .namespaceURI, plus .length and getNamedItem().
    let attrs_arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    for (i, (k, v)) in s.attrs.iter().enumerate() {
        let attr_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = attr_obj.insert_property(boa_engine::js_string!("name"), pd(s_str(k)));
        let _ = attr_obj.insert_property(boa_engine::js_string!("value"), pd(s_str(v)));
        let _ = attr_obj.insert_property(boa_engine::js_string!("nodeName"), pd(s_str(k)));
        let _ = attr_obj.insert_property(boa_engine::js_string!("nodeValue"), pd(s_str(v)));
        let _ = attr_obj.insert_property(boa_engine::js_string!("textContent"), pd(s_str(v)));
        let _ = attr_obj.insert_property(boa_engine::js_string!("localName"), pd(s_str(k)));
        let _ = attr_obj.insert_property(
            boa_engine::js_string!("prefix"),
            pd(JsValue::null()),
        );
        let _ = attr_obj.insert_property(
            boa_engine::js_string!("namespaceURI"),
            pd(JsValue::null()),
        );
        let _ = attr_obj.insert_property(boa_engine::js_string!("specified"), pd(JsValue::from(true)));
        let _ = attr_obj.insert_property(boa_engine::js_string!("ownerElement"), pd(JsValue::from(obj.clone())));
        let _ = attr_obj.insert_property(boa_engine::js_string!("localName"), pd(s_str(k)));
        let _ = attrs_arr.insert_property(i as u32, pd(JsValue::from(attr_obj)));
    }
    let _ = attrs_arr.insert_property(
        boa_engine::js_string!("length"),
        pd(JsValue::from(s.attrs.len() as u32)),
    );
    let _ = obj.insert_property(boa_engine::js_string!("attributes"), pd(attrs_arr.into()));

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
    // children (HTMLCollection of element children) — simplified to same as childNodes.
    let _ = obj.insert_property(
        boa_engine::js_string!("children"),
        pd(JsValue::from(boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()))),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("childElementCount"),
        pd(JsValue::from(0u32)),
    );
    // firstElementChild / lastElementChild / nextElementSibling / previousElementSibling
    let _ = obj.insert_property(
        boa_engine::js_string!("firstElementChild"),
        pd(JsValue::null()),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("lastElementChild"),
        pd(JsValue::null()),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("nextElementSibling"),
        pd(JsValue::null()),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("previousElementSibling"),
        pd(JsValue::null()),
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
        // Try to use document.body/documentElement if the parent is body/html.
        // This ensures parentNode has DOM methods (insertBefore, removeChild, etc).
        let parent_tag = snapshot_parent_tag;
        let doc_val = ctx.global_object().get(boa_engine::js_string!("document"), ctx).ok();
        let doc_obj = doc_val.as_ref().and_then(|v| v.as_object());
        let parent_val = if parent_tag == "body" {
            doc_obj.and_then(|d| d.get(boa_engine::js_string!("body"), ctx).ok())
                .filter(|v| !v.is_null() && !v.is_undefined())
                .unwrap_or_else(|| {
                    let pn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let _ = pn.insert_property(boa_engine::js_string!("_arisId"), pd(JsValue::from(s.parent_id)));
                    JsValue::from(pn)
                })
        } else if parent_tag == "html" {
            doc_obj.and_then(|d| d.get(boa_engine::js_string!("documentElement"), ctx).ok())
                .filter(|v| !v.is_null() && !v.is_undefined())
                .unwrap_or_else(|| {
                    let pn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let _ = pn.insert_property(boa_engine::js_string!("_arisId"), pd(JsValue::from(s.parent_id)));
                    JsValue::from(pn)
                })
        } else {
            let pn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = pn.insert_property(boa_engine::js_string!("_arisId"), pd(JsValue::from(s.parent_id)));
            JsValue::from(pn)
        };
        let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(parent_val.clone()));
        let _ = obj.insert_property(boa_engine::js_string!("parentElement"), pd(parent_val));
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

/// Clone a JS node object: copy all enumerable own properties (except internal
/// _arisId/_pending/_childIndex), then optionally recurse into childNodes.
fn clone_node_js(src: Option<&JsObject>, deep: bool, ctx: &mut Context) -> JsResult<JsValue> {
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };

    let Some(src) = src else {
        return Ok(JsValue::null());
    };

    let clone = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());

    // Copy the prototype from src so instanceof works on the clone.
    let proto = src.prototype();
    let _ = clone.set_prototype(proto);

    // Copy all own properties from src to clone.
    let keys = src.own_property_keys(ctx).map_err(|e| {
        boa_engine::JsNativeError::typ().with_message(format!("own_property_keys: {:?}", e))
    })?;
    for key in keys {
        let key_str = format!("{:?}", key);
        // Skip internal properties.
        if key_str.contains("_arisId")
            || key_str.contains("_pending")
            || key_str.contains("_childIndex")
            || key_str.contains("_children")  // Prevent deep clone recursion
            || key_str.contains("_data")       // Internal CharacterData store
        {
            continue;
        }
        // Skip accessor properties (firstChild, lastChild, innerHTML, data, nodeValue)
        // to prevent infinite recursion during deep clone.
        if key_str.contains("firstChild") || key_str.contains("lastChild")
            || key_str.contains("innerHTML") || key_str.contains("outerHTML")
            || key_str.contains("previousSibling") || key_str.contains("nextSibling")
            || key_str.contains("ownerElement")  // Prevent circular reference recursion
            || key_str.contains("ownerDocument") // Circular: element → document → elements
            || key_str.contains("textContent")   // Accessor property
            || key_str.contains("defaultView")   // Circular: document → window → document
            || key_str.contains("\"parentNode\"")  // Prevent parent-child cycles
            || key_str.contains("\"documentElement\"") // Circular: doc → html → doc
            || key_str.contains("\"body\"")         // Circular: doc → body → doc
            || key_str.contains("\"head\"")         // Circular: doc → head → doc
            || key_str.contains("\"doctype\"")      // Circular: doc → doctype → doc
        {
            continue;
        }
        // Get the value.
        let val = match src.get(key.clone(), ctx) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // If deep and this is childNodes, recurse.
        if deep && (key_str.contains("childNodes") || key_str.contains("children")) {
            if let Some(arr) = val.as_object() {
                let new_arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let arr_keys = arr.own_property_keys(ctx).unwrap_or_default();
                for (i, ak) in arr_keys.iter().enumerate() {
                    if let Ok(child_val) = arr.get(ak.clone(), ctx) {
                        if let Some(child_obj) = child_val.as_object() {
                            let child_clone = clone_node_js(Some(&child_obj), true, ctx)?;
                            let _ = new_arr.insert_property(i as u32, pd(child_clone));
                        }
                    }
                }
                let _ = clone.insert_property(key, pd(new_arr.into()));
                continue;
            }
        }
        // Functions are shared (not cloned).
        if val.as_object().is_some_and(|o| o.is_callable()) {
            let _ = clone.insert_property(key, pd(val));
            continue;
        }
        // Always clone the attributes NamedNodeMap (even in shallow clone).
        if key_str.contains("attributes") && val.as_object().is_some() && !val.is_null() {
            let obj = val.as_object();
            let attr_clone = clone_node_js(obj.as_ref(), true, ctx)?;
            let _ = clone.insert_property(key, pd(attr_clone));
            continue;
        }
        // For all other values (including objects), just copy by reference.
        // Deep cloning arbitrary objects risks infinite recursion due to
        // circular references (ownerDocument, implementation, etc.).
        let _ = clone.insert_property(key, pd(val));
    }

    // Copy methods (functions) that weren't copied above — re-add standard
    // DOM methods by checking if src has them.
    // Since methods are function objects (not enumerable in our setup), they
    // may not appear in own_property_keys. We add the most common ones.
    // Actually, in our setup methods ARE own properties, so they should be
    // captured above. But let's make sure cloneNode itself is available.
    let clone_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let d = args.first().and_then(|v| v.as_boolean()).unwrap_or(false);
        let obj = this.as_object();
        clone_node_js(obj.as_ref(), d, ctx)
    });
    let _ = clone.insert_property(
        boa_engine::js_string!("cloneNode"),
        pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), clone_fn).build(),
        )),
    );

    Ok(clone.into())
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
    let alert_fn = NativeFunction::from_copy_closure_with_captures(
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
    let alert = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), alert_fn)
        .name(boa_engine::js_string!("alert"))
        .length(1)
        .build();

    // In a browser, `window` IS the global object. So we install location,
    // alert, etc. directly on the global object, and make window/self point
    // back to it.
    let global = ctx.global_object();
    let _ = global.insert_property(
        boa_engine::js_string!("location"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(location)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );
    let _ = global.insert_property(
        boa_engine::js_string!("alert"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(alert)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );
    // window, self, globalThis all point to the global object itself.
    let self_ref = JsValue::from(global.clone());
    for name in &["window", "self", "globalThis", "top", "parent", "frames"] {
        let _ = global.insert_property(
            boa_engine::js_string!(*name),
            boa_engine::property::PropertyDescriptor::builder()
                .value(self_ref.clone())
                .writable(true)
                .enumerable(false)
                .configurable(true)
                .build(),
        );
    }
    // window.length = 0 (no frames).
    let _ = global.insert_property(
        boa_engine::js_string!("length"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(0u32))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );
    // window.name = "" (window name).
    let _ = global.insert_property(
        boa_engine::js_string!("name"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(boa_engine::js_string!("")))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );
    // window.closed = false.
    let _ = global.insert_property(
        boa_engine::js_string!("closed"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(false))
            .writable(true)
            .enumerable(true)
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
        let _ = obj.insert_property(boa_engine::js_string!("returnValue"), pd(JsValue::from(true)));
        let _ = obj.insert_property(boa_engine::js_string!("cancelBubble"), pd(JsValue::from(false)));
        let _ = obj.insert_property(boa_engine::js_string!("target"), pd(JsValue::null()));
        let _ = obj.insert_property(boa_engine::js_string!("srcElement"), pd(JsValue::null()));
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
        let pd_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
            if let Some(o) = this.as_object() {
                // Only prevent if cancelable and not passive.
                let cancelable = o.get(boa_engine::js_string!("cancelable"), ctx).ok()
                    .and_then(|v| v.as_boolean()).unwrap_or(false);
                let passive = o.get(boa_engine::js_string!("_passive"), ctx).ok()
                    .and_then(|v| v.as_boolean()).unwrap_or(false);
                if cancelable && !passive {
                    let pd2 = |val: JsValue| {
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(val).writable(true).enumerable(true).configurable(true).build()
                    };
                    let _ = o.insert_property(boa_engine::js_string!("defaultPrevented"), pd2(JsValue::from(true)));
                    let _ = o.insert_property(boa_engine::js_string!("returnValue"), pd2(JsValue::from(false)));
                }
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("preventDefault"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pd_fn).build(),
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
/// instanceof constructors, Range, and other DOM infrastructure.
fn install_dom_globals(ctx: &mut Context) {
    fn pd(val: JsValue) -> boa_engine::property::PropertyDescriptor {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    }

    // Range constructor: new Range() creates a collapsed range at (document, 0).
    let range_ctor = NativeFunction::from_copy_closure(|_this, _args, ctx| {
        let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        // Get the document global as startContainer/endContainer.
        let doc = ctx
            .global_object()
            .get(boa_engine::js_string!("document"), ctx)
            .unwrap_or(JsValue::null());
        let _ = obj.insert_property(boa_engine::js_string!("startContainer"), pd(doc.clone()));
        let _ = obj.insert_property(boa_engine::js_string!("endContainer"), pd(doc));
        let _ = obj.insert_property(
            boa_engine::js_string!("startOffset"),
            pd(JsValue::from(0u32)),
        );
        let _ = obj.insert_property(boa_engine::js_string!("endOffset"), pd(JsValue::from(0u32)));
        let _ = obj.insert_property(boa_engine::js_string!("collapsed"), pd(JsValue::from(true)));
        let _ = obj.insert_property(
            boa_engine::js_string!("commonAncestorContainer"),
            pd(ctx
                .global_object()
                .get(boa_engine::js_string!("document"), ctx)
                .unwrap_or(JsValue::null())),
        );
        // Range methods (all simplified — operate on JS properties, not blitz DOM).
        let set_start = NativeFunction::from_copy_closure(|this, args, ctx| {
            let container = args.first().cloned().unwrap_or(JsValue::null());
            let offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
            if let Some(o) = this.as_object() {
                let _ = o.insert_property(boa_engine::js_string!("startContainer"), pd(container));
                let _ = o.insert_property(
                    boa_engine::js_string!("startOffset"),
                    pd(JsValue::from(offset)),
                );
                // Update collapsed.
                let end_off = o
                    .get(boa_engine::js_string!("endOffset"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                let start_cont = o.get(boa_engine::js_string!("startContainer"), ctx).ok();
                let end_cont = o.get(boa_engine::js_string!("endContainer"), ctx).ok();
                let is_collapsed = offset == end_off && start_cont == end_cont;
                let _ = o.insert_property(
                    boa_engine::js_string!("collapsed"),
                    pd(JsValue::from(is_collapsed)),
                );
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("setStart"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_start).build(),
            )),
        );
        let set_end = NativeFunction::from_copy_closure(|this, args, ctx| {
            let container = args.first().cloned().unwrap_or(JsValue::null());
            let offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
            if let Some(o) = this.as_object() {
                let _ = o.insert_property(boa_engine::js_string!("endContainer"), pd(container));
                let _ = o.insert_property(
                    boa_engine::js_string!("endOffset"),
                    pd(JsValue::from(offset)),
                );
                let start_off = o
                    .get(boa_engine::js_string!("startOffset"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                let start_cont = o.get(boa_engine::js_string!("startContainer"), ctx).ok();
                let end_cont = o.get(boa_engine::js_string!("endContainer"), ctx).ok();
                let is_collapsed = offset == start_off && start_cont == end_cont;
                let _ = o.insert_property(
                    boa_engine::js_string!("collapsed"),
                    pd(JsValue::from(is_collapsed)),
                );
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("setEnd"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_end).build(),
            )),
        );
        let set_start_before =
            NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("setStartBefore"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_start_before)
                    .build(),
            )),
        );
        let set_start_after =
            NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("setStartAfter"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_start_after)
                    .build(),
            )),
        );
        let set_end_before =
            NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("setEndBefore"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_end_before).build(),
            )),
        );
        let set_end_after =
            NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("setEndAfter"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_end_after).build(),
            )),
        );
        let collapse_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let to_start = args.first().and_then(|v| v.as_boolean()).unwrap_or(true);
            if let Some(o) = this.as_object() {
                if to_start {
                    let sc = o
                        .get(boa_engine::js_string!("startContainer"), ctx)
                        .ok()
                        .unwrap_or(JsValue::null());
                    let so = o
                        .get(boa_engine::js_string!("startOffset"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    let _ = o.insert_property(boa_engine::js_string!("endContainer"), pd(sc));
                    let _ = o.insert_property(
                        boa_engine::js_string!("endOffset"),
                        pd(JsValue::from(so)),
                    );
                } else {
                    let ec = o
                        .get(boa_engine::js_string!("endContainer"), ctx)
                        .ok()
                        .unwrap_or(JsValue::null());
                    let eo = o
                        .get(boa_engine::js_string!("endOffset"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    let _ = o.insert_property(boa_engine::js_string!("startContainer"), pd(ec));
                    let _ = o.insert_property(
                        boa_engine::js_string!("startOffset"),
                        pd(JsValue::from(eo)),
                    );
                }
                let _ =
                    o.insert_property(boa_engine::js_string!("collapsed"), pd(JsValue::from(true)));
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("collapse"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), collapse_fn).build(),
            )),
        );
        let select_node = NativeFunction::from_copy_closure(|this, args, ctx| {
            let node = args.first().cloned().unwrap_or(JsValue::null());
            if let Some(o) = this.as_object() {
                let _ =
                    o.insert_property(boa_engine::js_string!("startContainer"), pd(node.clone()));
                let _ = o.insert_property(boa_engine::js_string!("endContainer"), pd(node));
                let _ = o.insert_property(
                    boa_engine::js_string!("startOffset"),
                    pd(JsValue::from(0u32)),
                );
                let _ =
                    o.insert_property(boa_engine::js_string!("endOffset"), pd(JsValue::from(0u32)));
                let _ =
                    o.insert_property(boa_engine::js_string!("collapsed"), pd(JsValue::from(true)));
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("selectNode"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), select_node).build(),
            )),
        );
        let select_node_contents = NativeFunction::from_copy_closure(|this, args, ctx| {
            let node = args.first().cloned().unwrap_or(JsValue::null());
            if let Some(o) = this.as_object() {
                let _ =
                    o.insert_property(boa_engine::js_string!("startContainer"), pd(node.clone()));
                let _ = o.insert_property(boa_engine::js_string!("endContainer"), pd(node));
                let _ = o.insert_property(
                    boa_engine::js_string!("startOffset"),
                    pd(JsValue::from(0u32)),
                );
                let _ =
                    o.insert_property(boa_engine::js_string!("endOffset"), pd(JsValue::from(0u32)));
                let _ =
                    o.insert_property(boa_engine::js_string!("collapsed"), pd(JsValue::from(true)));
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("selectNodeContents"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), select_node_contents)
                    .build(),
            )),
        );
        let clone_range = NativeFunction::from_copy_closure(|this, _args, ctx| {
            let src = this.as_object();
            let new_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            if let Some(s) = src {
                for prop_name in [
                    "startContainer",
                    "endContainer",
                    "startOffset",
                    "endOffset",
                    "collapsed",
                    "commonAncestorContainer",
                ] {
                    if let Ok(v) = s.get(boa_engine::js_string!(prop_name), ctx) {
                        let _ = new_obj.insert_property(boa_engine::js_string!(prop_name), pd(v));
                    }
                }
            }
            Ok(new_obj.into())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("cloneRange"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), clone_range).build(),
            )),
        );
        let detach = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("detach"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), detach).build(),
            )),
        );
        // Content manipulation methods (return null/empty for now).
        for mname in [
            "deleteContents",
            "extractContents",
            "cloneContents",
            "insertNode",
            "surroundContents",
            "createContextualFragment",
        ] {
            let noop = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::undefined()));
            let _ = obj.insert_property(
                boa_engine::js_string!(mname),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop).build(),
                )),
            );
        }
        // toString returns empty string.
        let to_str = NativeFunction::from_copy_closure(|_t, _a, _c| {
            Ok(JsValue::from(boa_engine::js_string!("")))
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("toString"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), to_str).build(),
            )),
        );
        // Comparison methods (return 0/true/false defaults).
        let compare_bp = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(0i32)));
        let _ = obj.insert_property(
            boa_engine::js_string!("compareBoundaryPoints"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), compare_bp).build(),
            )),
        );
        let compare_point = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(0i32)));
        let _ = obj.insert_property(
            boa_engine::js_string!("comparePoint"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), compare_point).build(),
            )),
        );
        let intersects = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(false)));
        let _ = obj.insert_property(
            boa_engine::js_string!("intersectsNode"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), intersects).build(),
            )),
        );
        let is_point_in = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(false)));
        let _ = obj.insert_property(
            boa_engine::js_string!("isPointInRange"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), is_point_in).build(),
            )),
        );
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("Range"), 0, range_ctor);

    // document.createRange()
    let doc_val = ctx
        .global_object()
        .get(boa_engine::js_string!("document"), &mut *ctx)
        .ok();
    if let Some(doc_obj) = doc_val.as_ref().and_then(|v| v.as_object()) {
        let create_range = NativeFunction::from_copy_closure(|_t, _a, ctx| {
            // Call the Range constructor.
            let range_ctor = ctx
                .global_object()
                .get(boa_engine::js_string!("Range"), ctx)
                .ok();
            let ctor_obj = range_ctor.as_ref().and_then(|v| v.as_object());
            if let Some(ctor) = ctor_obj {
                match ctor.construct(&[], None, ctx) {
                    Ok(v) => return Ok(v.into()),
                    Err(_) => {}
                }
            }
            Ok(JsValue::null())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createRange"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_range).build(),
            )),
        );
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
    let create_doc = NativeFunction::from_copy_closure(|_t, args, ctx| {
        // Create a usable XML document (like new Document() but with namespace support).
        let d = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
        let _ = d.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(9u32)));
        let _ = d.insert_property(boa_engine::js_string!("nodeName"), pd2(JsValue::from(boa_engine::js_string!("#document"))));
        let _ = d.insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::null()));
        let _ = d.insert_property(boa_engine::js_string!("textContent"), pd2(JsValue::null()));
        let _ = d.insert_property(boa_engine::js_string!("contentType"), pd2(JsValue::from(boa_engine::js_string!("application/xml"))));
        let _ = d.insert_property(boa_engine::js_string!("URL"), pd2(JsValue::from(boa_engine::js_string!("about:blank"))));
        let _ = d.insert_property(boa_engine::js_string!("documentURI"), pd2(JsValue::from(boa_engine::js_string!("about:blank"))));
        let _ = d.insert_property(boa_engine::js_string!("ownerDocument"), pd2(JsValue::null()));
        let _ = d.insert_property(boa_engine::js_string!("defaultView"), pd2(JsValue::null()));
        // createElement (no uppercase for XML)
        let doc_ref = d.clone();
        let ce_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_ref, ctx| {
            let tag = arg_to_string(args, 0, ctx);
            let el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = el.insert_property(boa_engine::js_string!("tagName"), pd3(JsValue::from(boa_engine::js_string!(tag.clone()))));
            let _ = el.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!(tag.clone()))));
            let _ = el.insert_property(boa_engine::js_string!("localName"), pd3(JsValue::from(boa_engine::js_string!(tag))));
            let _ = el.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(1u32)));
            let _ = el.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::null()));
            let _ = el.insert_property(boa_engine::js_string!("ownerDocument"), pd3(JsValue::from(doc_ref.clone())));
            let _ = el.insert_property(boa_engine::js_string!("namespaceURI"), pd3(JsValue::null()));
            let _ = el.insert_property(boa_engine::js_string!("prefix"), pd3(JsValue::null()));
            add_dom_methods(&el, ctx);
            Ok(el.into())
        }, doc_ref);
        let _ = d.insert_property(boa_engine::js_string!("createElement"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ce_fn).build())));
        // createTextNode
        let doc_ref2 = d.clone();
        let ct_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_ref2, ctx| {
            let text = arg_string(args, 0);
            let tn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = tn.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(3u32)));
            let _ = tn.insert_property(boa_engine::js_string!("_data"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = tn.insert_property(boa_engine::js_string!("data"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = tn.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = tn.insert_property(boa_engine::js_string!("textContent"), pd3(JsValue::from(boa_engine::js_string!(text))));
            let _ = tn.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!("#text"))));
            let _ = tn.insert_property(boa_engine::js_string!("ownerDocument"), pd3(JsValue::from(doc_ref2.clone())));
            Ok(tn.into())
        }, doc_ref2);
        let _ = d.insert_property(boa_engine::js_string!("createTextNode"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ct_fn).build())));
        // createComment
        let doc_ref3 = d.clone();
        let cc_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_ref3, ctx| {
            let text = arg_string(args, 0);
            let cn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = cn.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(8u32)));
            let _ = cn.insert_property(boa_engine::js_string!("_data"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = cn.insert_property(boa_engine::js_string!("data"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = cn.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::from(boa_engine::js_string!(text))));
            let _ = cn.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!("#comment"))));
            let _ = cn.insert_property(boa_engine::js_string!("ownerDocument"), pd3(JsValue::from(doc_ref3.clone())));
            Ok(cn.into())
        }, doc_ref3);
        let _ = d.insert_property(boa_engine::js_string!("createComment"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cc_fn).build())));
        // createProcessingInstruction
        let doc_ref4 = d.clone();
        let cpi_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_ref4, ctx| {
            let target = arg_string(args, 0);
            let target2 = target.clone();
            let data = arg_string(args, 1);
            let pi = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = pi.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(7u32)));
            let _ = pi.insert_property(boa_engine::js_string!("_data"), pd3(JsValue::from(boa_engine::js_string!(data.clone()))));
            let _ = pi.insert_property(boa_engine::js_string!("data"), pd3(JsValue::from(boa_engine::js_string!(data.clone()))));
            let _ = pi.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::from(boa_engine::js_string!(data))));
            let _ = pi.insert_property(boa_engine::js_string!("target"), pd3(JsValue::from(boa_engine::js_string!(target))));
            let _ = pi.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!(target2))));
            let _ = pi.insert_property(boa_engine::js_string!("ownerDocument"), pd3(JsValue::from(doc_ref4.clone())));
            Ok(pi.into())
        }, doc_ref4);
        let _ = d.insert_property(boa_engine::js_string!("createProcessingInstruction"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cpi_fn).build())));
        // createDocumentFragment
        let df_fn = NativeFunction::from_copy_closure(|_t, _a, ctx| {
            let df = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = df.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(11u32)));
            let _ = df.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!("#document-fragment"))));
            let _ = df.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::null()));
            add_dom_methods(&df, ctx);
            Ok(df.into())
        });
        let _ = d.insert_property(boa_engine::js_string!("createDocumentFragment"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_fn).build())));
        // createRange
        let doc_ref5 = d.clone();
        let cr_fn = NativeFunction::from_copy_closure_with_captures(move |_t, _a, doc_ref5, ctx| {
            let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let r = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = r.insert_property(boa_engine::js_string!("startContainer"), pd3(JsValue::from(doc_ref5.clone())));
            let _ = r.insert_property(boa_engine::js_string!("endContainer"), pd3(JsValue::from(doc_ref5.clone())));
            let _ = r.insert_property(boa_engine::js_string!("startOffset"), pd3(JsValue::from(0u32)));
            let _ = r.insert_property(boa_engine::js_string!("endOffset"), pd3(JsValue::from(0u32)));
            let _ = r.insert_property(boa_engine::js_string!("collapsed"), pd3(JsValue::from(true)));
            let _ = r.insert_property(boa_engine::js_string!("commonAncestorContainer"), pd3(JsValue::from(doc_ref5.clone())));
            Ok(r.into())
        }, doc_ref5);
        let _ = d.insert_property(boa_engine::js_string!("createRange"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cr_fn).build())));
        // Add DOM methods (appendChild etc.) so xmlDoc.appendChild works.
        add_dom_methods(&d, ctx);
        // Set Document.prototype for instanceof + Node method inheritance.
        if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!("Document"), ctx) {
            if let Some(ctor_obj) = ctor_val.as_object() {
                if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                    if let Some(proto) = proto_val.as_object() {
                        let _ = d.set_prototype(Some(proto));
                    }
                }
            }
        }
        // append/prepend (ParentNode interface).
        let doc_ref_app = d.clone();
        let app_fn = NativeFunction::from_copy_closure_with_captures(move |this, args, _doc_ref_app, ctx| {
            if let Some(o) = this.as_object() {
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&o, &child_obj, None, ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        }, doc_ref_app);
        let _ = d.insert_property(boa_engine::js_string!("append"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), app_fn).build())));
        let prep_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            if let Some(o) = this.as_object() {
                let first_child = if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                    if let Some(ca) = cv.as_object() {
                        let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                        if v.is_undefined() { None } else { v.as_object() }
                    } else { None }
                } else { None };
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&o, &child_obj, first_child.clone(), ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        });
        let _ = d.insert_property(boa_engine::js_string!("prepend"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), prep_fn).build())));
        // Handle doctype argument (3rd arg) — set as doctype property.
        if let Some(dt) = args.get(2) {
            if !dt.is_null() && !dt.is_undefined() {
                let _ = d.insert_property(boa_engine::js_string!("doctype"), pd2(dt.clone()));
            }
        }
        Ok(d.into())
    });
    let _ = impl_obj.insert_property(
        boa_engine::js_string!("createDocument"),
        pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_doc).build(),
        )),
    );
    let create_html_doc = NativeFunction::from_copy_closure(|_t, args, ctx| {
        let title = arg_string(args, 0);
        let d = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val).writable(true).enumerable(true).configurable(true).build()
        };
        // Build a proper document with doctype, head, title, body.
        let doctype = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = doctype.insert_property(boa_engine::js_string!("name"), pd(JsValue::from(boa_engine::js_string!("html"))));
        let _ = doctype.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("html"))));
        let _ = doctype.insert_property(boa_engine::js_string!("publicId"), pd(JsValue::from(boa_engine::js_string!(""))));
        let _ = doctype.insert_property(boa_engine::js_string!("systemId"), pd(JsValue::from(boa_engine::js_string!(""))));
        let _ = doctype.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32)));
        let _ = doctype.insert_property(boa_engine::js_string!("ownerDocument"), pd(JsValue::from(d.clone())));
        let _ = d.insert_property(boa_engine::js_string!("doctype"), pd(doctype.into()));
        let _ = d.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(9u32)));
        let _ = d.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("#document"))));
        let _ = d.insert_property(boa_engine::js_string!("contentType"), pd(JsValue::from(boa_engine::js_string!("text/html"))));
        let _ = d.insert_property(boa_engine::js_string!("URL"), pd(JsValue::from(boa_engine::js_string!("about:blank"))));
        let _ = d.insert_property(boa_engine::js_string!("documentURI"), pd(JsValue::from(boa_engine::js_string!("about:blank"))));
        let _ = d.insert_property(boa_engine::js_string!("compatMode"), pd(JsValue::from(boa_engine::js_string!("CSS1Compat"))));
        let _ = d.insert_property(boa_engine::js_string!("characterSet"), pd(JsValue::from(boa_engine::js_string!("UTF-8"))));
        let _ = d.insert_property(boa_engine::js_string!("charset"), pd(JsValue::from(boa_engine::js_string!("UTF-8"))));
        let _ = d.insert_property(boa_engine::js_string!("inputEncoding"), pd(JsValue::from(boa_engine::js_string!("UTF-8"))));
        let _ = d.insert_property(boa_engine::js_string!("location"), pd(JsValue::null()));
        // Build the implementation that knows its owner document.
        let impl2 = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let doc_ref = d.clone();
        let cdt_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args, doc_ref, ctx| {
            let name = arg_string(args, 0);
            let public_id = arg_string(args, 1);
            let system_id = arg_string(args, 2);
            let dt = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val).writable(true).enumerable(true).configurable(true).build()
            };
            let _ = dt.insert_property(boa_engine::js_string!("name"), pd(JsValue::from(boa_engine::js_string!(name.clone()))));
            let _ = dt.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!(name))));
            let _ = dt.insert_property(boa_engine::js_string!("publicId"), pd(JsValue::from(boa_engine::js_string!(public_id))));
            let _ = dt.insert_property(boa_engine::js_string!("systemId"), pd(JsValue::from(boa_engine::js_string!(system_id))));
            // nodeValue and textContent for DocumentType: always null (setter is no-op).
            let null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
            let noop_set = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
            let ng_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_get).build();
            let ns_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_set).build();
            let _ = dt.insert_property(
                boa_engine::js_string!("nodeValue"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(ng_fn.clone()).set(ns_fn.clone())
                    .enumerable(true).configurable(true).build(),
            );
            let _ = dt.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32)));
            let _ = dt.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(ng_fn).set(ns_fn)
                    .enumerable(true).configurable(true).build(),
            );
            let _ = dt.insert_property(boa_engine::js_string!("ownerDocument"), pd(JsValue::from(doc_ref.clone())));
            // Set DocumentType.prototype.
            if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!("DocumentType"), ctx) {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = dt.set_prototype(Some(proto));
                        }
                    }
                }
            }
            Ok(dt.into())
        }, doc_ref);
        let hf_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(true)));
        let _ = impl2.insert_property(boa_engine::js_string!("hasFeature"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), hf_fn).build()))
                .writable(true).enumerable(true).configurable(true).build());
        let _ = impl2.insert_property(boa_engine::js_string!("createDocumentType"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cdt_fn).build()))
                .writable(true).enumerable(true).configurable(true).build());
        let _ = d.insert_property(boa_engine::js_string!("implementation"), pd(impl2.into()));
        // head + body stubs.
        let head = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = head.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("HEAD"))));
        let title_el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = title_el.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("TITLE"))));
        let _ = title_el.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(boa_engine::js_string!(title))));
        let _ = d.insert_property(boa_engine::js_string!("head"), pd(head.into()));
        let body = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = body.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("BODY"))));
        let _ = body.insert_property(boa_engine::js_string!("tagName"), pd(JsValue::from(boa_engine::js_string!("BODY"))));
        let _ = body.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(1u32)));
        add_dom_methods(&body, ctx);
        let _ = d.insert_property(boa_engine::js_string!("body"), pd(body.into()));
        let html_el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = html_el.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("HTML"))));
        let _ = html_el.insert_property(boa_engine::js_string!("tagName"), pd(JsValue::from(boa_engine::js_string!("HTML"))));
        let _ = html_el.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(1u32)));
        add_dom_methods(&html_el, ctx);
        let _ = d.insert_property(boa_engine::js_string!("documentElement"), pd(html_el.into()));
        // createElement on this document (sets ownerDocument = this doc).
        let doc_for_ce = d.clone();
        let ce_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_for_ce, ctx| {
            let tag = arg_to_string(args, 0, ctx);
            let upper: String = tag.chars().map(|c| if c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c }).collect();
            let lower: String = tag.chars().map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c }).collect();
            let el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = el.insert_property(boa_engine::js_string!("tagName"), pd2(JsValue::from(boa_engine::js_string!(upper.clone()))));
            let _ = el.insert_property(boa_engine::js_string!("nodeName"), pd2(JsValue::from(boa_engine::js_string!(upper))));
            let _ = el.insert_property(boa_engine::js_string!("localName"), pd2(JsValue::from(boa_engine::js_string!(lower))));
            let _ = el.insert_property(boa_engine::js_string!("namespaceURI"), pd2(JsValue::from(boa_engine::js_string!("http://www.w3.org/1999/xhtml"))));
            let _ = el.insert_property(boa_engine::js_string!("prefix"), pd2(JsValue::null()));
            let _ = el.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(1u32)));
            let _ = el.insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::null()));
            let _ = el.insert_property(boa_engine::js_string!("ownerDocument"), pd2(JsValue::from(doc_for_ce.clone())));
            add_dom_methods(&el, ctx);
            Ok(el.into())
        }, doc_for_ce);
        let _ = d.insert_property(boa_engine::js_string!("createElement"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ce_fn).build())));
        // createTextNode
        let doc_for_ct = d.clone();
        let ct_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_for_ct, ctx| {
            let text = arg_string(args, 0);
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(3u32)));
            let _ = obj.insert_property(boa_engine::js_string!("_data"), pd2(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = obj.insert_property(boa_engine::js_string!("data"), pd2(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = obj.insert_property(boa_engine::js_string!("textContent"), pd2(JsValue::from(boa_engine::js_string!(text))));
            let _ = obj.insert_property(boa_engine::js_string!("nodeName"), pd2(JsValue::from(boa_engine::js_string!("#text"))));
            let _ = obj.insert_property(boa_engine::js_string!("ownerDocument"), pd2(JsValue::from(doc_for_ct.clone())));
            Ok(obj.into())
        }, doc_for_ct);
        let _ = d.insert_property(boa_engine::js_string!("createTextNode"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ct_fn).build())));
        // createComment
        let doc_for_cc = d.clone();
        let cc_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_for_cc, ctx| {
            let text = arg_string(args, 0);
            let cn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = cn.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(8u32)));
            let _ = cn.insert_property(boa_engine::js_string!("_data"), pd2(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = cn.insert_property(boa_engine::js_string!("data"), pd2(JsValue::from(boa_engine::js_string!(text.clone()))));
            let _ = cn.insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::from(boa_engine::js_string!(text))));
            let _ = cn.insert_property(boa_engine::js_string!("nodeName"), pd2(JsValue::from(boa_engine::js_string!("#comment"))));
            let _ = cn.insert_property(boa_engine::js_string!("ownerDocument"), pd2(JsValue::from(doc_for_cc.clone())));
            Ok(cn.into())
        }, doc_for_cc);
        let _ = d.insert_property(boa_engine::js_string!("createComment"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cc_fn).build())));
        // createDocumentFragment
        let df_fn = NativeFunction::from_copy_closure(|_t, _a, ctx| {
            let df = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = df.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(11u32)));
            let _ = df.insert_property(boa_engine::js_string!("nodeName"), pd2(JsValue::from(boa_engine::js_string!("#document-fragment"))));
            // nodeValue: always null (setter is no-op per spec).
            let null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
            let noop_set = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
            let ng_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_get).build();
            let ns_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_set).build();
            let _ = df.insert_property(
                boa_engine::js_string!("nodeValue"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(ng_fn.clone()).set(ns_fn.clone())
                    .enumerable(true).configurable(true).build(),
            );
            // Initialize _children for textContent setter support.
            let _ = df.insert_property(boa_engine::js_string!("_children"), pd2(JsArray::new(ctx).into()));
            // textContent accessor: getter concatenates children text, setter replaces children.
            let df_tc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    return Ok(JsValue::from(boa_engine::js_string!(collect_text_content(&o, ctx, 0))));
                }
                Ok(JsValue::from(boa_engine::js_string!("")))
            });
            let df_tc_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    let val = args.first().map(|v| v.clone()).unwrap_or(JsValue::null());
                    let s = if val.is_null() {
                        String::new()
                    } else {
                        val.to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default()
                    };
                    let pd3 = |v: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(v).writable(true).enumerable(true).configurable(true).build() };
                    if s.is_empty() {
                        let empty_arr = JsArray::new(ctx);
                        let _ = o.insert_property(boa_engine::js_string!("_children"), pd3(empty_arr.into()));
                    } else {
                        let text_node = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = text_node.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(3u32)));
                        let _ = text_node.insert_property(boa_engine::js_string!("_data"), pd3(JsValue::from(boa_engine::js_string!(s.clone()))));
                        let _ = text_node.insert_property(boa_engine::js_string!("data"), pd3(JsValue::from(boa_engine::js_string!(s.clone()))));
                        let _ = text_node.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::from(boa_engine::js_string!(s.clone()))));
                        let _ = text_node.insert_property(boa_engine::js_string!("textContent"), pd3(JsValue::from(boa_engine::js_string!(s))));
                        let _ = text_node.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!("#text"))));
                        let _ = text_node.insert_property(boa_engine::js_string!("parentNode"), pd3(JsValue::from(o.clone())));
                        if let Ok(text_ctor) = ctx.global_object().get(boa_engine::js_string!("Text"), ctx) {
                            if let Some(tc) = text_ctor.as_object() {
                                if let Ok(proto_val) = tc.get(boa_engine::js_string!("prototype"), ctx) {
                                    if let Some(proto) = proto_val.as_object() {
                                        let _ = text_node.set_prototype(Some(proto));
                                    }
                                }
                            }
                        }
                        let new_arr = JsArray::new(ctx);
                        let _ = new_arr.push(JsValue::from(text_node), ctx);
                        let _ = o.insert_property(boa_engine::js_string!("_children"), pd3(new_arr.into()));
                    }
                }
                Ok(JsValue::undefined())
            });
            let df_tg = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_tc_get).build();
            let df_ts = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_tc_set).build();
            let _ = df.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(df_tg).set(df_ts)
                    .enumerable(true).configurable(true).build(),
            );
            // firstChild/lastChild/childNodes as accessors on _children.
            let fc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if let Some(ca) = cv.as_object() {
                            let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                            if v.is_undefined() { return Ok(JsValue::null()); }
                            return Ok(v);
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let cn_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if cv.is_object() { return Ok(cv); }
                    }
                }
                Ok(JsValue::from(JsArray::new(ctx)))
            });
            let fc_fn2 = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), fc_get).build();
            let cn_fn2 = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cn_get).build();
            let _ = df.insert_property(
                boa_engine::js_string!("firstChild"),
                boa_engine::property::PropertyDescriptor::builder().get(fc_fn2.clone()).enumerable(true).configurable(true).build(),
            );
            let _ = df.insert_property(
                boa_engine::js_string!("lastChild"),
                boa_engine::property::PropertyDescriptor::builder().get(fc_fn2).enumerable(true).configurable(true).build(),
            );
            let _ = df.insert_property(
                boa_engine::js_string!("childNodes"),
                boa_engine::property::PropertyDescriptor::builder().get(cn_fn2).enumerable(true).configurable(true).build(),
            );
            // Set DocumentFragment.prototype.
            if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!("DocumentFragment"), ctx) {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = df.set_prototype(Some(proto));
                        }
                    }
                }
            }
            add_dom_methods(&df, ctx);
            Ok(df.into())
        });
        let _ = d.insert_property(boa_engine::js_string!("createDocumentFragment"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_fn).build())));
        // Add DOM methods to the document itself (appendChild, etc.)
        add_dom_methods(&d, ctx);
        // createRange on this document.
        let doc_for_cr = d.clone();
        let cr_fn = NativeFunction::from_copy_closure_with_captures(move |_t, _a, doc_for_cr, ctx| {
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let r = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = r.insert_property(boa_engine::js_string!("startContainer"), pd2(JsValue::from(doc_for_cr.clone())));
            let _ = r.insert_property(boa_engine::js_string!("endContainer"), pd2(JsValue::from(doc_for_cr.clone())));
            let _ = r.insert_property(boa_engine::js_string!("startOffset"), pd2(JsValue::from(0u32)));
            let _ = r.insert_property(boa_engine::js_string!("endOffset"), pd2(JsValue::from(0u32)));
            let _ = r.insert_property(boa_engine::js_string!("collapsed"), pd2(JsValue::from(true)));
            let _ = r.insert_property(boa_engine::js_string!("commonAncestorContainer"), pd2(JsValue::from(doc_for_cr.clone())));
            // setStart/setEnd
            let ss_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let container = args.first().cloned().unwrap_or(JsValue::null());
                    let offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    let _ = o.insert_property(boa_engine::js_string!("startContainer"), pd3(container));
                    let _ = o.insert_property(boa_engine::js_string!("startOffset"), pd3(JsValue::from(offset)));
                }
                Ok(JsValue::undefined())
            });
            let se_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let container = args.first().cloned().unwrap_or(JsValue::null());
                    let offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    let _ = o.insert_property(boa_engine::js_string!("endContainer"), pd3(container));
                    let _ = o.insert_property(boa_engine::js_string!("endOffset"), pd3(JsValue::from(offset)));
                }
                Ok(JsValue::undefined())
            });
            let _ = r.insert_property(boa_engine::js_string!("setStart"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ss_fn).build())));
            let _ = r.insert_property(boa_engine::js_string!("setEnd"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), se_fn).build())));
            // Register in __active_ranges: clear first, then push (only track current range).
            let global = ctx.global_object();
            if let Ok(ar) = global.get(boa_engine::js_string!("__active_ranges"), ctx) {
                if let Some(arr) = ar.as_object() {
                    // Clear first.
                    if let Ok(clear_val) = arr.get(boa_engine::js_string!("clear"), ctx) {
                        if let Some(clear_fn) = clear_val.as_object() {
                            if clear_fn.is_callable() {
                                let _ = clear_fn.call(&JsValue::from(arr.clone()), &[], ctx);
                            }
                        }
                    }
                    // Then push.
                    if let Ok(push_val) = arr.get(boa_engine::js_string!("push"), ctx) {
                        if let Some(push_fn) = push_val.as_object() {
                            if push_fn.is_callable() {
                                let _ = push_fn.call(&JsValue::from(arr.clone()), &[JsValue::from(r.clone())], ctx);
                            }
                        }
                    }
                }
            }
            Ok(r.into())
        }, doc_for_cr);
        let _ = d.insert_property(boa_engine::js_string!("createRange"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cr_fn).build())));
        add_dom_methods(&d, ctx);
        Ok(d.into())
    });
    let _ = impl_obj.insert_property(
        boa_engine::js_string!("createHTMLDocument"),
        pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_html_doc).build(),
        )),
    );
    let create_dt = NativeFunction::from_copy_closure(|_t, args, ctx| {
        let name = arg_string(args, 0);
        let public_id = arg_string(args, 1);
        let system_id = arg_string(args, 2);
        let d = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val).writable(true).enumerable(true).configurable(true).build()
        };
        let _ = d.insert_property(boa_engine::js_string!("name"), pd(JsValue::from(boa_engine::js_string!(name.clone()))));
        let _ = d.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!(name.clone()))));
        let _ = d.insert_property(boa_engine::js_string!("publicId"), pd(JsValue::from(boa_engine::js_string!(public_id))));
        let _ = d.insert_property(boa_engine::js_string!("systemId"), pd(JsValue::from(boa_engine::js_string!(system_id))));
        // nodeValue and textContent for DocumentType: always null (setter is no-op).
        let null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
        let noop_set = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let ng_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_get).build();
        let ns_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_set).build();
        let _ = d.insert_property(
            boa_engine::js_string!("nodeValue"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(ng_fn.clone()).set(ns_fn.clone())
                .enumerable(true).configurable(true).build(),
        );
        let _ = d.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32))); // DOCUMENT_TYPE_NODE
        let _ = d.insert_property(
            boa_engine::js_string!("textContent"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(ng_fn).set(ns_fn)
                .enumerable(true).configurable(true).build(),
        );
        // ownerDocument = the current document.
        let doc_val = ctx.global_object().get(boa_engine::js_string!("document"), ctx).unwrap_or(JsValue::null());
        let _ = d.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
        // Set DocumentType.prototype for instanceof + Node method inheritance.
        if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!("DocumentType"), ctx) {
            if let Some(ctor_obj) = ctor_val.as_object() {
                if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                    if let Some(proto) = proto_val.as_object() {
                        let _ = d.set_prototype(Some(proto));
                    }
                }
            }
        }
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
            let text = arg_to_string(args, 0, ctx);
            let text_len = text.encode_utf16().count() as u32;
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ =
                obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32))); // TEXT_NODE
            // Store data in an internal _data property (accessor reads/writes this).
            let _ = obj.insert_property(
                boa_engine::js_string!("_data"),
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
            let _ = obj.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(text_len)));
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
            // splitText(offset) — splits the text node at the given offset,
            // returns a new Text node with the second half.
            let split_text = NativeFunction::from_copy_closure(|this, args, ctx| {
                let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                let units = read_data_utf16(&this, ctx);
                let len = units.len() as u32;
                if offset > len {
                    return throw_index_size(ctx);
                }
                let first: Vec<u16> = units[..offset as usize].to_vec();
                let second: Vec<u16> = units[offset as usize..].to_vec();
                write_data_utf16(&this, &first, ctx);
                // Create a new Text node with the second half.
                let new_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let pd = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val).writable(true).enumerable(true).configurable(true).build()
                };
                let _ = new_obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32)));
                let js_str = boa_engine::JsString::from(&second[..]);
                let _ = new_obj.insert_property(boa_engine::js_string!("data"), pd(JsValue::from(js_str.clone())));
                let _ = new_obj.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(js_str)));
                let _ = new_obj.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(second.len() as u32)));
                let _ = new_obj.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("#text"))));
                // Add CharacterData methods
                for (mname, mfn) in build_character_data_methods() {
                    let _ = new_obj.insert_property(
                        boa_engine::js_string!(mname),
                        pd(JsValue::from(
                            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), mfn).build(),
                        )),
                    );
                }
                Ok(new_obj.into())
            });
            let _ = obj.insert_property(
                boa_engine::js_string!("splitText"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), split_text).build(),
                )),
            );
            // wholeText — returns the concatenation of all adjacent text nodes (simplified: just data).
            let whole_text = NativeFunction::from_copy_closure(|this, _args, ctx| {
                let units = read_data_utf16(&this, ctx);
                Ok(JsValue::from(boa_engine::JsString::from(&units[..])))
            });
            let _ = obj.insert_property(
                boa_engine::js_string!("wholeText"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), whole_text).build(),
                )),
            );
            // nodeValue as accessor: getter returns _data, setter updates _data.
            let nv_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                let units = read_data_utf16(&this, ctx);
                Ok(JsValue::from(boa_engine::JsString::from(&units[..])))
            });
            let nv_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                let val = match args.first() {
                    Some(v) if v.is_null() => String::new(),
                    Some(v) => match v.to_string(ctx) {
                        Ok(s) => s.to_std_string_escaped(),
                        Err(_) => String::new(),
                    },
                    None => String::new(),
                };
                let units: Vec<u16> = val.encode_utf16().collect();
                write_data_utf16(&this, &units, ctx);
                Ok(JsValue::undefined())
            });
            let nv_get_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nv_get).build();
            let nv_set_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nv_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeValue"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(nv_get_fn)
                    .set(nv_set_fn)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // data as accessor (same get/set as nodeValue).
            let data_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                let units = read_data_utf16(&this, ctx);
                Ok(JsValue::from(boa_engine::JsString::from(&units[..])))
            });
            let data_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                let val = match args.first() {
                    Some(v) if v.is_null() => String::new(),
                    Some(v) => match v.to_string(ctx) {
                        Ok(s) => s.to_std_string_escaped(),
                        Err(_) => String::new(),
                    },
                    None => String::new(),
                };
                let units: Vec<u16> = val.encode_utf16().collect();
                write_data_utf16(&this, &units, ctx);
                Ok(JsValue::undefined())
            });
            let dg_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), data_get).build();
            let ds_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), data_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("data"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(dg_fn)
                    .set(ds_fn)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // textContent as accessor (same get/set as data/nodeValue).
            let tc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                let units = read_data_utf16(&this, ctx);
                Ok(JsValue::from(boa_engine::JsString::from(&units[..])))
            });
            let tc_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                let val = match args.first() {
                    Some(v) if v.is_null() => String::new(),
                    Some(v) => match v.to_string(ctx) {
                        Ok(s) => s.to_std_string_escaped(),
                        Err(_) => String::new(),
                    },
                    None => String::new(),
                };
                let units: Vec<u16> = val.encode_utf16().collect();
                write_data_utf16(&this, &units, ctx);
                Ok(JsValue::undefined())
            });
            let tcg_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_get).build();
            let tcs_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(tcg_fn)
                    .set(tcs_fn)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // Set prototype to Text.prototype → CharacterData.prototype → Node.prototype
            // so Node constants are inherited.
            if let Ok(text_ctor) = ctx.global_object().get(boa_engine::js_string!("Text"), ctx) {
                if let Some(tc) = text_ctor.as_object() {
                    if let Ok(proto_val) = tc.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = obj.set_prototype(Some(proto));
                        }
                    }
                }
            }
            // ownerDocument — the global document object.
            if let Ok(doc_val) = ctx.global_object().get(boa_engine::js_string!("document"), ctx) {
                let _ = obj.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
            }
            // Text nodes have no children.
            let empty_arr = JsArray::new(ctx);
            let _ = obj.insert_property(boa_engine::js_string!("childNodes"), pd(empty_arr.into()));
            let _ = obj.insert_property(boa_engine::js_string!("firstChild"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("lastChild"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
            let has_child_nodes = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::from(false)));
            let _ = obj.insert_property(
                boa_engine::js_string!("hasChildNodes"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), has_child_nodes).build())),
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
            let text = arg_to_string(args, 0, ctx);
            let text_len = text.encode_utf16().count() as u32;
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ =
                obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(8u32))); // COMMENT_NODE
            // Store in _data (internal) for CharacterData method compatibility.
            let _ = obj.insert_property(
                boa_engine::js_string!("_data"),
                pd(JsValue::from(boa_engine::js_string!(text.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("data"),
                pd(JsValue::from(boa_engine::js_string!(text.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                pd(JsValue::from(boa_engine::js_string!(text.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("length"),
                pd(JsValue::from(text_len)),
            );
            // Store in _data (internal) for CharacterData method compatibility.
            let _ = obj.insert_property(
                boa_engine::js_string!("_data"),
                pd(JsValue::from(boa_engine::js_string!(text.clone()))),
            );
            // data and nodeValue as accessors (same as Text nodes).
            let cd_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                let units = read_data_utf16(&this, ctx);
                Ok(JsValue::from(boa_engine::JsString::from(&units[..])))
            });
            let cd_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                let val = match args.first() {
                    Some(v) if v.is_null() => String::new(),
                    Some(v) => match v.to_string(ctx) {
                        Ok(s) => s.to_std_string_escaped(),
                        Err(_) => String::new(),
                    },
                    None => String::new(),
                };
                let units: Vec<u16> = val.encode_utf16().collect();
                write_data_utf16(&this, &units, ctx);
                Ok(JsValue::undefined())
            });
            let cd_get_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cd_get).build();
            let cd_set_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cd_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("data"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cd_get_fn.clone()).set(cd_set_fn.clone())
                    .enumerable(true).configurable(true).build(),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeValue"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cd_get_fn.clone()).set(cd_set_fn.clone())
                    .enumerable(true).configurable(true).build(),
            );
            // textContent as accessor (same get/set as data/nodeValue).
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cd_get_fn).set(cd_set_fn)
                    .enumerable(true).configurable(true).build(),
            );
            for (mname, mfn) in build_character_data_methods() {
                let _ = obj.insert_property(
                    boa_engine::js_string!(mname),
                    pd(JsValue::from(
                        boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), mfn).build(),
                    )),
                );
            }
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeName"),
                pd(JsValue::from(boa_engine::js_string!("#comment"))),
            );
            // Set Comment.prototype for instanceof + Node constant inheritance.
            if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!("Comment"), ctx) {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = obj.set_prototype(Some(proto));
                        }
                    }
                }
            }
            // ownerDocument — the global document object.
            if let Ok(doc_val) = ctx.global_object().get(boa_engine::js_string!("document"), ctx) {
                let _ = obj.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
            }
            // Comment nodes have no children.
            let empty_arr2 = JsArray::new(ctx);
            let _ = obj.insert_property(boa_engine::js_string!("childNodes"), pd(empty_arr2.into()));
            let _ = obj.insert_property(boa_engine::js_string!("firstChild"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("lastChild"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
            let has_child_nodes2 = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::from(false)));
            let _ = obj.insert_property(
                boa_engine::js_string!("hasChildNodes"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), has_child_nodes2).build())),
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
            // nodeValue: always null (setter is no-op per spec).
            let null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
            let noop_set = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
            let ng_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_get).build();
            let ns_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeValue"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(ng_fn.clone()).set(ns_fn.clone())
                    .enumerable(true).configurable(true).build(),
            );
            // Initialize _children for textContent/firstChild/childNodes support.
            let _ = obj.insert_property(boa_engine::js_string!("_children"), pd(JsArray::new(ctx).into()));
            // textContent accessor.
            let df_tc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    return Ok(JsValue::from(boa_engine::js_string!(collect_text_content(&o, ctx, 0))));
                }
                Ok(JsValue::from(boa_engine::js_string!("")))
            });
            let df_tc_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    let val = args.first().map(|v| v.clone()).unwrap_or(JsValue::null());
                    let s = if val.is_null() { String::new() }
                    else { val.to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default() };
                    let pd3 = |v: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(v).writable(true).enumerable(true).configurable(true).build() };
                    if s.is_empty() {
                        let empty_arr = JsArray::new(ctx);
                        let _ = o.insert_property(boa_engine::js_string!("_children"), pd3(empty_arr.into()));
                    } else {
                        let tn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = tn.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(3u32)));
                        let _ = tn.insert_property(boa_engine::js_string!("_data"), pd3(JsValue::from(boa_engine::js_string!(s.clone()))));
                        let _ = tn.insert_property(boa_engine::js_string!("data"), pd3(JsValue::from(boa_engine::js_string!(s.clone()))));
                        let _ = tn.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::from(boa_engine::js_string!(s.clone()))));
                        let _ = tn.insert_property(boa_engine::js_string!("textContent"), pd3(JsValue::from(boa_engine::js_string!(s))));
                        let _ = tn.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!("#text"))));
                        let _ = tn.insert_property(boa_engine::js_string!("parentNode"), pd3(JsValue::from(o.clone())));
                        if let Ok(text_ctor) = ctx.global_object().get(boa_engine::js_string!("Text"), ctx) {
                            if let Some(tc) = text_ctor.as_object() {
                                if let Ok(pv) = tc.get(boa_engine::js_string!("prototype"), ctx) {
                                    if let Some(p) = pv.as_object() { let _ = tn.set_prototype(Some(p)); }
                                }
                            }
                        }
                        let new_arr = JsArray::new(ctx);
                        let _ = new_arr.push(JsValue::from(tn), ctx);
                        let _ = o.insert_property(boa_engine::js_string!("_children"), pd3(new_arr.into()));
                    }
                }
                Ok(JsValue::undefined())
            });
            let df_tg = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_tc_get).build();
            let df_ts = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_tc_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(df_tg).set(df_ts).enumerable(true).configurable(true).build(),
            );
            // firstChild/lastChild/childNodes accessors.
            let fc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if let Some(ca) = cv.as_object() {
                            let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                            if v.is_undefined() { return Ok(JsValue::null()); }
                            return Ok(v);
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let cn_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if cv.is_object() { return Ok(cv); }
                    }
                }
                Ok(JsValue::from(JsArray::new(ctx)))
            });
            let fc_fn3 = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), fc_get).build();
            let cn_fn3 = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cn_get).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("firstChild"),
                boa_engine::property::PropertyDescriptor::builder().get(fc_fn3.clone()).enumerable(true).configurable(true).build(),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("lastChild"),
                boa_engine::property::PropertyDescriptor::builder().get(fc_fn3).enumerable(true).configurable(true).build(),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("childNodes"),
                boa_engine::property::PropertyDescriptor::builder().get(cn_fn3).enumerable(true).configurable(true).build(),
            );
            // Set DocumentFragment.prototype.
            if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!("DocumentFragment"), ctx) {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = obj.set_prototype(Some(proto));
                        }
                    }
                }
            }
            add_dom_methods(&obj, ctx);
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createDocumentFragment"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_frag).build(),
            )),
        );

        // document.append(...nodes) — ParentNode method.
        let doc_append = NativeFunction::from_copy_closure(|this, args, ctx| {
            if let Some(o) = this.as_object() {
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&o, &child_obj, None, ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("append"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), doc_append).build())),
        );
        // document.prepend(...nodes) — ParentNode method.
        let doc_prepend = NativeFunction::from_copy_closure(|this, args, ctx| {
            if let Some(o) = this.as_object() {
                // Find first child (reference node).
                let first_child = if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                    if let Some(ca) = cv.as_object() {
                        let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                        if v.is_undefined() { None } else { v.as_object() }
                    } else { None }
                } else { None };
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&o, &child_obj, first_child.clone(), ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("prepend"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), doc_prepend).build())),
        );

        // document.createProcessingInstruction(target, data)
        let create_pi = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let target = arg_string(args, 0);
            let data = arg_string(args, 1);
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(7u32))); // PROCESSING_INSTRUCTION_NODE
            let _ = obj.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!(target.clone()))));
            let _ = obj.insert_property(boa_engine::js_string!("target"), pd(JsValue::from(boa_engine::js_string!(target))));
            // data and nodeValue are the same for PI. Use accessors that read/write _data.
            let _ = obj.insert_property(boa_engine::js_string!("_data"), pd(JsValue::from(boa_engine::js_string!(data))));
            let pi_data_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                let units = read_data_utf16(&this, ctx);
                Ok(JsValue::from(boa_engine::JsString::from(&units[..])))
            });
            let pi_data_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                let val = match args.first() {
                    Some(v) if v.is_null() => String::new(),
                    Some(v) => match v.to_string(ctx) {
                        Ok(s) => s.to_std_string_escaped(),
                        Err(_) => String::new(),
                    },
                    None => String::new(),
                };
                let units: Vec<u16> = val.encode_utf16().collect();
                write_data_utf16(&this, &units, ctx);
                Ok(JsValue::undefined())
            });
            let pi_dg = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pi_data_get).build();
            let pi_ds = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pi_data_set).build();
            for prop_name in &["data", "nodeValue", "textContent"] {
                let _ = obj.insert_property(
                    boa_engine::js_string!(*prop_name),
                    boa_engine::property::PropertyDescriptor::builder()
                        .get(pi_dg.clone()).set(pi_ds.clone())
                        .enumerable(true).configurable(true).build(),
                );
            }
            // Add CharacterData methods.
            for (mname, mfn) in build_character_data_methods() {
                let _ = obj.insert_property(
                    boa_engine::js_string!(mname),
                    pd(JsValue::from(
                        boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), mfn).build(),
                    )),
                );
            }
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createProcessingInstruction"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_pi).build(),
            )),
        );

        // document.createCDATASection(data)
        let create_cdata = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let data = arg_string(args, 0);
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |v: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(v).writable(true).enumerable(true).configurable(true).build() };
            let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(4u32)));
            let _ = obj.insert_property(boa_engine::js_string!("_data"), pd2(JsValue::from(boa_engine::js_string!(data.clone()))));
            let _ = obj.insert_property(boa_engine::js_string!("data"), pd2(JsValue::from(boa_engine::js_string!(data.clone()))));
            let _ = obj.insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::from(boa_engine::js_string!(data.clone()))));
            let _ = obj.insert_property(boa_engine::js_string!("textContent"), pd2(JsValue::from(boa_engine::js_string!(data))));
            let _ = obj.insert_property(boa_engine::js_string!("nodeName"), pd2(JsValue::from(boa_engine::js_string!("#cdata-section"))));
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createCDATASection"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_cdata).build())),
        );

        // document.createAttribute(name)
        let create_attr = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let name = arg_string(args, 0);
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |v: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(v).writable(true).enumerable(true).configurable(true).build() };
            let _ = obj.insert_property(boa_engine::js_string!("name"), pd2(JsValue::from(boa_engine::js_string!(name.clone()))));
            let _ = obj.insert_property(boa_engine::js_string!("nodeName"), pd2(JsValue::from(boa_engine::js_string!(name.clone()))));
            let _ = obj.insert_property(boa_engine::js_string!("localName"), pd2(JsValue::from(boa_engine::js_string!(name))));
            let _ = obj.insert_property(boa_engine::js_string!("value"), pd2(JsValue::from(boa_engine::js_string!(""))));
            let _ = obj.insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::from(boa_engine::js_string!(""))));
            let _ = obj.insert_property(boa_engine::js_string!("textContent"), pd2(JsValue::from(boa_engine::js_string!(""))));
            let _ = obj.insert_property(boa_engine::js_string!("prefix"), pd2(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("namespaceURI"), pd2(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("specified"), pd2(JsValue::from(true)));
            let _ = obj.insert_property(boa_engine::js_string!("ownerElement"), pd2(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(2u32)));
            // Set Attr.prototype as the prototype.
            if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!("Attr"), ctx) {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = obj.set_prototype(Some(proto));
                        }
                    }
                }
            }
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createAttribute"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_attr).build())),
        );

        // document.createTreeWalker(root, whatToShow, filter)
        let create_tw = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let root = args.first().cloned().unwrap_or(JsValue::null());
            let what_to_show = args.get(1).cloned().unwrap_or(JsValue::from(0xFFFFFFFFu32));
            let filter = args.get(2).cloned().unwrap_or(JsValue::null());
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(false).enumerable(true).configurable(false).build() };
            let _ = obj.insert_property(boa_engine::js_string!("root"), pd2(root.clone()));
            let _ = obj.insert_property(boa_engine::js_string!("whatToShow"), pd2(what_to_show));
            let _ = obj.insert_property(boa_engine::js_string!("filter"), pd2(filter));
            let _ = obj.insert_property(boa_engine::js_string!("currentNode"),
                boa_engine::property::PropertyDescriptor::builder().value(root).writable(true).enumerable(true).configurable(true).build());
            // parentNode, firstChild, lastChild, previousSibling, nextSibling — stubs returning null.
            let null_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::null()));
            for mname in &["parentNode", "firstChild", "lastChild", "previousSibling", "nextSibling"] {
                let _ = obj.insert_property(boa_engine::js_string!(*mname),
                    pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_fn.clone()).build())));
            }
            // Set TreeWalker.prototype and Symbol.toStringTag.
            if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!("TreeWalker"), ctx) {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = obj.set_prototype(Some(proto));
                        }
                    }
                }
            }
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createTreeWalker"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_tw).build())),
        );

        // document.createNodeIterator(root, whatToShow, filter)
        let create_ni = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let root = args.first().cloned().unwrap_or(JsValue::null());
            let what_to_show = args.get(1).cloned().unwrap_or(JsValue::from(0xFFFFFFFFu32));
            let filter = args.get(2).cloned().unwrap_or(JsValue::null());
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(false).enumerable(true).configurable(false).build() };
            let _ = obj.insert_property(boa_engine::js_string!("root"), pd2(root.clone()));
            let _ = obj.insert_property(boa_engine::js_string!("whatToShow"), pd2(what_to_show));
            let _ = obj.insert_property(boa_engine::js_string!("filter"), pd2(filter));
            let _ = obj.insert_property(boa_engine::js_string!("referenceNode"),
                boa_engine::property::PropertyDescriptor::builder().value(root).writable(true).enumerable(true).configurable(true).build());
            let null_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::null()));
            for mname in &["nextNode", "previousNode", "detach"] {
                let _ = obj.insert_property(boa_engine::js_string!(*mname),
                    pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_fn.clone()).build())));
            }
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createNodeIterator"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_ni).build())),
        );

        // document.adoptNode(node) — removes from parent, sets ownerDocument, returns node.
        let adopt_node = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let node = args.first().cloned().unwrap_or(JsValue::null());
            if let Some(node_obj) = node.as_object() {
                let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                // Remove from parent.
                let parent = node_obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                    .and_then(|v| v.as_object());
                if let Some(parent_obj) = parent {
                    remove_from_children(&parent_obj, &node_obj, ctx);
                }
                let _ = node_obj.insert_property(boa_engine::js_string!("parentNode"), pd2(JsValue::null()));
                let _ = node_obj.insert_property(boa_engine::js_string!("parentElement"), pd2(JsValue::null()));
                // Set ownerDocument to this document.
                let doc_val = ctx.global_object().get(boa_engine::js_string!("document"), ctx).unwrap_or(JsValue::null());
                let _ = node_obj.insert_property(boa_engine::js_string!("ownerDocument"), pd2(doc_val));
            }
            Ok(node)
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("adoptNode"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), adopt_node).build(),
            )),
        );

        // document.importNode(node, deep?) — returns a clone of the node.
        let import_node = NativeFunction::from_copy_closure(|this, args, ctx| {
            let node = args.first().cloned().unwrap_or(JsValue::null());
            let deep = args.get(1).and_then(|v| v.as_boolean()).unwrap_or(false);
            // Use cloneNode on the node if available.
            if let Some(o) = node.as_object() {
                if let Ok(cn) = o.get(boa_engine::js_string!("cloneNode"), ctx) {
                    if let Some(cn_fn) = cn.as_object() {
                        if cn_fn.is_callable() {
                            return cn_fn.call(&node, &[JsValue::from(deep)], ctx);
                        }
                    }
                }
            }
            Ok(node)
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("importNode"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), import_node).build(),
            )),
        );

        // document.createEvent(type) — returns a new Event-like object.
        let create_event = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let event_type = arg_string(args, 0);
            // Whitelist of createEvent-compatible event types (from DOM spec).
            let iface_name = match event_type.as_str() {
                "BeforeUnloadEvent" => "BeforeUnloadEvent",
                "CompositionEvent" => "CompositionEvent",
                "CustomEvent" => "CustomEvent",
                "DeviceMotionEvent" => "DeviceMotionEvent",
                "DeviceOrientationEvent" => "DeviceOrientationEvent",
                "DragEvent" => "DragEvent",
                "Event" | "Events" | "HTMLEvents" | "SVGEvents" => "Event",
                "FocusEvent" => "FocusEvent",
                "HashChangeEvent" => "HashChangeEvent",
                "KeyboardEvent" => "KeyboardEvent",
                "MessageEvent" => "MessageEvent",
                "MouseEvent" | "MouseEvents" => "MouseEvent",
                "StorageEvent" => "StorageEvent",
                "TextEvent" => "TextEvent",
                "TouchEvent" => "TouchEvent",
                "UIEvent" | "UIEvents" => "UIEvent",
                "WheelEvent" => "WheelEvent",
                _ => {
                    // Unknown type → throw NOT_SUPPORTED_ERR.
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NOT_SUPPORTED_ERR: The provided event type is not supported")
                        .into());
                }
            };
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val).writable(true).enumerable(true).configurable(true).build()
            };
            let _ = obj.insert_property(boa_engine::js_string!("type"), pd(JsValue::from(boa_engine::js_string!(""))));
            let _ = obj.insert_property(boa_engine::js_string!("bubbles"), pd(JsValue::from(false)));
            let _ = obj.insert_property(boa_engine::js_string!("cancelable"), pd(JsValue::from(false)));
            let _ = obj.insert_property(boa_engine::js_string!("composed"), pd(JsValue::from(false)));
            let _ = obj.insert_property(boa_engine::js_string!("defaultPrevented"), pd(JsValue::from(false)));
            let _ = obj.insert_property(boa_engine::js_string!("returnValue"), pd(JsValue::from(true)));
            let _ = obj.insert_property(boa_engine::js_string!("isTrusted"), pd(JsValue::from(false)));
            let _ = obj.insert_property(boa_engine::js_string!("target"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("currentTarget"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("srcElement"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("timeStamp"), pd(JsValue::from(0u32)));
            let _ = obj.insert_property(boa_engine::js_string!("eventPhase"), pd(JsValue::from(0u32)));
            let _ = obj.insert_property(boa_engine::js_string!("cancelBubble"), pd(JsValue::from(false)));

            // Look up the constructor on the global object and set prototype.
            if let Ok(ctor_val) = ctx.global_object().get(boa_engine::js_string!(iface_name), ctx) {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = obj.set_prototype(Some(proto));
                        }
                    }
                }
            }

            // initEvent(type, bubbles, cancelable)
            let init_ev = NativeFunction::from_copy_closure(|this, args, ctx| {
                // First parameter (type) is mandatory.
                if args.is_empty() || args.first().map(|v| v.is_undefined()).unwrap_or(true) {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("initEvent requires at least 1 argument")
                        .into());
                }
                if let Some(o) = this.as_object() {
                    let pd = |val: JsValue| {
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(val).writable(true).enumerable(true).configurable(true).build()
                    };
                    let t = arg_string(args, 0);
                    let b = args.get(1).and_then(|v| v.as_boolean()).unwrap_or(false);
                    let c = args.get(2).and_then(|v| v.as_boolean()).unwrap_or(false);
                    let _ = o.insert_property(boa_engine::js_string!("type"), pd(JsValue::from(boa_engine::js_string!(t))));
                    let _ = o.insert_property(boa_engine::js_string!("bubbles"), pd(JsValue::from(b)));
                    let _ = o.insert_property(boa_engine::js_string!("cancelable"), pd(JsValue::from(c)));
                    let _ = o.insert_property(boa_engine::js_string!("defaultPrevented"), pd(JsValue::from(false)));
                    let _ = o.insert_property(boa_engine::js_string!("returnValue"), pd(JsValue::from(true)));
                    let _ = o.insert_property(boa_engine::js_string!("isTrusted"), pd(JsValue::from(false)));
                    let _ = o.insert_property(boa_engine::js_string!("target"), pd(JsValue::null()));
                    let _ = o.insert_property(boa_engine::js_string!("srcElement"), pd(JsValue::null()));
                }
                Ok(JsValue::undefined())
            });
            let _ = obj.insert_property(boa_engine::js_string!("initEvent"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), init_ev).build())));
            // preventDefault with _passive check
            let pd_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    let cancelable = o.get(boa_engine::js_string!("cancelable"), ctx).ok()
                        .and_then(|v| v.as_boolean()).unwrap_or(false);
                    let passive = o.get(boa_engine::js_string!("_passive"), ctx).ok()
                        .and_then(|v| v.as_boolean()).unwrap_or(false);
                    if cancelable && !passive {
                        let pd2 = |val: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(val).writable(true).enumerable(true).configurable(true).build()
                        };
                        let _ = o.insert_property(boa_engine::js_string!("defaultPrevented"), pd2(JsValue::from(true)));
                        let _ = o.insert_property(boa_engine::js_string!("returnValue"), pd2(JsValue::from(false)));
                    }
                }
                Ok(JsValue::undefined())
            });
            let _ = obj.insert_property(boa_engine::js_string!("preventDefault"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pd_fn).build())));
            let stop_fn = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
            let _ = obj.insert_property(boa_engine::js_string!("stopPropagation"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), stop_fn.clone()).build())));
            let _ = obj.insert_property(boa_engine::js_string!("stopImmediatePropagation"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), stop_fn).build())));
            // For CustomEvent: add detail property and initCustomEvent method.
            if iface_name == "CustomEvent" {
                let _ = obj.insert_property(boa_engine::js_string!("detail"), pd(JsValue::null()));
                let ice_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                    // First parameter (type) is mandatory.
                    if args.is_empty() || args.first().map(|v| v.is_undefined()).unwrap_or(true) {
                        return Err(boa_engine::JsNativeError::typ()
                            .with_message("initCustomEvent requires at least 1 argument")
                            .into());
                    }
                    if let Some(o) = this.as_object() {
                        let pd2 = |val: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(val).writable(true).enumerable(true).configurable(true).build()
                        };
                        let t = arg_string(args, 0);
                        let b = args.get(1).and_then(|v| v.as_boolean()).unwrap_or(false);
                        let c = args.get(2).and_then(|v| v.as_boolean()).unwrap_or(false);
                        let detail = args.get(3).cloned().unwrap_or(JsValue::null());
                        let _ = o.insert_property(boa_engine::js_string!("type"), pd2(JsValue::from(boa_engine::js_string!(t))));
                        let _ = o.insert_property(boa_engine::js_string!("bubbles"), pd2(JsValue::from(b)));
                        let _ = o.insert_property(boa_engine::js_string!("cancelable"), pd2(JsValue::from(c)));
                        let _ = o.insert_property(boa_engine::js_string!("detail"), pd2(detail));
                    }
                    Ok(JsValue::undefined())
                });
                let _ = obj.insert_property(boa_engine::js_string!("initCustomEvent"),
                    pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ice_fn).build())));
            }
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createEvent"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_event).build(),
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
        // Document.textContent is always null (getter returns null, setter is no-op).
        let tc_null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
        let tc_noop_set = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let tc_ng = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_null_get).build();
        let tc_ns = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_noop_set).build();
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("textContent"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(tc_ng.clone()).set(tc_ns.clone())
                .enumerable(true).configurable(true).build(),
        );
        // Document.nodeValue is always null (setter is no-op).
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("nodeValue"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(tc_ng).set(tc_ns)
                .enumerable(true).configurable(true).build(),
        );
        let _ = doc_obj.insert_property(boa_engine::js_string!("body"), pd(JsValue::null()));
        let _ = doc_obj.insert_property(boa_engine::js_string!("head"), pd(JsValue::null()));
        // document.defaultView = window (the global object).
        let win = ctx.global_object();
        let _ = doc_obj.insert_property(boa_engine::js_string!("defaultView"), pd(win.into()));
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
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("charset"),
            pd(JsValue::from(boa_engine::js_string!("UTF-8"))),
        );
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("inputEncoding"),
            pd(JsValue::from(boa_engine::js_string!("UTF-8"))),
        );
        // nodeName = "#document" for Document nodes.
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("#document"))),
        );
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("nodeType"),
            pd(JsValue::from(9u32)),
        );
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("textContent"),
            pd(JsValue::null()),
        );
        // doctype stub — the real doctype comes from the parsed HTML.
        let doctype_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("name"),
            pd(JsValue::from(boa_engine::js_string!("html"))),
        );
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("html"))),
        );
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("publicId"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("systemId"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("nodeType"),
            pd(JsValue::from(10u32)),
        );
        // nodeValue and textContent for DocumentType: always null (setter is no-op).
        let dt_null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
        let dt_noop_set = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let dt_ng = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), dt_null_get).build();
        let dt_ns = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), dt_noop_set).build();
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("textContent"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(dt_ng.clone()).set(dt_ns.clone())
                .enumerable(true).configurable(true).build(),
        );
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("nodeValue"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(dt_ng).set(dt_ns)
                .enumerable(true).configurable(true).build(),
        );
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("doctype"),
            pd(doctype_obj.into()),
        );
    }

    // Global constructors for instanceof checks: Node, Element, Text, Comment,
    // DocumentFragment, plus all HTMLElement subclasses that WPT tests check.
    let all_types = [
        ("Node", 0u32),
        ("Element", 1),
        ("HTMLElement", 1),
        ("Text", 3),
        ("Comment", 8),
        ("DocumentFragment", 11),
        ("Document", 9),
        ("XMLDocument", 9),
        ("CharacterData", 0),
        // HTML element subclasses (all use nodeType=1, same constructor body).
        ("HTMLAnchorElement", 1),
        ("HTMLAreaElement", 1),
        ("HTMLAudioElement", 1),
        ("HTMLBRElement", 1),
        ("HTMLBaseElement", 1),
        ("HTMLBodyElement", 1),
        ("HTMLButtonElement", 1),
        ("HTMLCanvasElement", 1),
        ("HTMLDListElement", 1),
        ("HTMLDataElement", 1),
        ("HTMLDataListElement", 1),
        ("HTMLDetailsElement", 1),
        ("HTMLDialogElement", 1),
        ("HTMLDirectoryElement", 1),
        ("HTMLDivElement", 1),
        ("HTMLEmbedElement", 1),
        ("HTMLFieldSetElement", 1),
        ("HTMLFontElement", 1),
        ("HTMLFormElement", 1),
        ("HTMLFrameElement", 1),
        ("HTMLFrameSetElement", 1),
        ("HTMLHRElement", 1),
        ("HTMLHeadElement", 1),
        ("HTMLHeadingElement", 1),
        ("HTMLHtmlElement", 1),
        ("HTMLIFrameElement", 1),
        ("HTMLImageElement", 1),
        ("HTMLInputElement", 1),
        ("HTMLLIElement", 1),
        ("HTMLLabelElement", 1),
        ("HTMLLegendElement", 1),
        ("HTMLLinkElement", 1),
        ("HTMLMapElement", 1),
        ("HTMLMediaElement", 1),
        ("HTMLMenuElement", 1),
        ("HTMLMetaElement", 1),
        ("HTMLMeterElement", 1),
        ("HTMLModElement", 1),
        ("HTMLOListElement", 1),
        ("HTMLObjectElement", 1),
        ("HTMLOptGroupElement", 1),
        ("HTMLOptionElement", 1),
        ("HTMLOutputElement", 1),
        ("HTMLParagraphElement", 1),
        ("HTMLParamElement", 1),
        ("HTMLPictureElement", 1),
        ("HTMLPreElement", 1),
        ("HTMLProgressElement", 1),
        ("HTMLQuoteElement", 1),
        ("HTMLScriptElement", 1),
        ("HTMLSelectElement", 1),
        ("HTMLSlotElement", 1),
        ("HTMLSourceElement", 1),
        ("HTMLSpanElement", 1),
        ("HTMLStyleElement", 1),
        ("HTMLTableCaptionElement", 1),
        ("HTMLTableCellElement", 1),
        ("HTMLTableColElement", 1),
        ("HTMLTableElement", 1),
        ("HTMLTableRowElement", 1),
        ("HTMLTableSectionElement", 1),
        ("HTMLTemplateElement", 1),
        ("HTMLTextAreaElement", 1),
        ("HTMLTimeElement", 1),
        ("HTMLTitleElement", 1),
        ("HTMLTrackElement", 1),
        ("HTMLUListElement", 1),
        ("HTMLUnknownElement", 1),
        ("HTMLVideoElement", 1),
        // SVG element
        ("SVGElement", 1),
        ("MathMLElement", 1),
        // Event interfaces (for createEvent prototype checks)
        ("UIEvent", 0),
        ("FocusEvent", 0),
        ("MouseEvent", 0),
        ("KeyboardEvent", 0),
        ("WheelEvent", 0),
        ("BeforeUnloadEvent", 0),
        ("CompositionEvent", 0),
        ("DeviceMotionEvent", 0),
        ("DeviceOrientationEvent", 0),
        ("DragEvent", 0),
        ("HashChangeEvent", 0),
        ("MessageEvent", 0),
        ("StorageEvent", 0),
        ("TextEvent", 0),
        ("TouchEvent", 0),
        ("AnimationEvent", 0),
        ("TransitionEvent", 0),
        ("PageTransitionEvent", 0),
        ("BeforeInputEvent", 0),
        ("InputEvent", 0),
        ("CloseEvent", 0),
        ("ErrorEvent", 0),
        ("ProgressEvent", 0),
        ("SecurityPolicyViolationEvent", 0),
        // Traversal interfaces
        ("TreeWalker", 0),
        ("NodeIterator", 0),
        ("NodeFilter", 0),
        // Attr
        ("Attr", 2),
        ("NamedNodeMap", 0),
        // Additional DOM interfaces for interface-objects test
        ("AbortController", 0),
        ("AbortSignal", 0),
        ("DOMImplementation", 0),
        ("ProcessingInstruction", 7),
        ("NodeList", 0),
        ("HTMLCollection", 0),
        ("DOMTokenList", 0),
        ("DocumentType", 10),
        ("CharacterData", 0),
        ("CDATASection", 4),
        ("ShadowRoot", 11),
    ];
    for (name, nt) in all_types {
        let nt = nt.clone();
        let name_for_proto = name.to_string();
        let ctor_fn = NativeFunction::from_copy_closure_with_captures(move |this, _a, name_for_proto, ctx| {
            // Use `this` from Boa's OrdinaryCallConstruct (has correct prototype).
            let obj = this.as_object().unwrap_or_else(|| boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()));
            // Explicitly set prototype to ConstructorName.prototype so instanceof works.
            // Boa's OrdinaryCallConstruct may not set the prototype correctly for native closures.
            let global = ctx.global_object();
            if let Ok(ctor_val) = global.get(boa_engine::JsString::from(name_for_proto.as_str()), &mut *ctx) {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), &mut *ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = obj.set_prototype(Some(proto));
                        }
                    }
                }
            }
            let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(nt)));
            // For Document constructor, add document-level methods.
            if name == "Document" {
                let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                let _ = obj.insert_property(boa_engine::js_string!("nodeName"), pd2(JsValue::from(boa_engine::js_string!("#document"))));
                let _ = obj.insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::null()));
                let _ = obj.insert_property(boa_engine::js_string!("textContent"), pd2(JsValue::null()));
                let _ = obj.insert_property(boa_engine::js_string!("contentType"), pd2(JsValue::from(boa_engine::js_string!("application/xml"))));
                let _ = obj.insert_property(boa_engine::js_string!("URL"), pd2(JsValue::from(boa_engine::js_string!("about:blank"))));
                let _ = obj.insert_property(boa_engine::js_string!("documentURI"), pd2(JsValue::from(boa_engine::js_string!("about:blank"))));
                let _ = obj.insert_property(boa_engine::js_string!("compatMode"), pd2(JsValue::from(boa_engine::js_string!("CSS1Compat"))));
                let _ = obj.insert_property(boa_engine::js_string!("characterSet"), pd2(JsValue::from(boa_engine::js_string!("UTF-8"))));
                let _ = obj.insert_property(boa_engine::js_string!("charset"), pd2(JsValue::from(boa_engine::js_string!("UTF-8"))));
                let _ = obj.insert_property(boa_engine::js_string!("inputEncoding"), pd2(JsValue::from(boa_engine::js_string!("UTF-8"))));
                let _ = obj.insert_property(boa_engine::js_string!("ownerDocument"), pd2(JsValue::null()));
                let _ = obj.insert_property(boa_engine::js_string!("defaultView"), pd2(ctx.global_object().into()));
                // createElement — new Document() is XML, so no ASCII lowercasing, constructor is Element
                let doc_ref = obj.clone();
                let ce_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_ref, ctx| {
                    let tag = arg_to_string(args, 0, ctx);
                    let el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let _ = el.insert_property(boa_engine::js_string!("tagName"), pd3(JsValue::from(boa_engine::js_string!(tag.clone()))));
                    let _ = el.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!(tag.clone()))));
                    let _ = el.insert_property(boa_engine::js_string!("localName"), pd3(JsValue::from(boa_engine::js_string!(tag.clone()))));
                    let _ = el.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(1u32)));
                    let _ = el.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::null()));
                    let _ = el.insert_property(boa_engine::js_string!("ownerDocument"), pd3(JsValue::from(doc_ref.clone())));
                    let _ = el.insert_property(boa_engine::js_string!("namespaceURI"), pd3(JsValue::null()));
                    let _ = el.insert_property(boa_engine::js_string!("prefix"), pd3(JsValue::null()));
                    // Set prototype to Element.prototype so constructor === Element
                    let global = ctx.global_object();
                    if let Ok(elem_ctor_val) = global.get(boa_engine::js_string!("Element"), &mut *ctx) {
                        if let Some(elem_ctor) = elem_ctor_val.as_object() {
                            if let Ok(elem_proto_val) = elem_ctor.get(boa_engine::js_string!("prototype"), &mut *ctx) {
                                if let Some(elem_proto) = elem_proto_val.as_object() {
                                    let _ = el.set_prototype(Some(elem_proto));
                                }
                            }
                        }
                    }
                    add_dom_methods(&el, ctx);
                    Ok(el.into())
                }, doc_ref);
                let _ = obj.insert_property(boa_engine::js_string!("createElement"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ce_fn).build())));
                // createTextNode
                let doc_ref2 = obj.clone();
                let ct_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_ref2, ctx| {
                    let text = arg_string(args, 0);
                    let tn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let _ = tn.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(3u32)));
                    let _ = tn.insert_property(boa_engine::js_string!("_data"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
                    let _ = tn.insert_property(boa_engine::js_string!("data"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
                    let _ = tn.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
                    let _ = tn.insert_property(boa_engine::js_string!("textContent"), pd3(JsValue::from(boa_engine::js_string!(text))));
                    let _ = tn.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!("#text"))));
                    let _ = tn.insert_property(boa_engine::js_string!("ownerDocument"), pd3(JsValue::from(doc_ref2.clone())));
                    Ok(tn.into())
                }, doc_ref2);
                let _ = obj.insert_property(boa_engine::js_string!("createTextNode"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ct_fn).build())));
                // createComment
                let doc_ref3 = obj.clone();
                let cc_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_ref3, ctx| {
                    let text = arg_string(args, 0);
                    let cn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let _ = cn.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(8u32)));
                    let _ = cn.insert_property(boa_engine::js_string!("_data"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
                    let _ = cn.insert_property(boa_engine::js_string!("data"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
                    let _ = cn.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::from(boa_engine::js_string!(text.clone()))));
                    let _ = cn.insert_property(boa_engine::js_string!("textContent"), pd3(JsValue::from(boa_engine::js_string!(text))));
                    let _ = cn.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!("#comment"))));
                    let _ = cn.insert_property(boa_engine::js_string!("ownerDocument"), pd3(JsValue::from(doc_ref3.clone())));
                    Ok(cn.into())
                }, doc_ref3);
                let _ = obj.insert_property(boa_engine::js_string!("createComment"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cc_fn).build())));
                // createCDATASection
                let doc_ref4 = obj.clone();
                let cd_fn = NativeFunction::from_copy_closure_with_captures(move |_t, args, doc_ref4, ctx| {
                    let data = arg_string(args, 0);
                    let cd = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let _ = cd.insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(4u32)));
                    let _ = cd.insert_property(boa_engine::js_string!("_data"), pd3(JsValue::from(boa_engine::js_string!(data.clone()))));
                    let _ = cd.insert_property(boa_engine::js_string!("data"), pd3(JsValue::from(boa_engine::js_string!(data.clone()))));
                    let _ = cd.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::from(boa_engine::js_string!(data.clone()))));
                    let _ = cd.insert_property(boa_engine::js_string!("textContent"), pd3(JsValue::from(boa_engine::js_string!(data))));
                    let _ = cd.insert_property(boa_engine::js_string!("nodeName"), pd3(JsValue::from(boa_engine::js_string!("#cdata-section"))));
                    let _ = cd.insert_property(boa_engine::js_string!("ownerDocument"), pd3(JsValue::from(doc_ref4.clone())));
                    Ok(cd.into())
                }, doc_ref4);
                let _ = obj.insert_property(boa_engine::js_string!("createCDATASection"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cd_fn).build())));
                // createRange
                let doc_ref5 = obj.clone();
                let cr_fn = NativeFunction::from_copy_closure_with_captures(move |_t, _a, doc_ref5, _ctx| {
                    let pd3 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let r = boa_engine::object::JsObject::with_object_proto(_ctx.intrinsics());
                    let _ = r.insert_property(boa_engine::js_string!("startContainer"), pd3(JsValue::from(doc_ref5.clone())));
                    let _ = r.insert_property(boa_engine::js_string!("endContainer"), pd3(JsValue::from(doc_ref5.clone())));
                    let _ = r.insert_property(boa_engine::js_string!("startOffset"), pd3(JsValue::from(0u32)));
                    let _ = r.insert_property(boa_engine::js_string!("endOffset"), pd3(JsValue::from(0u32)));
                    let _ = r.insert_property(boa_engine::js_string!("collapsed"), pd3(JsValue::from(true)));
                    let _ = r.insert_property(boa_engine::js_string!("commonAncestorContainer"), pd3(JsValue::from(doc_ref5.clone())));
                    Ok(r.into())
                }, doc_ref5);
                let _ = obj.insert_property(boa_engine::js_string!("createRange"), pd2(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cr_fn).build())));
            }
            // Add empty childNodes/firstChild/lastChild for Document constructor.
            let empty_arr = JsArray::new(ctx);
            let pd_doc = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = obj.insert_property(boa_engine::js_string!("childNodes"), pd_doc(empty_arr.into()));
            let _ = obj.insert_property(boa_engine::js_string!("firstChild"), pd_doc(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("lastChild"), pd_doc(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("doctype"), pd_doc(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("documentElement"), pd_doc(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("location"), pd_doc(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("head"), pd_doc(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("body"), pd_doc(JsValue::null()));
            Ok(obj.into())
        }, name_for_proto);
        let _ = ctx.register_global_callable(boa_engine::js_string!(name), 0, ctor_fn);

        // Use the .prototype already created by register_global_callable.
        // Just add constructor back-reference if not present.
        let global = ctx.global_object();
        if let Ok(ctor_val) = global.get(boa_engine::js_string!(name), &mut *ctx) {
            if let Some(ctor_obj) = ctor_val.as_object() {
                if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), &mut *ctx) {
                    if let Some(proto) = proto_val.as_object() {
                        let _ = proto.insert_property(
                            boa_engine::js_string!("constructor"),
                            pd(ctor_obj.clone().into()),
                        );
                    }
                }
            }
        }
    }

    // Set up prototype chain links between constructors' .prototype objects:
    // Node.prototype ← Element.prototype ← HTMLElement.prototype ← HTMLDivElement.prototype
    let get_proto = |name: &str, ctx: &mut Context| -> Option<boa_engine::JsObject> {
        let global = ctx.global_object();
        let ctor = global.get(boa_engine::js_string!(name), ctx).ok()?;
        let ctor_obj = ctor.as_object()?.clone();
        let proto_val = ctor_obj
            .get(boa_engine::js_string!("prototype"), ctx)
            .ok()?;
        proto_val.as_object()
    };

    // Link: HTMLElement.prototype.__proto__ = Element.prototype.__proto__ = Node.prototype
    if let (Some(node_proto), Some(elem_proto), Some(html_proto)) = (
        get_proto("Node", ctx),
        get_proto("Element", ctx),
        get_proto("HTMLElement", ctx),
    ) {
        let _ = elem_proto.set_prototype(Some(node_proto.clone()));
        let _ = html_proto.set_prototype(Some(elem_proto.clone()));
        // Add Node constants to Node.prototype so element.ELEMENT_NODE works.
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val).writable(false).enumerable(false).configurable(false).build()
        };
        let _ = node_proto.insert_property(boa_engine::js_string!("ELEMENT_NODE"), pd(JsValue::from(1u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("ATTRIBUTE_NODE"), pd(JsValue::from(2u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("TEXT_NODE"), pd(JsValue::from(3u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("CDATA_SECTION_NODE"), pd(JsValue::from(4u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("ENTITY_REFERENCE_NODE"), pd(JsValue::from(5u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("ENTITY_NODE"), pd(JsValue::from(6u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("PROCESSING_INSTRUCTION_NODE"), pd(JsValue::from(7u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("COMMENT_NODE"), pd(JsValue::from(8u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("DOCUMENT_NODE"), pd(JsValue::from(9u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("DOCUMENT_TYPE_NODE"), pd(JsValue::from(10u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("DOCUMENT_FRAGMENT_NODE"), pd(JsValue::from(11u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("NOTATION_NODE"), pd(JsValue::from(12u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("DOCUMENT_POSITION_CONTAINED_BY"), pd(JsValue::from(0x10u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("DOCUMENT_POSITION_CONTAINS"), pd(JsValue::from(0x08u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("DOCUMENT_POSITION_PRECEDING"), pd(JsValue::from(0x02u32)));
        let _ = node_proto.insert_property(boa_engine::js_string!("DOCUMENT_POSITION_FOLLOWING"), pd(JsValue::from(0x04u32)));

        // Also add constants to the Node constructor itself (Node.ELEMENT_NODE).
        if let Ok(node_ctor_val) = ctx.global_object().get(boa_engine::js_string!("Node"), ctx) {
            if let Some(node_ctor) = node_ctor_val.as_object() {
                for (name, val) in &[
                    ("ELEMENT_NODE", 1u32), ("ATTRIBUTE_NODE", 2u32), ("TEXT_NODE", 3u32),
                    ("CDATA_SECTION_NODE", 4u32), ("ENTITY_REFERENCE_NODE", 5u32),
                    ("ENTITY_NODE", 6u32), ("PROCESSING_INSTRUCTION_NODE", 7u32),
                    ("COMMENT_NODE", 8u32), ("DOCUMENT_NODE", 9u32),
                    ("DOCUMENT_TYPE_NODE", 10u32), ("DOCUMENT_FRAGMENT_NODE", 11u32),
                    ("NOTATION_NODE", 12u32),
                    ("DOCUMENT_POSITION_DISCONNECTED", 0x01u32),
                    ("DOCUMENT_POSITION_PRECEDING", 0x02u32),
                    ("DOCUMENT_POSITION_FOLLOWING", 0x04u32),
                    ("DOCUMENT_POSITION_CONTAINS", 0x08u32),
                    ("DOCUMENT_POSITION_CONTAINED_BY", 0x10u32),
                    ("DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC", 0x20u32),
                ] {
                    let _ = node_ctor.insert_property(boa_engine::js_string!(*name), pd(JsValue::from(*val)));
                }
            }
        }

        // Add isSameNode to Node.prototype so all node types inherit it.
        let isn_fn = NativeFunction::from_copy_closure(|this, args, _ctx| {
            let other = args.first().cloned().unwrap_or(JsValue::null());
            // Reference equality: this === other
            Ok(JsValue::from(JsValue::same_value(this, &other)))
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("isSameNode"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), isn_fn).build())
                .writable(true).enumerable(true).configurable(true).build(),
        );

        // Add contains to Node.prototype.
        let contains_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let other = args.first().cloned().unwrap_or(JsValue::null());
            if other.is_null() || other.is_undefined() {
                return Ok(JsValue::from(false));
            }
            if JsValue::same_value(this, &other) {
                return Ok(JsValue::from(true));
            }
            // Walk up parentNode chain from other.
            let mut current = other.as_object();
            for _ in 0..10000 {
                match current {
                    Some(o) => {
                        if let Ok(pn) = o.get(boa_engine::js_string!("parentNode"), ctx) {
                            if JsValue::same_value(this, &pn) {
                                return Ok(JsValue::from(true));
                            }
                            current = pn.as_object();
                        } else {
                            break;
                        }
                    }
                    None => break,
                }
            }
            Ok(JsValue::from(false))
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("contains"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), contains_fn).build())
                .writable(true).enumerable(true).configurable(true).build(),
        );

        // Add getRootNode to Node.prototype.
        let grn_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
            let mut current = this.as_object();
            let mut root = current.clone();
            for _ in 0..10000 {
                match current {
                    Some(o) => {
                        if let Ok(pn) = o.get(boa_engine::js_string!("parentNode"), ctx) {
                            if pn.is_null() || pn.is_undefined() {
                                break;
                            }
                            current = pn.as_object();
                            root = current.clone();
                        } else {
                            break;
                        }
                    }
                    None => break,
                }
            }
            match root {
                Some(o) => Ok(o.into()),
                None => Ok(this.clone()),
            }
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("getRootNode"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), grn_fn).build())
                .writable(true).enumerable(true).configurable(true).build(),
        );

        // Add hasChildNodes to Node.prototype.
        let hcn_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
            if let Some(o) = this.as_object() {
                if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                    if let Some(ca) = cv.as_object() {
                        let len = ca.get(boa_engine::js_string!("length"), ctx).ok()
                            .and_then(|v| v.as_number()).unwrap_or(0.0);
                        return Ok(JsValue::from(len > 0.0));
                    }
                }
                if let Ok(cn) = o.get(boa_engine::js_string!("childNodes"), ctx) {
                    if let Some(ca) = cn.as_object() {
                        let len = ca.get(boa_engine::js_string!("length"), ctx).ok()
                            .and_then(|v| v.as_number()).unwrap_or(0.0);
                        return Ok(JsValue::from(len > 0.0));
                    }
                }
            }
            Ok(JsValue::from(false))
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("hasChildNodes"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), hcn_fn).build())
                .writable(true).enumerable(true).configurable(true).build(),
        );
    }

    // Link all HTMLxxxElement.prototype to HTMLElement.prototype
    if let Some(html_proto) = get_proto("HTMLElement", ctx) {
        for (name, _) in all_types
            .iter()
            .filter(|(n, _)| n.starts_with("HTML") && *n != "HTMLElement")
        {
            if let Some(p) = get_proto(name, ctx) {
                let _ = p.set_prototype(Some(html_proto.clone()));
            }
        }
    }

    // Add Symbol.toStringTag to key prototypes for Object.prototype.toString.call().
    // Get the actual Symbol.toStringTag well-known symbol from the global Symbol object.
    let to_string_tag_sym: Option<boa_engine::JsSymbol> = {
        let global = ctx.global_object();
        if let Ok(sym_ctor_val) = global.get(boa_engine::js_string!("Symbol"), ctx) {
            if let Some(sym_ctor) = sym_ctor_val.as_object() {
                if let Ok(tst_val) = sym_ctor.get(boa_engine::js_string!("toStringTag"), ctx) {
                    tst_val.as_symbol()
                } else { None }
            } else { None }
        } else { None }
    };
    let tag_pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val).writable(false).enumerable(false).configurable(true).build()
    };
    if let Some(sym) = to_string_tag_sym {
        for (name, tag) in &[
            ("TreeWalker", "TreeWalker"), ("NodeIterator", "NodeIterator"),
            ("NodeList", "NodeList"), ("HTMLCollection", "HTMLCollection"),
            ("Range", "Range"), ("Document", "Document"),
            ("DocumentFragment", "DocumentFragment"), ("DocumentType", "DocumentType"),
            ("Element", "Element"), ("Text", "Text"), ("Comment", "Comment"),
            ("Attr", "Attr"), ("Event", "Event"), ("Node", "Node"),
        ] {
            if let Some(proto) = get_proto(name, ctx) {
                let _ = proto.insert_property(sym.clone(), tag_pd(JsValue::from(boa_engine::js_string!(*tag))));
            }
        }
    }

    // Link Text.prototype → CharacterData.prototype → Node.prototype
    // Link Comment.prototype → CharacterData.prototype → Node.prototype
    // Link Document.prototype → Node.prototype
    // Link DocumentFragment.prototype → Node.prototype
    // Link Attr.prototype → Node.prototype
    if let Some(node_proto) = get_proto("Node", ctx) {
        if let Some(char_proto) = get_proto("CharacterData", ctx) {
            let _ = char_proto.set_prototype(Some(node_proto.clone()));
            if let Some(text_proto) = get_proto("Text", ctx) {
                let _ = text_proto.set_prototype(Some(char_proto.clone()));
            }
            if let Some(comment_proto) = get_proto("Comment", ctx) {
                let _ = comment_proto.set_prototype(Some(char_proto.clone()));
            }
        }
        for name in &["Document", "DocumentFragment", "Attr", "DocumentType", "ProcessingInstruction"] {
            if let Some(p) = get_proto(name, ctx) {
                let _ = p.set_prototype(Some(node_proto.clone()));
            }
        }
    }

    // Add NodeFilter constants to the NodeFilter constructor.
    let pd_const = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val).writable(false).enumerable(false).configurable(false).build()
    };
    if let Ok(nf_val) = ctx.global_object().get(boa_engine::js_string!("NodeFilter"), ctx) {
        if let Some(nf) = nf_val.as_object() {
            let _ = nf.insert_property(boa_engine::js_string!("FILTER_ACCEPT"), pd_const(JsValue::from(1u32)));
            let _ = nf.insert_property(boa_engine::js_string!("FILTER_REJECT"), pd_const(JsValue::from(2u32)));
            let _ = nf.insert_property(boa_engine::js_string!("FILTER_SKIP"), pd_const(JsValue::from(3u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_ALL"), pd_const(JsValue::from(0xFFFFFFFFu32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_ELEMENT"), pd_const(JsValue::from(0x1u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_ATTRIBUTE"), pd_const(JsValue::from(0x2u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_TEXT"), pd_const(JsValue::from(0x4u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_CDATA_SECTION"), pd_const(JsValue::from(0x8u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_PROCESSING_INSTRUCTION"), pd_const(JsValue::from(0x40u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_COMMENT"), pd_const(JsValue::from(0x80u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_DOCUMENT"), pd_const(JsValue::from(0x100u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_DOCUMENT_TYPE"), pd_const(JsValue::from(0x200u32)));
            let _ = nf.insert_property(boa_engine::js_string!("SHOW_DOCUMENT_FRAGMENT"), pd_const(JsValue::from(0x400u32)));
        }
    }
}
fn build_character_data_methods() -> Vec<(&'static str, NativeFunction)> {
    let append = NativeFunction::from_copy_closure(|this, args, ctx| {
        let v = arg_to_string(args, 0, ctx);
        let old_len = read_data_utf16(&this, ctx).len() as u32;
        let data_len = v.encode_utf16().count() as u32;
        let mut units = read_data_utf16(&this, ctx);
        units.extend(v.encode_utf16());
        write_data_utf16(&this, &units, ctx);
        // Update Range boundary points: appendData = replaceData(oldLen, 0, data)
        if let Some(o) = this.as_object() {
            update_ranges_for_data_change(&o, old_len, 0, data_len, ctx);
        }
        Ok(JsValue::undefined())
    });
    let delete_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().map(|v| js_to_uint32(v)).unwrap_or(0);
        let count = args.get(1).map(|v| js_to_uint32(v)).unwrap_or(0);
        let mut units = read_data_utf16(&this, ctx);
        let len = units.len() as u32;
        if offset > len {
            return throw_index_size(ctx);
        }
        let actual_count = count.min(len - offset);
        let end = (offset.saturating_add(count)).min(len) as usize;
        units.drain(offset as usize..end);
        write_data_utf16(&this, &units, ctx);
        if let Some(o) = this.as_object() {
            update_ranges_for_data_change(&o, offset, actual_count, 0, ctx);
        }
        Ok(JsValue::undefined())
    });
    let insert_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().map(|v| js_to_uint32(v)).unwrap_or(0);
        let data = arg_to_string(args, 1, ctx);
        let data_len = data.encode_utf16().count() as u32;
        let mut units = read_data_utf16(&this, ctx);
        let len = units.len() as u32;
        if offset > len {
            return throw_index_size(ctx);
        }
        let insert_units: Vec<u16> = data.encode_utf16().collect();
        let pos = offset as usize;
        units.splice(pos..pos, insert_units);
        write_data_utf16(&this, &units, ctx);
        if let Some(o) = this.as_object() {
            update_ranges_for_data_change(&o, offset, 0, data_len, ctx);
        }
        Ok(JsValue::undefined())
    });
    let replace_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().map(|v| js_to_uint32(v)).unwrap_or(0);
        let count = args.get(1).map(|v| js_to_uint32(v)).unwrap_or(0);
        let data = arg_to_string(args, 2, ctx);
        let data_len = data.encode_utf16().count() as u32;
        let mut units = read_data_utf16(&this, ctx);
        let len = units.len() as u32;
        if offset > len {
            return throw_index_size(ctx);
        }
        let actual_count = count.min(len - offset);
        let end = (offset.saturating_add(count)).min(len) as usize;
        let replace_units: Vec<u16> = data.encode_utf16().collect();
        units.splice(offset as usize..end, replace_units);
        write_data_utf16(&this, &units, ctx);
        if let Some(o) = this.as_object() {
            update_ranges_for_data_change(&o, offset, actual_count, data_len, ctx);
        }
        Ok(JsValue::undefined())
    });
    let substring_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        // offset and count are mandatory (throw TypeError if missing).
        if args.is_empty() || args.first().map(|v| v.is_undefined()).unwrap_or(true) {
            return Err(boa_engine::JsNativeError::typ()
                .with_message("substringData requires at least 1 argument").into());
        }
        if args.get(1).map(|v| v.is_undefined()).unwrap_or(true) {
            return Err(boa_engine::JsNativeError::typ()
                .with_message("substringData requires offset and count").into());
        }
        let offset = args.first().map(|v| js_to_uint32(v)).unwrap_or(0);
        let count = args.get(1).map(|v| js_to_uint32(v)).unwrap_or(0);
        let units = read_data_utf16(&this, ctx);
        let len = units.len() as u32;
        if offset > len {
            return throw_index_size(ctx);
        }
        let end = (offset.saturating_add(count)).min(len) as usize;
        let sub = &units[offset as usize..end];
        Ok(JsValue::from(boa_engine::JsString::from(sub)))
    });
    let remove_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                remove_from_children(&parent_obj, &obj, ctx);
                let pd = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val).writable(true).enumerable(true).configurable(true).build()
                };
                let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
                let _ = obj.insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
            }
        }
        Ok(JsValue::undefined())
    });
    let before_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&parent_obj, &child_obj, Some(obj.clone()), ctx);
                    }
                }
            }
        }
        Ok(JsValue::undefined())
    });
    let after_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                let next_sibling = get_next_sibling(&parent_obj, &obj, ctx);
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&parent_obj, &child_obj, next_sibling.clone(), ctx);
                    }
                }
            }
        }
        Ok(JsValue::undefined())
    });
    let replace_with_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                // Insert all args before this node.
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&parent_obj, &child_obj, Some(obj.clone()), ctx);
                    }
                }
                // Remove this node.
                remove_from_children(&parent_obj, &obj, ctx);
                let pd = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val).writable(true).enumerable(true).configurable(true).build()
                };
                let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
                let _ = obj.insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
            }
        }
        Ok(JsValue::undefined())
    });
    vec![
        ("appendData", append),
        ("deleteData", delete_d),
        ("insertData", insert_d),
        ("replaceData", replace_d),
        ("substringData", substring_d),
        ("remove", remove_fn),
        ("before", before_fn),
        ("after", after_fn),
        ("replaceWith", replace_with_fn),
    ]
}

/// Read the "data" property of a CharacterData node as UTF-16 code units.
fn read_data_utf16(this: &boa_engine::JsValue, ctx: &mut Context) -> Vec<u16> {
    // Only read from _data (internal store). Never read "data" directly
    // because it may be an accessor that calls this function → infinite recursion.
    if let Some(o) = this.as_object() {
        if let Ok(v) = o.get(boa_engine::js_string!("_data"), ctx) {
            if let Some(s) = v.as_string() {
                return s.iter().collect();
            }
        }
    }
    Vec::new()
}

/// Write UTF-16 code units to _data/data and update textContent/length.
fn write_data_utf16(this: &boa_engine::JsValue, units: &[u16], ctx: &mut Context) {
    if let Some(o) = this.as_object() {
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let js_str = boa_engine::JsString::from(units);
        // Write to _data (internal) for CharacterData method compatibility.
        let _ = o.insert_property(boa_engine::js_string!("_data"), pd(JsValue::from(js_str.clone())));
        // Also update textContent and length.
        let _ = o.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(js_str)));
        let _ = o.insert_property(
            boa_engine::js_string!("length"),
            pd(JsValue::from(units.len() as u32)),
        );
    }
}

/// Update all Range boundary points after a CharacterData mutation.
/// Implements the "replace data" steps from the DOM spec:
/// - For every boundary point whose node is `node` and offset > offset && <= offset+count:
///   set offset to `offset`.
/// - For every boundary point whose node is `node` and offset > offset+count:
///   add data_len, subtract count.
fn update_ranges_for_data_change(node: &JsObject, offset: u32, count: u32, data_len: u32, ctx: &mut Context) {
    // Get the global __active_ranges array (if it exists).
    let global = ctx.global_object();
    let ranges_val = match global.get(boa_engine::js_string!("__active_ranges"), ctx) {
        Ok(v) => v,
        Err(_) => return,
    };
    let ranges_arr = match ranges_val.as_object() {
        Some(o) => o,
        None => return,
    };
    let len = ranges_arr.get(boa_engine::js_string!("length"), ctx).ok()
        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val).writable(true).enumerable(true).configurable(true).build()
    };
    // Per DOM spec "replace data" steps:
    // Step 1: For offset > offset && <= offset+count, set to offset.
    // Step 2: For offset > offset+count, add data_len, subtract count.
    for i in 0..len {
        if let Ok(range_val) = ranges_arr.get(i as u32, ctx) {
            if let Some(range) = range_val.as_object() {
                for (container_key, offset_key) in &[
                    (boa_engine::js_string!("startContainer"), boa_engine::js_string!("startOffset")),
                    (boa_engine::js_string!("endContainer"), boa_engine::js_string!("endOffset")),
                ] {
                    let cont = range.get(container_key.clone(), ctx).ok();
                    if let Some(cont_obj) = cont.and_then(|v| v.as_object()) {
                        if boa_engine::object::JsObject::equals(&cont_obj, node) {
                            let cur_offset = range.get(offset_key.clone(), ctx).ok()
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                            let new_offset = if cur_offset > offset && cur_offset <= offset + count {
                                // Step 1: clamp to offset
                                offset
                            } else if cur_offset > offset + count {
                                // Step 2: add data_len, subtract count
                                cur_offset + data_len - count
                            } else {
                                cur_offset
                            };
                            let _ = range.insert_property(offset_key.clone(), pd(JsValue::from(new_offset)));
                        }
                    }
                }
            }
        }
    }
}

/// Update Range boundary points after a node is removed from its parent.
/// Per DOM spec "insert" and "remove" steps.
fn update_ranges_for_node_removal(removed_node: &JsObject, old_parent: &JsObject, old_index: u32, ctx: &mut Context) {
    let global = ctx.global_object();
    let ranges_val = match global.get(boa_engine::js_string!("__active_ranges"), ctx) {
        Ok(v) => v,
        Err(_) => return,
    };
    let ranges_arr = match ranges_val.as_object() {
        Some(o) => o,
        None => return,
    };
    let len = ranges_arr.get(boa_engine::js_string!("length"), ctx).ok()
        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val).writable(true).enumerable(true).configurable(true).build()
    };
    for i in 0..len {
        if let Ok(range_val) = ranges_arr.get(i as u32, ctx) {
            if let Some(range) = range_val.as_object() {
                for (container_key, offset_key) in &[
                    (boa_engine::js_string!("startContainer"), boa_engine::js_string!("startOffset")),
                    (boa_engine::js_string!("endContainer"), boa_engine::js_string!("endOffset")),
                ] {
                    let cont = range.get(container_key.clone(), ctx).ok();
                    if let Some(cont_obj) = cont.and_then(|v| v.as_object()) {
                        // Check if container is removed_node or a descendant of it.
                        let is_removed_or_descendant = boa_engine::object::JsObject::equals(&cont_obj, removed_node)
                            || is_descendant_of(&cont_obj, removed_node, ctx);
                        if is_removed_or_descendant {
                            // Set boundary to (old_parent, old_index).
                            let _ = range.insert_property(container_key.clone(), pd(JsValue::from(old_parent.clone())));
                            let _ = range.insert_property(offset_key.clone(), pd(JsValue::from(old_index)));
                        } else if boa_engine::object::JsObject::equals(&cont_obj, old_parent) {
                            // If container is old_parent and offset > old_index, subtract 1.
                            let cur_offset = range.get(offset_key.clone(), ctx).ok()
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                            if cur_offset > old_index {
                                let _ = range.insert_property(offset_key.clone(), pd(JsValue::from(cur_offset - 1)));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Update Range boundary points after a node is inserted into a parent.
fn update_ranges_for_node_insertion(inserted_node: &JsObject, ctx: &mut Context) {
    let parent = match inserted_node.get(boa_engine::js_string!("parentNode"), ctx).ok()
        .and_then(|v| v.as_object()) {
        Some(p) => p,
        None => return,
    };
    let new_index = node_index_in_parent(inserted_node, ctx);

    let global = ctx.global_object();
    let ranges_val = match global.get(boa_engine::js_string!("__active_ranges"), ctx) {
        Ok(v) => v,
        Err(_) => return,
    };
    let ranges_arr = match ranges_val.as_object() {
        Some(o) => o,
        None => return,
    };
    let len = ranges_arr.get(boa_engine::js_string!("length"), ctx).ok()
        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val).writable(true).enumerable(true).configurable(true).build()
    };
    for i in 0..len {
        if let Ok(range_val) = ranges_arr.get(i as u32, ctx) {
            if let Some(range) = range_val.as_object() {
                for (container_key, offset_key) in &[
                    (boa_engine::js_string!("startContainer"), boa_engine::js_string!("startOffset")),
                    (boa_engine::js_string!("endContainer"), boa_engine::js_string!("endOffset")),
                ] {
                    let cont = range.get(container_key.clone(), ctx).ok();
                    if let Some(cont_obj) = cont.and_then(|v| v.as_object()) {
                        if boa_engine::object::JsObject::equals(&cont_obj, &parent) {
                            let cur_offset = range.get(offset_key.clone(), ctx).ok()
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                            if cur_offset > new_index {
                                let _ = range.insert_property(offset_key.clone(), pd(JsValue::from(cur_offset + 1)));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Check if `node` is a descendant of `ancestor` by walking up parentNode chain.
fn is_descendant_of(node: &JsObject, ancestor: &JsObject, ctx: &mut Context) -> bool {
    let mut current = node.get(boa_engine::js_string!("parentNode"), ctx).ok()
        .and_then(|v| v.as_object());
    for _ in 0..100 {
        match current {
            Some(p) => {
                if boa_engine::object::JsObject::equals(&p, ancestor) {
                    return true;
                }
                current = p.get(boa_engine::js_string!("parentNode"), ctx).ok()
                    .and_then(|v| v.as_object());
            }
            None => return false,
        }
    }
    false
}

/// Get the index of a node in its parent's _children array.
fn node_index_in_parent(node: &JsObject, ctx: &mut Context) -> u32 {
    let parent = match node.get(boa_engine::js_string!("parentNode"), ctx).ok()
        .and_then(|v| v.as_object()) {
        Some(p) => p,
        None => return 0,
    };
    let children = match parent.get(boa_engine::js_string!("_children"), ctx).ok()
        .and_then(|v| v.as_object()) {
        Some(c) => c,
        None => return 0,
    };
    let len = children.get(boa_engine::js_string!("length"), ctx).ok()
        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
    for i in 0..len {
        if let Ok(child) = children.get(i as u32, ctx) {
            if let Some(child_obj) = child.as_object() {
                if boa_engine::object::JsObject::equals(&child_obj, node) {
                    return i;
                }
            }
        }
    }
    0
}

/// Check if a JS element object matches a simple CSS selector.
/// Supports: tag, .class, #id, [attr], [attr=val], tag.class, tag#id, tag[attr],
/// div:not(.cls), tag.cls#id[attr=val], comma-separated selector lists.
/// Get the next sibling of a child in parent's _children.
/// Get or create a JsArray for parent's _children.
fn get_or_create_children(parent: &JsObject, ctx: &mut Context) -> JsArray {
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val).writable(true).enumerable(true).configurable(true).build()
    };
    let children = parent.get(boa_engine::js_string!("_children"), ctx).ok()
        .and_then(|v| v.as_object());
    match children {
        Some(obj) => JsArray::from_object(obj).unwrap_or_else(|_| JsArray::new(ctx)),
        None => {
            let arr = JsArray::new(ctx);
            let _ = parent.insert_property(boa_engine::js_string!("_children"), pd(arr.clone().into()));
            arr
        }
    }
}

fn get_next_sibling(parent: &JsObject, child: &JsObject, ctx: &mut Context) -> Option<JsObject> {
    let children = parent.get(boa_engine::js_string!("_children"), ctx).ok()
        .and_then(|v| v.as_object())?;
    let arr = JsArray::from_object(children).ok()?;
    let len = arr.length(ctx).ok()? as u32;
    for i in 0..len {
        if let Ok(v) = arr.at(i, ctx) {
            if let Some(o) = v.as_object() {
                if JsObject::equals(&o, child) {
                    if i + 1 < len {
                        if let Ok(next) = arr.at(i + 1, ctx) {
                            return next.as_object();
                        }
                    }
                    return None;
                }
            }
        }
    }
    None
}

fn remove_from_children(parent: &JsObject, child: &JsObject, ctx: &mut Context) {
    let arr = get_or_create_children(parent, ctx);
    let len = arr.length(ctx).ok().unwrap_or(0) as u32;
    let mut found: Option<u32> = None;
    for i in 0..len {
        if let Ok(v) = arr.at(i, ctx) {
            if let Some(o) = v.as_object() {
                if JsObject::equals(&o, child) {
                    found = Some(i);
                    break;
                }
            }
        }
    }
    if let Some(idx) = found {
        let splice_fn = arr.get(boa_engine::js_string!("splice"), ctx).ok()
            .and_then(|v| v.as_object());
        if let Some(sf) = splice_fn {
            if sf.is_callable() {
                let arr_obj: &JsObject = &*arr;
                let _ = sf.call(&JsValue::from(arr_obj.clone()), &[JsValue::from(idx as i32), JsValue::from(1i32)], ctx);
            }
        }
    }
}

fn insert_into_children(parent: &JsObject, child: &JsObject, before: Option<JsObject>, ctx: &mut Context) {
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val).writable(true).enumerable(true).configurable(true).build()
    };
    let arr = get_or_create_children(parent, ctx);
    let len = arr.length(ctx).ok().unwrap_or(0) as u32;
    if before.is_none() {
        let _ = arr.push(JsValue::from(child.clone()), ctx);
    } else {
        let ref_node = before.unwrap();
        let mut ref_idx = len;
        for i in 0..len {
            if let Ok(v) = arr.at(i, ctx) {
                if let Some(o) = v.as_object() {
                    if JsObject::equals(&o, &ref_node) {
                        ref_idx = i;
                        break;
                    }
                }
            }
        }
        let splice_fn = arr.get(boa_engine::js_string!("splice"), ctx).ok()
            .and_then(|v| v.as_object());
        if let Some(sf) = splice_fn {
            if sf.is_callable() {
                let arr_obj2: &JsObject = &*arr;
                let _ = sf.call(&JsValue::from(arr_obj2.clone()), &[JsValue::from(ref_idx as i32), JsValue::from(0i32), JsValue::from(child.clone())], ctx);
            } else {
                let _ = arr.push(JsValue::from(child.clone()), ctx);
            }
        } else {
            let _ = arr.push(JsValue::from(child.clone()), ctx);
        }
    }
    let _ = child.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::from(parent.clone())));
    let _ = child.insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::from(parent.clone())));
}

/// Add DOM mutation methods + child tracking to any JS object.
/// This is used by both make_element_handle (bridge elements) and
/// new Document().createElement() (JS-only elements).
fn add_dom_methods(obj: &JsObject, ctx: &mut Context) {
    // appendChild — delegates to insert_into_children for consistency
    let append = NativeFunction::from_copy_closure(|this, args, ctx| {
        let child = args.first().cloned().unwrap_or(JsValue::null());
        if let (Some(parent_obj), Some(child_obj)) = (this.as_object(), child.as_object()) {
            insert_into_children(&parent_obj, &child_obj, None, ctx);
        }
        Ok(child)
    });
    let _ = obj.insert_property(boa_engine::js_string!("appendChild"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), append).build()))
            .writable(true).enumerable(true).configurable(true).build());

    // insertBefore
    let insert_before = NativeFunction::from_copy_closure(|this, args, ctx| {
        let child = args.first().cloned().unwrap_or(JsValue::null());
        let ref_node = args.get(1).cloned().unwrap_or(JsValue::null());
        if let (Some(parent_obj), Some(child_obj)) = (this.as_object(), child.as_object()) {
            insert_into_children(&parent_obj, &child_obj, ref_node.as_object(), ctx);
        }
        Ok(child)
    });
    let _ = obj.insert_property(boa_engine::js_string!("insertBefore"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), insert_before).build()))
            .writable(true).enumerable(true).configurable(true).build());

    // removeChild
    let remove_child = NativeFunction::from_copy_closure(|this, args, ctx| {
        let child = args.first().cloned().unwrap_or(JsValue::null());
        if child.is_null() || child.is_undefined() || child.as_object().is_none() {
            return Err(boa_engine::JsNativeError::typ().with_message("Argument is not a Node").into());
        }
        let child_obj = child.as_object().unwrap();
        if let Some(parent_obj) = this.as_object() {
            remove_from_children(&parent_obj, &child_obj, ctx);
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            let _ = child_obj.insert_property(boa_engine::js_string!("parentNode"), pd2(JsValue::null()));
            let _ = child_obj.insert_property(boa_engine::js_string!("parentElement"), pd2(JsValue::null()));
        }
        Ok(child)
    });
    let _ = obj.insert_property(boa_engine::js_string!("removeChild"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), remove_child).build()))
            .writable(true).enumerable(true).configurable(true).build());

    // setAttribute
    let set_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0).to_ascii_lowercase();
        let value = arg_string(args, 1);
        let value2 = value.clone();
        if let Some(o) = this.as_object() {
            let _ = o.insert_property(boa_engine::js_string!(name.clone()), boa_engine::property::PropertyDescriptor::builder().value(JsValue::from(boa_engine::js_string!(value))).writable(true).enumerable(true).configurable(true).build());
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    let attr_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let _ = attr_obj.insert_property(boa_engine::js_string!("name"), pd2(JsValue::from(boa_engine::js_string!(name))));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("value"), pd2(JsValue::from(boa_engine::js_string!(value2))));
                    let _ = attrs.insert_property(len, pd2(attr_obj.into()));
                    let _ = attrs.insert_property(boa_engine::js_string!("length"), pd2(JsValue::from(len + 1)));
                }
            }
        }
        Ok(JsValue::undefined())
    });
    let _ = obj.insert_property(boa_engine::js_string!("setAttribute"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_attr).build()))
            .writable(true).enumerable(true).configurable(true).build());

    // getAttribute
    let get_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0).to_ascii_lowercase();
        if let Some(o) = this.as_object() {
            if let Ok(v) = o.get(boa_engine::js_string!(name), ctx) {
                if !v.is_undefined() && !v.is_null() { return Ok(v); }
            }
        }
        Ok(JsValue::null())
    });
    let _ = obj.insert_property(boa_engine::js_string!("getAttribute"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), get_attr).build()))
            .writable(true).enumerable(true).configurable(true).build());

    // hasAttribute
    let has_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0).to_ascii_lowercase();
        if let Some(o) = this.as_object() {
            if let Ok(v) = o.get(boa_engine::js_string!(name), ctx) {
                return Ok(JsValue::from(!v.is_undefined() && !v.is_null()));
            }
        }
        Ok(JsValue::from(false))
    });
    let _ = obj.insert_property(boa_engine::js_string!("hasAttribute"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), has_attr).build()))
            .writable(true).enumerable(true).configurable(true).build());

    // remove
    let remove_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                remove_from_children(&parent_obj, &obj, ctx);
                let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd2(JsValue::null()));
            }
        }
        Ok(JsValue::undefined())
    });
    let _ = obj.insert_property(boa_engine::js_string!("remove"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), remove_fn).build()))
            .writable(true).enumerable(true).configurable(true).build());

    // Empty attributes NamedNodeMap.
    let attrs_map = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
    let _ = attrs_map.insert_property(boa_engine::js_string!("length"), pd2(JsValue::from(0u32)));
    let _ = obj.insert_property(boa_engine::js_string!("attributes"), pd2(attrs_map.into()));
}

/// Convert a JsValue to a DOM node for insertion (strings become text nodes).
fn value_to_node(val: &JsValue, ctx: &mut Context) -> JsValue {
    if val.is_object() {
        return val.clone();
    }
    // Convert to string and create a text node.
    let s = val.to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default();
    let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let pd = |v: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(v).writable(true).enumerable(true).configurable(true).build()
    };
    let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32)));
    let _ = obj.insert_property(boa_engine::js_string!("_data"), pd(JsValue::from(boa_engine::js_string!(s.clone()))));
    let _ = obj.insert_property(boa_engine::js_string!("data"), pd(JsValue::from(boa_engine::js_string!(s.clone()))));
    let _ = obj.insert_property(boa_engine::js_string!("nodeValue"), pd(JsValue::from(boa_engine::js_string!(s.clone()))));
    let _ = obj.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(boa_engine::js_string!(s))));
    let _ = obj.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("#text"))));
    // Set Text.prototype so instanceof Text works.
    if let Ok(text_ctor) = ctx.global_object().get(boa_engine::js_string!("Text"), ctx) {
        if let Some(tc) = text_ctor.as_object() {
            if let Ok(proto_val) = tc.get(boa_engine::js_string!("prototype"), ctx) {
                if let Some(proto) = proto_val.as_object() {
                    let _ = obj.set_prototype(Some(proto));
                }
            }
        }
    }
    JsValue::from(obj)
}

/// Serialize a node to HTML string for innerHTML.
/// Collect text content from a node and all its descendants.
fn collect_text_content(o: &boa_engine::object::JsObject, ctx: &mut Context, depth: u32) -> String {
    if depth > 50 { return String::new(); }
    let nt = o.get(boa_engine::js_string!("nodeType"), ctx).ok()
        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
    // Text nodes: return their data.
    if nt == 3 {
        if let Ok(v) = o.get(boa_engine::js_string!("_data"), ctx) {
            if let Some(s) = v.as_string() { return s.to_std_string_escaped(); }
        }
        return String::new();
    }
    // Element/Document/DocumentFragment: concatenate children's text.
    let children_html = serialize_children(o, ctx);
    // serialize_children returns HTML serialization; for text content we need raw text.
    // Use the same JsArray iteration but collect text only.
    let mut result = String::new();
    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
        if let Some(ca) = cv.as_object() {
            let ca_clone = ca.clone();
            if let Ok(arr) = JsArray::from_object(ca) {
                if let Ok(len) = arr.length(ctx) {
                    for i in 0..len as u32 {
                        if let Ok(child) = arr.at(i, ctx) {
                            if let Some(child_obj) = child.as_object() {
                                result.push_str(&collect_text_content(&child_obj, ctx, depth + 1));
                            }
                        }
                    }
                }
            } else {
                let ca_ref = &ca_clone;
                let len = ca_ref.get(boa_engine::js_string!("length"), ctx).ok()
                    .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                for i in 0..len {
                    if let Ok(child) = ca_ref.get(i as u32, ctx) {
                        if let Some(child_obj) = child.as_object() {
                            result.push_str(&collect_text_content(&child_obj, ctx, depth + 1));
                        }
                    }
                }
            }
        }
    }
    result
}

/// Serialize only the children of a node (for innerHTML).
fn serialize_children(o: &boa_engine::object::JsObject, ctx: &mut Context) -> String {
    let mut html = String::new();
    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
        if let Some(ca) = cv.as_object() {
            let ca_ref = &ca;
            // Try JsArray first, fallback to plain object get.
            if let Ok(arr) = JsArray::from_object(ca_ref.clone()) {
                if let Ok(clen) = arr.length(ctx) {
                    for i in 0..clen as u32 {
                        if let Ok(child) = arr.at(i, ctx) {
                            html.push_str(&serialize_node_depth(&child, ctx, 1));
                        }
                    }
                }
            } else {
                let clen = ca_ref.get(boa_engine::js_string!("length"), ctx).ok()
                    .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                for i in 0..clen {
                    if let Ok(child) = ca_ref.get(i as u32, ctx) {
                        html.push_str(&serialize_node_depth(&child, ctx, 1));
                    }
                }
            }
        }
    }
    html
}

fn serialize_node(val: &JsValue, ctx: &mut Context) -> String {
    serialize_node_depth(val, ctx, 0)
}

fn serialize_node_depth(val: &JsValue, ctx: &mut Context, depth: u32) -> String {
    if depth > 100 { return String::new(); } // Prevent stack overflow from cycles.
    if let Some(o) = val.as_object() {
        let nt = o.get(boa_engine::js_string!("nodeType"), ctx).ok()
            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        let nt = o.get(boa_engine::js_string!("nodeType"), ctx).ok()
            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        match nt {
            3 => {
                // Text node
                o.get(boa_engine::js_string!("data"), ctx).ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default()
            }
            8 => {
                // Comment node
                let data = o.get(boa_engine::js_string!("data"), ctx).ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                format!("<!--{}-->", data)
            }
            1 | 9 | 11 => {
                // Element / Document / DocumentFragment
                let tag = o.get(boa_engine::js_string!("tagName"), ctx).ok()
                    .or_else(|| o.get(boa_engine::js_string!("nodeName"), ctx).ok())
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                // Serialize attributes.
                let mut attrs_str = String::new();
                if let Ok(av) = o.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(ao) = av.as_object() {
                        let alen = ao.get(boa_engine::js_string!("length"), ctx).ok()
                            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                        for i in 0..alen {
                            if let Ok(a) = ao.get(i as u32, ctx) {
                                if let Some(a_obj) = a.as_object() {
                                    let name = a_obj.get(boa_engine::js_string!("name"), ctx).ok()
                                        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                                        .unwrap_or_default();
                                    let value = a_obj.get(boa_engine::js_string!("value"), ctx).ok()
                                        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                                        .unwrap_or_default();
                                    if !name.is_empty() {
                                        attrs_str.push_str(&format!(" {}=\"{}\"", name.to_ascii_lowercase(), value));
                                    }
                                }
                            }
                        }
                    }
                }
                // Serialize children from _children array (use serialize_children for JsArray support).
                let children_html = serialize_children(&o, ctx);
                // Void elements don't have closing tags.
                let void = matches!(tag.to_ascii_lowercase().as_str(),
                    "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" |
                    "link" | "meta" | "param" | "source" | "track" | "wbr");
                if void {
                    format!("<{}{}>", tag.to_ascii_lowercase(), attrs_str)
                } else {
                    format!("<{}{}>{}</{}>", tag.to_ascii_lowercase(), attrs_str, children_html, tag.to_ascii_lowercase())
                }
            }
            _ => String::new(),
        }
    } else {
        val.display().to_string()
    }
}

fn element_matches_selector(obj: &boa_engine::object::JsObject, selector: &str, ctx: &mut Context) -> bool {
    let selector = selector.trim();
    if selector.is_empty() { return false; }
    // Handle comma-separated selector lists: match if ANY sub-selector matches.
    for sub in selector.split(',') {
        let sub = sub.trim();
        if element_matches_single_selector(obj, sub, ctx) {
            return true;
        }
    }
    false
}

fn element_matches_single_selector(obj: &boa_engine::object::JsObject, selector: &str, ctx: &mut Context) -> bool {
    let selector = selector.trim();
    if selector.is_empty() { return false; }

    // Handle :not(...) pseudo-class.
    if let Some(rest) = selector.strip_prefix(":not(") {
        if let Some(inner) = rest.strip_suffix(')') {
            return !element_matches_single_selector(obj, inner.trim(), ctx);
        }
    }

    // Parse compound selector: tag.class#id[attr=val]...
    let mut tag = String::new();
    let mut classes: Vec<String> = Vec::new();
    let mut id: Option<String> = None;
    let mut attrs: Vec<(String, Option<String>)> = Vec::new();
    let mut pseudo_not: Option<String> = None;

    // Extract pseudo-class :not(...) first.
    let working = if let Some(idx) = selector.find(":not(") {
        let before = &selector[..idx];
        let after_paren = &selector[idx + 5..];
        if let Some(end) = after_paren.find(')') {
            pseudo_not = Some(after_paren[..end].to_string());
        }
        before.to_string()
    } else {
        selector.to_string()
    };

    // Parse the compound selector character by character.
    let mut chars = working.chars().peekable();
    let mut current = String::new();
    let mut state = 't'; // t=tag, .=class, #=id, [=attr
    let mut attr_name = String::new();
    let mut attr_val: Option<String> = None;

    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if state == 't' && !current.is_empty() { tag = current.clone(); }
                else if state == '.' { classes.push(current.clone()); }
                else if state == '#' { id = Some(current.clone()); }
                else if state == ']' { attrs.push((attr_name.clone(), attr_val.clone())); }
                current.clear();
                state = '.';
            }
            '#' => {
                if state == 't' && !current.is_empty() { tag = current.clone(); }
                else if state == '.' { classes.push(current.clone()); }
                else if state == '#' { id = Some(current.clone()); }
                else if state == ']' { attrs.push((attr_name.clone(), attr_val.clone())); }
                current.clear();
                state = '#';
            }
            '[' => {
                if state == 't' && !current.is_empty() { tag = current.clone(); }
                else if state == '.' { classes.push(current.clone()); }
                else if state == '#' { id = Some(current.clone()); }
                current.clear();
                attr_name.clear();
                attr_val = None;
                state = '[';
            }
            ']' => {
                // Parse attr_name=val from current.
                if let Some(eq) = current.find('=') {
                    attr_name = current[..eq].trim().to_string();
                    let v = current[eq + 1..].trim().to_string();
                    attr_val = Some(v.trim_matches('"').trim_matches('\'').to_string());
                } else {
                    attr_name = current.trim().to_string();
                }
                attrs.push((attr_name.clone(), attr_val.clone()));
                current.clear();
                state = ']';
            }
            _ => { current.push(c); }
        }
    }
    // Handle trailing state.
    match state {
        't' => { if !current.is_empty() { tag = current; } }
        '.' => { classes.push(current); }
        '#' => { id = Some(current); }
        '[' => {
            if let Some(eq) = current.find('=') {
                attr_name = current[..eq].trim().to_string();
                let v = current[eq + 1..].trim().to_string();
                attr_val = Some(v.trim_matches('"').trim_matches('\'').to_string());
            } else {
                attr_name = current.trim().to_string();
            }
            attrs.push((attr_name, attr_val));
        }
        _ => {}
    }

    // Check tag name.
    if !tag.is_empty() {
        let tn = obj.get(boa_engine::js_string!("tagName"), ctx).ok()
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        if !tn.eq_ignore_ascii_case(&tag) { return false; }
    }
    // Check classes.
    for cls in &classes {
        let cn = obj.get(boa_engine::js_string!("className"), ctx).ok()
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        if !cn.split_whitespace().any(|c| c.eq_ignore_ascii_case(cls)) { return false; }
    }
    // Check id.
    if let Some(ref id_val) = id {
        let eid = obj.get(boa_engine::js_string!("id"), ctx).ok()
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        if &eid != id_val { return false; }
    }
    // Check attributes.
    for (aname, aval) in &attrs {
        let has = obj.get(boa_engine::js_string!(aname.clone()), ctx).ok();
        match has {
            Some(v) => {
                if let Some(expected) = aval {
                    let actual = v.as_string().map(|s| s.to_std_string_escaped()).unwrap_or_default();
                    if &actual != expected { return false; }
                }
            }
            None => {
                // Check if it's in the NamedNodeMap.
                let found = if let Ok(av2) = obj.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(ao) = av2.as_object() {
                        let len = ao.get(boa_engine::js_string!("length"), ctx).ok()
                            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                        let mut f = false;
                        for i in 0..len {
                            if let Ok(a) = ao.get(i as u32, ctx) {
                                if let Some(a_obj) = a.as_object() {
                                    if let Ok(an) = a_obj.get(boa_engine::js_string!("name"), ctx) {
                                        if an.as_string().map(|s| s.to_std_string_escaped().to_ascii_lowercase()).as_deref() == Some(&aname.to_ascii_lowercase()) {
                                            if let Some(expected) = aval {
                                                if let Ok(av3) = a_obj.get(boa_engine::js_string!("value"), ctx) {
                                                    if av3.as_string().map(|s| s.to_std_string_escaped()).as_deref() == Some(expected.as_str()) { f = true; break; }
                                                }
                                            } else { f = true; break; }
                                        }
                                    }
                                }
                            }
                        }
                        f
                    } else { false }
                } else { false };
                if !found { return false; }
            }
        }
    }
    // Check :not() pseudo-class.
    if let Some(not_sel) = pseudo_not {
        if element_matches_single_selector(obj, &not_sel, ctx) { return false; }
    }
    true
}

/// Throw an IndexSizeError DOMException-like error.
fn throw_index_size(ctx: &mut Context) -> JsResult<JsValue> {
    Err(boa_engine::JsNativeError::typ()
        .with_message("IndexSizeError: The index is not in the allowed range.")
        .into())
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
            // Per spec, createElement converts the argument to string via String().
            // createElement(undefined) → createElement("undefined")
            // createElement(null) → createElement("null")
            let tag = arg_to_string(args, 0, ctx);
            // Validate the tag name per the Name production.
            // First char must be a letter, '_', or ':'. Rest must be letter, digit,
            // '_', ':', '-', '.', or combining char.
            // Validate per HTML spec: first char must be a letter, _, or :.
            // No empty string. The rest can be almost anything except <, >, /.
            if tag.is_empty() || !is_valid_name_first(&tag) {
                return Err(boa_engine::JsNativeError::typ()
                    .with_message("InvalidCharacterError: The string contains an invalid character")
                    .into());
                return Err(boa_engine::JsNativeError::typ()
                    .with_message("InvalidCharacterError: The string contains an invalid character")
                    .into());
            }
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
            // ASCII-only uppercase for tagName (per HTML spec for HTML documents).
            let upper: String = tag.chars().map(|c| if c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c }).collect();
            let lower: String = tag.chars().map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c }).collect();
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
                pd(JsValue::from(boa_engine::js_string!(lower))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("namespaceURI"),
                pd(JsValue::from(boa_engine::js_string!(
                    "http://www.w3.org/1999/xhtml"
                ))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("prefix"),
                pd(JsValue::null()),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("nodeType"),
                pd(JsValue::from(1u32)),
            );
            // nodeValue is set as an accessor (always null) by make_element_handle.
            // textContent is set as an accessor by make_element_handle.
            let _ = handle.insert_property(
                boa_engine::js_string!("ownerDocument"),
                pd(ctx.global_object().get(boa_engine::js_string!("document"), ctx).unwrap_or(JsValue::null())),
            );
            // innerHTML is set as an accessor by make_element_handle.
            let _ = handle.insert_property(
                boa_engine::js_string!("outerHTML"),
                pd(JsValue::from(boa_engine::js_string!(""))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("id"),
                pd(JsValue::from(boa_engine::js_string!(""))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("className"),
                pd(JsValue::from(boa_engine::js_string!(""))),
            );
            // Empty attributes NamedNodeMap.
            let attrs_map = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = attrs_map.insert_property(
                boa_engine::js_string!("length"),
                pd(JsValue::from(0u32)),
            );
            let _ = handle.insert_property(boa_engine::js_string!("attributes"), pd(attrs_map.into()));
            // classList — DOMTokenList backed by className.
            let class_list = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = class_list.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
            // contains(token) — checks if className contains token.
            let cl_contains = NativeFunction::from_copy_closure(|this, args, ctx| {
                let token = arg_string(args, 0);
                if let Some(o) = this.as_object() {
                    // Get className from the parent element (stored as _className on classList).
                    let cn = o.get(boa_engine::js_string!("_className"), ctx).ok()
                        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                        .unwrap_or_default();
                    let found = cn.split_whitespace().any(|c| c == token);
                    return Ok(JsValue::from(found));
                }
                Ok(JsValue::from(false))
            });
            let _ = class_list.insert_property(boa_engine::js_string!("contains"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_contains).build())));
            // add(...tokens)
            let cl_add = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
            let _ = class_list.insert_property(boa_engine::js_string!("add"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_add).build())));
            // remove(...tokens)
            let cl_remove = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
            let _ = class_list.insert_property(boa_engine::js_string!("remove"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_remove).build())));
            // toggle(token)
            let cl_toggle = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::from(false)));
            let _ = class_list.insert_property(boa_engine::js_string!("toggle"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_toggle).build())));
            let _ = handle.insert_property(boa_engine::js_string!("classList"), pd(class_list.into()));
            // Element navigation properties (all null/empty for new elements).
            let _ = handle.insert_property(boa_engine::js_string!("children"),
                pd(JsValue::from(boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()))));
            let _ = handle.insert_property(boa_engine::js_string!("childElementCount"), pd(JsValue::from(0u32)));
            let _ = handle.insert_property(boa_engine::js_string!("firstElementChild"), pd(JsValue::null()));
            let _ = handle.insert_property(boa_engine::js_string!("lastElementChild"), pd(JsValue::null()));
            let _ = handle.insert_property(boa_engine::js_string!("nextElementSibling"), pd(JsValue::null()));
            let _ = handle.insert_property(boa_engine::js_string!("previousElementSibling"), pd(JsValue::null()));
            // firstChild/lastChild/childNodes are set as accessors by make_element_handle.
            let _ = handle.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
            let _ = handle.insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
            let doc_val = ctx.global_object().get(boa_engine::js_string!("document"), ctx).unwrap_or(JsValue::null());
            let _ = handle.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val.clone()));
            // baseURI = document.URL (about:blank by default)
            let base_uri = doc_val.as_object()
                .and_then(|d| d.get(boa_engine::js_string!("URL"), ctx).ok())
                .unwrap_or(JsValue::from(boa_engine::js_string!("about:blank")));
            let _ = handle.insert_property(boa_engine::js_string!("baseURI"), pd(base_uri));
            let _ = handle.insert_property(boa_engine::js_string!("isConnected"), pd(JsValue::from(false)));
            // Set the prototype chain so instanceof works.
            set_element_prototype(&handle, &tag, ctx);
            Ok(handle.into())
        },
        Gc::clone(&bridge),
    );

    // createElementNS(namespace, qualifiedName)
    // Like createElement but respects the namespace and doesn't uppercase for non-HTML.
    let create_el_ns = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            // Handle null/undefined namespace → null (not empty string).
            let ns_raw = args.first().cloned().unwrap_or(JsValue::null());
            let ns_is_null = ns_raw.is_null() || ns_raw.is_undefined();
            let ns = if ns_is_null { String::new() } else { arg_string(args, 0) };
            let qname_raw = args.get(1).cloned().unwrap_or(JsValue::null());
            let qname = if qname_raw.is_null() || qname_raw.is_undefined() {
                "null".to_string()
            } else {
                arg_to_string(args, 1, ctx)
            };
            // Validate qualifiedName per spec.
            if qname.is_empty() {
                return Err(boa_engine::JsNativeError::typ()
                    .with_message("INVALID_CHARACTER_ERR").into());
            }
            // Check first character is valid NameStartChar.
            if !qname.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_' || c == ':').unwrap_or(false) {
                // Check for INVALID_CHARACTER_ERR (invalid first char).
                if qname.chars().next().map(|c| c == '<' || c == '>' || c == '}' || c == '/' || c == '\\' || c.is_ascii_digit() || c == '-' || c == '.' || c == ' ' || c == '^' || c == '|' || c == '&' || c == '?' || c == '*' || c == '(' || c == ')' || c == '+' || c == '=' || c == '@' || c == '$' || c == '%' || c == '#' || c == '~' || c == '!' || c == '"' || c == '\'' || c == '`').unwrap_or(false) {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("INVALID_CHARACTER_ERR").into());
                }
            }
            // Check for invalid characters anywhere.
            for c in qname.chars() {
                if c == '<' || c == '>' || c == ' ' || c == '"' || c == '\'' {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("INVALID_CHARACTER_ERR").into());
                }
            }
            // NAMESPACE_ERR: starts with ':' or contains '::' or prefix is xml/xmlns.
            if qname.starts_with(':') {
                return Err(boa_engine::JsNativeError::typ()
                    .with_message("NAMESPACE_ERR").into());
            }
            if qname.contains("::") {
                return Err(boa_engine::JsNativeError::typ()
                    .with_message("NAMESPACE_ERR").into());
            }
            // NAMESPACE_ERR checks per spec.
            if let Some(idx) = qname.find(':') {
                let prefix_str = &qname[..idx];
                let local_name = &qname[idx + 1..];
                // Namespace is null/empty but qualifiedName has prefix → NAMESPACE_ERR.
                if ns_is_null || ns.is_empty() {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR").into());
                }
                // Prefix 'xml' but namespace is not XML namespace.
                if prefix_str == "xml" && ns != "http://www.w3.org/XML/1998/namespace" {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR").into());
                }
                // Prefix 'xmlns' but namespace is not XMLNS namespace.
                if prefix_str == "xmlns" && ns != "http://www.w3.org/2000/xmlns/" {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR").into());
                }
                // localName 'xmlns' but namespace is not XMLNS namespace.
                if local_name == "xmlns" && ns != "http://www.w3.org/2000/xmlns/" {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR").into());
                }
            } else {
                // No prefix. localName 'xmlns' without XMLNS namespace → NAMESPACE_ERR.
                if qname == "xmlns" && ns != "http://www.w3.org/2000/xmlns/" {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR").into());
                }
            }
            // Parse prefix:localName from qname.
            let (prefix, local) = if let Some(idx) = qname.find(':') {
                (&qname[..idx], &qname[idx + 1..])
            } else {
                ("", qname.as_str())
            };
            let html_ns = "http://www.w3.org/1999/xhtml";
            let is_html = ns == html_ns;
            // For HTML namespace, tagName/nodeName is ASCII-uppercased full qualifiedName; otherwise preserve case.
            let tag_name = if is_html {
                qname.chars().map(|c| if c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c }).collect::<String>()
            } else {
                qname.clone()
            };
            let tag_name_clone = tag_name.clone();
            let pid = {
                let mut bb = b.borrow_mut();
                let pid = bb.next_pending;
                bb.next_pending += 1;
                bb.pending.insert(pid, (local.to_string(), String::new(), Vec::new()));
                pid
            };
            let handle = make_element_handle(ctx, Gc::clone(b), 0, Some(pid))?;
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val).writable(true).enumerable(true).configurable(true).build()
            };
            let _ = handle.insert_property(boa_engine::js_string!("tagName"), pd(JsValue::from(boa_engine::js_string!(tag_name))));
            let _ = handle.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!(tag_name_clone))));
            let _ = handle.insert_property(boa_engine::js_string!("localName"), pd(JsValue::from(boa_engine::js_string!(local))));
            // namespaceURI: null if namespace was null/undefined/empty, else the namespace.
            let ns_val = if ns_is_null || ns.is_empty() {
                JsValue::null()
            } else {
                JsValue::from(boa_engine::js_string!(ns.clone()))
            };
            let _ = handle.insert_property(boa_engine::js_string!("namespaceURI"), pd(ns_val));
            // prefix: null if no colon, else the prefix part.
            let _ = handle.insert_property(boa_engine::js_string!("prefix"),
                pd(if prefix.is_empty() { JsValue::null() } else { JsValue::from(boa_engine::js_string!(prefix.to_string())) }));
            let _ = handle.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(1u32)));
            // nodeValue is set as an accessor (always null) by make_element_handle.
            // textContent is set as an accessor by make_element_handle.
            let _ = handle.insert_property(boa_engine::js_string!("id"), pd(JsValue::from(boa_engine::js_string!(""))));
            let _ = handle.insert_property(boa_engine::js_string!("className"), pd(JsValue::from(boa_engine::js_string!(""))));
            // ownerDocument = the global document.
            let doc_val = ctx.global_object().get(boa_engine::js_string!("document"), ctx).unwrap_or(JsValue::null());
            let _ = handle.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
            let attrs_map = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = attrs_map.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
            let _ = handle.insert_property(boa_engine::js_string!("attributes"), pd(attrs_map.into()));
            set_element_prototype(&handle, local, ctx);
            // For non-HTML namespace, override prototype to Element.prototype (not HTMLxxxElement).
            if !is_html {
                if let Ok(elem_ctor) = ctx.global_object().get(boa_engine::js_string!("Element"), ctx) {
                    if let Some(ec) = elem_ctor.as_object() {
                        if let Ok(proto_val) = ec.get(boa_engine::js_string!("prototype"), ctx) {
                            if let Some(proto) = proto_val.as_object() {
                                let _ = handle.set_prototype(Some(proto));
                            }
                        }
                    }
                }
            }
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
        .function(create_el_ns, boa_engine::js_string!("createElementNS"), 2)
        .function(query_sel, boa_engine::js_string!("querySelector"), 1)
        .build();

    // Add addEventListener/dispatchEvent to document (EventTarget interface).
    {
        let doc_ref = document.clone();
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val).writable(true).enumerable(true).configurable(true).build()
        };
        let doc_for_add = doc_ref.clone();
        let add_ev = NativeFunction::from_copy_closure_with_captures(move |_this, args, doc_for_add, ctx| {
            let event_type = arg_string(args, 0);
            let handler = args.get(1).cloned().unwrap_or(JsValue::undefined());
            let lmap = doc_for_add.get(boa_engine::js_string!("_js_listeners"), ctx).ok()
                .and_then(|v| v.as_object());
            let lm = match lmap {
                Some(m) => m,
                None => {
                    let m = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let _ = doc_for_add.insert_property(boa_engine::js_string!("_js_listeners"), pd2(m.clone().into()));
                    m
                }
            };
            let type_arr = lm.get(boa_engine::js_string!(event_type.clone()), ctx).ok()
                .and_then(|v| v.as_object());
            let arr = match type_arr {
                Some(a) => a,
                None => {
                    let a = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
                    let _ = a.insert_property(boa_engine::js_string!("length"), pd2(JsValue::from(0u32)));
                    let _ = lm.insert_property(boa_engine::js_string!(event_type.clone()), pd2(a.clone().into()));
                    a
                }
            };
            let len = arr.get(boa_engine::js_string!("length"), ctx).ok()
                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
            let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(true).configurable(true).build() };
            // Determine passive: touch/wheel events are passive by default on document.
            let explicit_passive = args.get(2).and_then(|o| o.as_object())
                .and_then(|o| o.get(boa_engine::js_string!("passive"), ctx).ok())
                .and_then(|v| v.as_boolean());
            let is_passive_event = matches!(event_type.as_str(), "touchstart" | "touchmove" | "wheel" | "mousewheel");
            let passive = explicit_passive.unwrap_or(is_passive_event);
            let listener_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = listener_obj.insert_property(boa_engine::js_string!("callback"), pd2(handler));
            let _ = listener_obj.insert_property(boa_engine::js_string!("passive"), pd2(JsValue::from(passive)));
            let _ = arr.insert_property(len, pd2(listener_obj.into()));
            let _ = arr.insert_property(boa_engine::js_string!("length"), pd2(JsValue::from(len + 1)));
            Ok(JsValue::undefined())
        }, doc_for_add);
        let doc_for_disp = doc_ref.clone();
        let disp_ev = NativeFunction::from_copy_closure_with_captures(move |_this, args, doc_for_disp, ctx| {
            let event = args.first().cloned().unwrap_or(JsValue::null());
            let event_type = event.as_object()
                .and_then(|o| o.get(boa_engine::js_string!("type"), ctx).ok())
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            let mut not_canceled = true;
            if let Ok(lmap) = doc_for_disp.get(boa_engine::js_string!("_js_listeners"), ctx) {
                if let Some(lm) = lmap.as_object() {
                    if let Ok(type_arr) = lm.get(boa_engine::js_string!(event_type), ctx) {
                        if let Some(arr) = type_arr.as_object() {
                            let len = arr.get(boa_engine::js_string!("length"), ctx).ok()
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                            for i in 0..len {
                                if let Ok(listener) = arr.get(i as u32, ctx) {
                                    // Listener is {callback, passive} or raw function.
                                    let (callback, is_passive) = if let Some(lo) = listener.as_object() {
                                        let cb = lo.get(boa_engine::js_string!("callback"), ctx).ok().unwrap_or(listener.clone());
                                        let p = lo.get(boa_engine::js_string!("passive"), ctx).ok()
                                            .and_then(|v| v.as_boolean()).unwrap_or(false);
                                        (cb, p)
                                    } else {
                                        (listener, false)
                                    };
                                    // Set _passive flag on event.
                                    if let Some(ev_obj) = event.as_object() {
                                        let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(false).configurable(true).build() };
                                        let _ = ev_obj.insert_property(boa_engine::js_string!("_passive"), pd2(JsValue::from(is_passive)));
                                    }
                                    if let Some(fn_obj) = callback.as_object() {
                                        if fn_obj.is_callable() {
                                            let _ = fn_obj.call(&JsValue::null(), &[event.clone()], ctx);
                                        }
                                    }
                                }
                                if let Some(ev_obj) = event.as_object() {
                                    let dp = ev_obj.get(boa_engine::js_string!("defaultPrevented"), ctx).ok()
                                        .and_then(|v| v.as_boolean()).unwrap_or(false);
                                    if dp { not_canceled = false; }
                                }
                            }
                        }
                    }
                }
            }
            Ok(JsValue::from(not_canceled))
        }, doc_for_disp);
        let rm_ev = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
        let _ = document.insert_property(boa_engine::js_string!("addEventListener"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), add_ev).build())));
        let _ = document.insert_property(boa_engine::js_string!("dispatchEvent"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), disp_ev).build())));
        let _ = document.insert_property(boa_engine::js_string!("removeEventListener"),
            pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), rm_ev).build())));
    }

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
        |this, args, b, ctx| {
            // HTML attribute names are case-insensitive → lowercase.
            let name = arg_string(args, 0).to_ascii_lowercase();
            let value = arg_string(args, 1);
            if let Some(nid) = read_handle_id(this) {
                b.borrow_mut().ops.push(Op::SetAttr {
                    node_id: nid,
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            // Update the attributes NamedNodeMap and JS-visible properties.
            if let Some(o) = this.as_object() {
                let pd = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                // Update id/className shortcuts.
                if name == "id" {
                    let _ = o.insert_property(
                        boa_engine::js_string!("id"),
                        pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                    );
                } else if name == "class" {
                    let _ = o.insert_property(
                        boa_engine::js_string!("className"),
                        pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                    );
                }
                // Also set the attribute as a direct property.
                let _ = o.insert_property(
                    boa_engine::js_string!(name.clone()),
                    pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                );
                // Update the attributes NamedNodeMap: find or create the Attr.
                if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(attrs) = attrs_val.as_object() {
                        // Build the Attr object.
                        let attr_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = attr_obj.insert_property(boa_engine::js_string!("name"), pd(JsValue::from(boa_engine::js_string!(name.clone()))));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!(name.clone()))));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("value"), pd(JsValue::from(boa_engine::js_string!(value.clone()))));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("nodeValue"), pd(JsValue::from(boa_engine::js_string!(value.clone()))));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(boa_engine::js_string!(value.clone()))));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("localName"), pd(JsValue::from(boa_engine::js_string!(name.clone()))));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("prefix"), pd(JsValue::null()));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("namespaceURI"), pd(JsValue::null()));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("specified"), pd(JsValue::from(true)));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("ownerElement"), pd(JsValue::from(o.clone())));
                        // Find if this attr already exists, else append.
                        let len = attrs.get(boa_engine::js_string!("length"), ctx).ok()
                            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                        let mut found_idx: Option<u32> = None;
                        for i in 0..len {
                            if let Ok(av) = attrs.get(i as u32, ctx) {
                                if let Some(ao) = av.as_object() {
                                    if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                        if an.as_string().map(|s| s.to_std_string_escaped()).as_deref() == Some(&name) {
                                            found_idx = Some(i);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        let idx = found_idx.unwrap_or(len);
                        let _ = attrs.insert_property(idx, pd(attr_obj.into()));
                        if found_idx.is_none() {
                            let _ = attrs.insert_property(
                                boa_engine::js_string!("length"),
                                pd(JsValue::from(len + 1)),
                            );
                        }
                    }
                }
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(set_attr, boa_engine::js_string!("setAttribute"), 2);

    // getAttribute — reads from JS-visible properties (snapshot from creation).
    let get_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        // HTML attribute names are case-insensitive → lowercase for lookup.
        let name = arg_string(args, 0).to_ascii_lowercase();
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

    // hasAttribute — HTML attributes are case-insensitive in HTML documents.
    let has_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0);
        let name_lower = name.to_ascii_lowercase();
        if let Some(o) = this.as_object() {
            // Try exact match first.
            if let Ok(v) = o.get(boa_engine::js_string!(name.clone()), ctx) {
                if !v.is_null() && !v.is_undefined() {
                    return Ok(JsValue::from(true));
                }
            }
            // Try lowercase (HTML case-insensitivity).
            if let Ok(v) = o.get(boa_engine::js_string!(name_lower.clone()), ctx) {
                if !v.is_null() && !v.is_undefined() {
                    return Ok(JsValue::from(true));
                }
            }
            // Also check the attributes NamedNodeMap.
            if let Ok(av) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(ao) = av.as_object() {
                    let len = ao.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(attr) = ao.get(i as u32, ctx) {
                            if let Some(ao2) = attr.as_object() {
                                if let Ok(an) = ao2.get(boa_engine::js_string!("name"), ctx) {
                                    if let Some(an_s) = an.as_string() {
                                        if an_s.to_std_string_escaped().to_ascii_lowercase() == name_lower.as_str() {
                                            return Ok(JsValue::from(true));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(JsValue::from(false))
    });
    init.function(has_attr, boa_engine::js_string!("hasAttribute"), 1);

    // removeAttribute — removes from attributes NamedNodeMap (best-effort).
    let remove_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0).to_ascii_lowercase();
        if let Some(o) = this.as_object() {
            // Remove from attributes NamedNodeMap by decrementing length.
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    if len > 0 {
                        let _ = attrs.insert_property(boa_engine::js_string!("length"),
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(JsValue::from(len - 1)).writable(true).enumerable(true).configurable(true).build());
                    }
                }
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(remove_attr, boa_engine::js_string!("removeAttribute"), 1);

    // toggleAttribute(name, force?) — toggles boolean attribute.
    let toggle_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0);
        // Validate name.
        if name.is_empty() || !is_valid_name_first(&name) {
            return Err(boa_engine::JsNativeError::typ()
                .with_message("InvalidCharacterError")
                .into());
        }
        let lower = name.to_ascii_lowercase();
        let force = args.get(1).and_then(|v| v.as_boolean());
        if let Some(o) = this.as_object() {
            // Check if attribute exists.
            let exists = if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    let mut found = false;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                    if an.as_string().map(|s| s.to_std_string_escaped().to_ascii_lowercase()).as_deref() == Some(&lower) {
                                        found = true; break;
                                    }
                                }
                            }
                        }
                    }
                    found
                } else { false }
            } else { false };
            match force {
                Some(true) => {
                    // Set to empty string.
                    let sfn = o.get(boa_engine::js_string!("setAttribute"), ctx).ok();
                    if let Some(sv) = sfn { if let Some(sf) = sv.as_object() { let _ = sf.call(&JsValue::from(o.clone()), &[JsValue::from(boa_engine::js_string!(lower)), JsValue::from(boa_engine::js_string!(""))], ctx); } }
                    return Ok(JsValue::from(true));
                }
                Some(false) => {
                    if exists {
                        let rfn = o.get(boa_engine::js_string!("removeAttribute"), ctx).ok();
                        if let Some(rv) = rfn { if let Some(rf) = rv.as_object() { let _ = rf.call(&JsValue::from(o.clone()), &[JsValue::from(boa_engine::js_string!(name))], ctx); } }
                    }
                    return Ok(JsValue::from(false));
                }
                None => {
                    if exists {
                        let rfn = o.get(boa_engine::js_string!("removeAttribute"), ctx).ok();
                        if let Some(rv) = rfn { if let Some(rf) = rv.as_object() { let _ = rf.call(&JsValue::from(o.clone()), &[JsValue::from(boa_engine::js_string!(name))], ctx); } }
                        return Ok(JsValue::from(false));
                    } else {
                        let sfn = o.get(boa_engine::js_string!("setAttribute"), ctx).ok();
                        if let Some(sv) = sfn { if let Some(sf) = sv.as_object() { let _ = sf.call(&JsValue::from(o.clone()), &[JsValue::from(boa_engine::js_string!(lower)), JsValue::from(boa_engine::js_string!(""))], ctx); } }
                        return Ok(JsValue::from(true));
                    }
                }
            }
        }
        Ok(JsValue::from(false))
    });
    init.function(toggle_attr, boa_engine::js_string!("toggleAttribute"), 1);

    // getAttributeNode(name) — returns the Attr object or null.
    let get_attr_node = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0).to_ascii_lowercase();
        if let Some(o) = this.as_object() {
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                    if an.as_string().map(|s| s.to_std_string_escaped().to_ascii_lowercase()).as_deref() == Some(&name) {
                                        return Ok(av);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(JsValue::null())
    });
    init.function(get_attr_node, boa_engine::js_string!("getAttributeNode"), 1);

    // hasAttributeNS(ns, name) — simplified: checks local name.
    let has_attr_ns = NativeFunction::from_copy_closure(|this, args, ctx| {
        let _ns = arg_string(args, 0);
        let name = arg_string(args, 1).to_ascii_lowercase();
        if let Some(o) = this.as_object() {
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("localName"), ctx)
                                    .or_else(|_| ao.get(boa_engine::js_string!("name"), ctx)) {
                                    if an.as_string().map(|s| s.to_std_string_escaped().to_ascii_lowercase()).as_deref() == Some(&name) {
                                        return Ok(JsValue::from(true));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(JsValue::from(false))
    });
    init.function(has_attr_ns, boa_engine::js_string!("hasAttributeNS"), 2);

    // setAttributeNS(ns, qualifiedName, value) — namespace-aware attribute setting.
    // Does NOT lowercase the name (unlike setAttribute).
    let set_attr_ns = NativeFunction::from_copy_closure(|this, args, ctx| {
        let ns = arg_string(args, 0);
        let qname = arg_string(args, 1);
        let value = arg_string(args, 2);
        // Parse prefix:localName.
        let (prefix, local) = if let Some(idx) = qname.find(':') {
            (&qname[..idx], &qname[idx + 1..])
        } else {
            ("", qname.as_str())
        };
        if let Some(o) = this.as_object() {
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val).writable(true).enumerable(true).configurable(true).build()
            };
            // Also set as direct property (case-sensitive for NS attributes).
            let _ = o.insert_property(boa_engine::js_string!(local.to_string()), pd(JsValue::from(boa_engine::js_string!(value.clone()))));
            // Update the attributes NamedNodeMap.
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let attr_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let _ = attr_obj.insert_property(boa_engine::js_string!("name"), pd(JsValue::from(boa_engine::js_string!(qname.clone()))));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!(qname.clone()))));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("value"), pd(JsValue::from(boa_engine::js_string!(value.clone()))));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("nodeValue"), pd(JsValue::from(boa_engine::js_string!(value.clone()))));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(boa_engine::js_string!(value.clone()))));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("localName"), pd(JsValue::from(boa_engine::js_string!(local.to_string()))));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("prefix"), pd(if prefix.is_empty() { JsValue::null() } else { JsValue::from(boa_engine::js_string!(prefix.to_string())) }));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("namespaceURI"), pd(if ns.is_empty() { JsValue::null() } else { JsValue::from(boa_engine::js_string!(ns.clone())) }));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("specified"), pd(JsValue::from(true)));
                    let _ = attr_obj.insert_property(boa_engine::js_string!("ownerElement"), pd(JsValue::from(o.clone())));
                    // Find if attr with same localName already exists.
                    let len = attrs.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    let mut found_idx: Option<u32> = None;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("localName"), ctx) {
                                    if an.as_string().map(|s| s.to_std_string_escaped()).as_deref() == Some(local) {
                                        found_idx = Some(i); break;
                                    }
                                }
                            }
                        }
                    }
                    let idx = found_idx.unwrap_or(len);
                    let _ = attrs.insert_property(idx, pd(attr_obj.into()));
                    if found_idx.is_none() {
                        let _ = attrs.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(len + 1)));
                    }
                }
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(set_attr_ns, boa_engine::js_string!("setAttributeNS"), 3);

    // getAttributeNS(ns, name) — returns value or null. Case-sensitive (no lowercasing).
    let get_attr_ns = NativeFunction::from_copy_closure(|this, args, ctx| {
        let _ns = arg_string(args, 0);
        let name = arg_string(args, 1);
        if let Some(o) = this.as_object() {
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("localName"), ctx)
                                    .or_else(|_| ao.get(boa_engine::js_string!("name"), ctx)) {
                                    if an.as_string().map(|s| s.to_std_string_escaped()).as_deref() == Some(&name) {
                                        if let Ok(val) = ao.get(boa_engine::js_string!("value"), ctx) {
                                            return Ok(val);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(JsValue::null())
    });
    init.function(get_attr_ns, boa_engine::js_string!("getAttributeNS"), 2);

    // removeAttributeNS — no-op stub.
    let remove_attr_ns = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
    init.function(remove_attr_ns, boa_engine::js_string!("removeAttributeNS"), 2);

    // hasAttributes() — true if attributes.length > 0.
    let has_attributes = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(o) = this.as_object() {
            if let Ok(av) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(ao) = av.as_object() {
                    let len = ao.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0);
                    return Ok(JsValue::from(len > 0.0));
                }
            }
        }
        Ok(JsValue::from(false))
    });
    init.function(has_attributes, boa_engine::js_string!("hasAttributes"), 0);

    // addEventListener
    let add_listener = NativeFunction::from_copy_closure(|this, args, ctx| {
        let event_type = arg_string(args, 0);
        let handler = args.get(1).cloned().unwrap_or(JsValue::undefined());
        // Check if options specify passive.
        let explicit_passive = args.get(2).and_then(|o| o.as_object())
            .and_then(|o| o.get(boa_engine::js_string!("passive"), ctx).ok())
            .and_then(|v| v.as_boolean());
        // Determine default passive: touchstart, touchmove, wheel, mousewheel
        // are passive by default on window/document/documentElement/body.
        let is_passive_event = matches!(event_type.as_str(),
            "touchstart" | "touchmove" | "wheel" | "mousewheel");
        // Check if this target is a passive-by-default target.
        // Window: this === global object. Document: nodeName=#document.
        // documentElement: nodeName=HTML. body: nodeName=BODY.
        let is_window = this.as_object().map(|o| {
            // Check if this is the global object by comparing with ctx.global_object().
            boa_engine::object::JsObject::equals(&o, &ctx.global_object())
        }).unwrap_or(false);
        let target_name = this.as_object()
            .and_then(|o| o.get(boa_engine::js_string!("nodeName"), ctx).ok())
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        let is_passive_target = is_window
            || target_name == "#document"
            || target_name == "HTML" || target_name == "BODY";
        let passive = explicit_passive.unwrap_or(is_passive_event && is_passive_target);
        if let Some(o) = this.as_object() {
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val).writable(true).enumerable(true).configurable(true).build()
            };
            // Get or create _js_listeners map on this object.
            let listeners = o.get(boa_engine::js_string!("_js_listeners"), ctx).ok()
                .and_then(|v| v.as_object());
            let lmap = match listeners {
                Some(m) => m,
                None => {
                    let m = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let _ = o.insert_property(boa_engine::js_string!("_js_listeners"), pd(m.clone().into()));
                    m
                }
            };
            // Get or create the array for this event type.
            let type_arr = lmap.get(boa_engine::js_string!(event_type.clone()), ctx).ok()
                .and_then(|v| v.as_object());
            let arr = match type_arr {
                Some(a) => a,
                None => {
                    let a = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let _ = a.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
                    let _ = lmap.insert_property(boa_engine::js_string!(event_type.clone()), pd(a.clone().into()));
                    a
                }
            };
            let len = arr.get(boa_engine::js_string!("length"), ctx).ok()
                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
            // Store handler as {callback, passive} object.
            let listener_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = listener_obj.insert_property(boa_engine::js_string!("callback"), pd(handler));
            let _ = listener_obj.insert_property(boa_engine::js_string!("passive"), pd(JsValue::from(passive)));
            let _ = arr.insert_property(len, pd(listener_obj.into()));
            let _ = arr.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(len + 1)));
        }
        Ok(JsValue::undefined())
    });
    init.function(add_listener, boa_engine::js_string!("addEventListener"), 2);

    // dispatchEvent(event) — calls listeners registered via addEventListener
    // + on<type> property handlers.
    let dispatch = NativeFunction::from_copy_closure(|this, args, ctx| {
        let event = args.first().cloned().unwrap_or(JsValue::null());
        let event_type = event.as_object()
            .and_then(|o| o.get(boa_engine::js_string!("type"), ctx).ok())
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        // Set event.target = this.
        if let Some(ev_obj) = event.as_object() {
            let _ = ev_obj.insert_property(
                boa_engine::js_string!("target"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(this.clone())
                    .writable(true).enumerable(true).configurable(true).build(),
            );
            let _ = ev_obj.insert_property(
                boa_engine::js_string!("currentTarget"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(this.clone())
                    .writable(true).enumerable(true).configurable(true).build(),
            );
        }
        let mut not_canceled = true;
        // 1. Call on<type> property handler.
        if !event_type.is_empty() {
            if let Some(o) = this.as_object() {
                let handler_prop = format!("on{}", event_type);
                if let Ok(handler) = o.get(boa_engine::js_string!(handler_prop), ctx) {
                    if let Some(fn_obj) = handler.as_object() {
                        if fn_obj.is_callable() {
                            let _ = fn_obj.call(&this, &[event.clone()], ctx);
                            // Check defaultPrevented.
                            if let Some(ev_obj) = event.as_object() {
                                let dp = ev_obj.get(boa_engine::js_string!("defaultPrevented"), ctx).ok()
                                    .and_then(|v| v.as_boolean()).unwrap_or(false);
                                if dp { not_canceled = false; }
                            }
                        }
                    }
                }
            }
        }
        // 2. Call addEventListener listeners from _js_listeners.
        if let Some(o) = this.as_object() {
            if let Ok(lmap) = o.get(boa_engine::js_string!("_js_listeners"), ctx) {
                if let Some(lm) = lmap.as_object() {
                    if let Ok(type_arr) = lm.get(boa_engine::js_string!(event_type.clone()), ctx) {
                        if let Some(arr) = type_arr.as_object() {
                            let len = arr.get(boa_engine::js_string!("length"), ctx).ok()
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                            for i in 0..len {
                                if let Ok(listener) = arr.get(i as u32, ctx) {
                                    // Listener is {callback, passive} or raw function.
                                    let (callback, is_passive) = if let Some(lo) = listener.as_object() {
                                        let cb = lo.get(boa_engine::js_string!("callback"), ctx).ok().unwrap_or(JsValue::undefined());
                                        let p = lo.get(boa_engine::js_string!("passive"), ctx).ok()
                                            .and_then(|v| v.as_boolean()).unwrap_or(false);
                                        (cb, p)
                                    } else {
                                        (listener, false)
                                    };
                                    // Set _passive flag on event so preventDefault is a no-op.
                                    if let Some(ev_obj) = event.as_object() {
                                        let pd2 = |val: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(val).writable(true).enumerable(false).configurable(true).build() };
                                        let _ = ev_obj.insert_property(boa_engine::js_string!("_passive"), pd2(JsValue::from(is_passive)));
                                    }
                                    if let Some(fn_obj) = callback.as_object() {
                                        if fn_obj.is_callable() {
                                            let _ = fn_obj.call(&this, &[event.clone()], ctx);
                                        } else if let Ok(he) = fn_obj.get(boa_engine::js_string!("handleEvent"), ctx) {
                                            if let Some(he_fn) = he.as_object() {
                                                if he_fn.is_callable() {
                                                    let _ = he_fn.call(&callback, &[event.clone()], ctx);
                                                }
                                            }
                                        }
                                    }
                                }
                                // Check defaultPrevented after each handler.
                                if let Some(ev_obj) = event.as_object() {
                                    let dp = ev_obj.get(boa_engine::js_string!("defaultPrevented"), ctx).ok()
                                        .and_then(|v| v.as_boolean()).unwrap_or(false);
                                    if dp { not_canceled = false; }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(JsValue::from(not_canceled))
    });
    init.function(dispatch, boa_engine::js_string!("dispatchEvent"), 1);

    // removeEventListener — removes from _js_listeners.
    let remove_listener = NativeFunction::from_copy_closure(|this, args, ctx| {
        let event_type = arg_string(args, 0);
        let handler_obj = args.get(1).and_then(|v| v.as_object());
        if let Some(o) = this.as_object() {
            if let Ok(lmap) = o.get(boa_engine::js_string!("_js_listeners"), ctx) {
                if let Some(lm) = lmap.as_object() {
                    if let Ok(type_arr) = lm.get(boa_engine::js_string!(event_type.clone()), ctx) {
                        if let Some(arr) = type_arr.as_object() {
                            let len = arr.get(boa_engine::js_string!("length"), ctx).ok()
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                            let new_arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                            let pd = |val: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(val).writable(true).enumerable(true).configurable(true).build()
                            };
                            let mut new_len = 0u32;
                            for i in 0..len {
                                if let Ok(h) = arr.get(i as u32, ctx) {
                                    let matches = match (&handler_obj, h.as_object()) {
                                        (Some(target), Some(ho)) => boa_engine::object::JsObject::equals(target, &ho),
                                        _ => false,
                                    };
                                    if !matches {
                                        let _ = new_arr.insert_property(new_len, pd(h));
                                        new_len += 1;
                                    }
                                }
                            }
                            let _ = new_arr.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(new_len)));
                            let _ = lm.insert_property(boa_engine::js_string!(event_type),
                                pd(JsValue::from(new_arr)));
                        }
                    }
                }
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(remove_listener, boa_engine::js_string!("removeEventListener"), 2);

    // appendChild
    let append = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            let child = args.first().cloned().unwrap_or(JsValue::null());
            let parent_id = read_handle_id(this);
            let child_pending = read_pending(&child);
            if let (Some(parent_id), Some(child_pending)) = (parent_id, child_pending) {
                b.borrow_mut().ops.push(Op::AppendChild {
                    parent_id,
                    pending_id: child_pending,
                });
            }
            // JS-level children tracking via insert_into_children (uses JsArray).
            if let (Some(parent_obj), Some(child_obj)) = (this.as_object(), child.as_object()) {
                insert_into_children(&parent_obj, &child_obj, None, ctx);
            }
            Ok(child)
        },
        Gc::clone(&bridge),
    );
    init.function(append, boa_engine::js_string!("appendChild"), 1);

    // removeChild — returns the removed child + updates _children.
    let remove_child = NativeFunction::from_copy_closure(|this, args, ctx| {
        let child = args.first().cloned().unwrap_or(JsValue::null());
        // Throw TypeError for null/undefined/non-object.
        if child.is_null() || child.is_undefined() || child.as_object().is_none() {
            return Err(boa_engine::JsNativeError::typ()
                .with_message("Argument is not a Node")
                .into());
        }
        let child_obj = child.as_object().unwrap();
        let parent_obj = match this.as_object() {
            Some(o) => o,
            None => return Ok(child),
        };
        // Check if child is actually in parent's _children.
        let in_children = parent_obj.get(boa_engine::js_string!("_children"), ctx).ok()
            .and_then(|v| v.as_object())
            .map(|arr| {
                let len = arr.get(boa_engine::js_string!("length"), ctx).ok()
                    .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                (0..len).any(|i| {
                    arr.get(i as u32, ctx).ok()
                        .and_then(|v| v.as_object())
                        .map(|o| boa_engine::object::JsObject::equals(&o, &child_obj))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if !in_children {
            // Also check bridge snapshot for parsed DOM nodes.
            let parent_id = read_handle_id(&JsValue::from(parent_obj.clone()));
            let child_id = read_handle_id(&child);
            if parent_id.is_some() && child_id.is_some() {
                // Both are bridge nodes — assume it's in the tree.
                // (We can't easily check the bridge tree here.)
            } else {
                return Err(boa_engine::JsNativeError::typ()
                    .with_message("NotFoundError: The node to be removed is not a child of this node")
                    .into());
            }
        }
        remove_from_children(&parent_obj, &child_obj, ctx);
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val).writable(true).enumerable(true).configurable(true).build()
        };
        let _ = child_obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
        let _ = child_obj.insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
        Ok(child)
    });
    init.function(remove_child, boa_engine::js_string!("removeChild"), 1);

    // insertBefore — inserts child before reference node in _children.
    let insert_before = NativeFunction::from_copy_closure(|this, args, ctx| {
        let child = args.first().cloned().unwrap_or(JsValue::null());
        let ref_node = args.get(1).cloned().unwrap_or(JsValue::null());
        if let (Some(parent_obj), Some(child_obj)) = (this.as_object(), child.as_object()) {
            insert_into_children(&parent_obj, &child_obj, ref_node.as_object(), ctx);
        }
        Ok(child)
    });
    init.function(insert_before, boa_engine::js_string!("insertBefore"), 2);

    // cloneNode(deep) — returns a copy with all properties cloned from `this`.
    // Deep clone also clones childNodes recursively.
    let clone = NativeFunction::from_copy_closure(|this, args, ctx| {
        let deep = args.first().and_then(|v| v.as_boolean()).unwrap_or(false);
        let this_obj = this.as_object();
        clone_node_js(this_obj.as_ref(), deep, ctx)
    });
    init.function(clone, boa_engine::js_string!("cloneNode"), 0);

    // contains(node) — check if this node contains another by walking up
    // the parentNode chain.
    let contains_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let target = args.first().cloned().unwrap_or(JsValue::null());
        let this_obj = match this.as_object() {
            Some(o) => o,
            None => return Ok(JsValue::from(false)),
        };
        let target_obj = match target.as_object() {
            Some(o) => o,
            None => return Ok(JsValue::from(false)),
        };
        // Same object → true.
        if boa_engine::object::JsObject::equals(&this_obj, &target_obj) {
            return Ok(JsValue::from(true));
        }
        // Walk up target's parentNode chain (limited to prevent stack overflow).
        let mut current = target_obj;
        for _ in 0..100 {
            let parent = current.get(boa_engine::js_string!("parentNode"), ctx).ok();
            match parent.and_then(|v| v.as_object()) {
                Some(p) => {
                    if boa_engine::object::JsObject::equals(&p, &this_obj) {
                        return Ok(JsValue::from(true));
                    }
                    current = p;
                }
                None => break,
            }
        }
        Ok(JsValue::from(false))
    });
    init.function(contains_fn, boa_engine::js_string!("contains"), 1);

    // replaceChild(newChild, oldChild) — returns oldChild (simplified).
    let replace_child = NativeFunction::from_copy_closure(|this, args, ctx| {
        // Per spec: if newChild or oldChild is null/undefined, throw TypeError.
        let new_child = args.first().cloned().unwrap_or(JsValue::null());
        let old_child = args.get(1).cloned().unwrap_or(JsValue::null());
        if new_child.is_null() || new_child.is_undefined()
            || old_child.is_null() || old_child.is_undefined()
        {
            return Err(boa_engine::JsNativeError::typ()
                .with_message("Argument is not an object")
                .into());
        }
        // JS-level: replace old_child with new_child in _children.
        if let Some(parent_obj) = this.as_object() {
            if let (Some(new_obj), Some(old_obj)) = (new_child.as_object(), old_child.as_object()) {
                // Insert new_child before old_child, then remove old_child.
                insert_into_children(&parent_obj, &new_obj, Some(old_obj.clone()), ctx);
                remove_from_children(&parent_obj, &old_obj, ctx);
            }
        }
        Ok(old_child)
    });
    init.function(replace_child, boa_engine::js_string!("replaceChild"), 2);

    // hasChildNodes() — true if childNodes.length > 0.
    let has_child_nodes = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(o) = this.as_object() {
            if let Ok(cn) = o.get(boa_engine::js_string!("childNodes"), ctx) {
                if let Some(cn_obj) = cn.as_object() {
                    let len = cn_obj.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0);
                    return Ok(JsValue::from(len > 0.0));
                }
            }
        }
        Ok(JsValue::from(false))
    });
    init.function(has_child_nodes, boa_engine::js_string!("hasChildNodes"), 0);

    // isEqualNode(other) — structural equality check (simplified: same tagName +
    // same textContent + same number of children).
    let is_equal_node = NativeFunction::from_copy_closure(|this, args, ctx| {
        let other = args.first().cloned().unwrap_or(JsValue::null());
        let this_obj = match this.as_object() {
            Some(o) => o,
            None => return Ok(JsValue::from(false)),
        };
        let other_obj = match other.as_object() {
            Some(o) => o,
            None => return Ok(JsValue::from(false)),
        };
        if boa_engine::object::JsObject::equals(&this_obj, &other_obj) {
            return Ok(JsValue::from(true));
        }
        // Compare nodeType.
        let nt1 = this_obj.get(boa_engine::js_string!("nodeType"), ctx).unwrap_or_default();
        let nt2 = other_obj.get(boa_engine::js_string!("nodeType"), ctx).unwrap_or_default();
        if nt1 != nt2 { return Ok(JsValue::from(false)); }
        // Compare nodeName.
        let tn1 = this_obj.get(boa_engine::js_string!("nodeName"), ctx).unwrap_or_default();
        let tn2 = other_obj.get(boa_engine::js_string!("nodeName"), ctx).unwrap_or_default();
        if tn1 != tn2 { return Ok(JsValue::from(false)); }

        let nt_val = nt1.as_number().unwrap_or(0.0) as u32;
        match nt_val {
            // DocumentType: compare name, publicId, systemId
            10 => {
                for prop in &["name", "publicId", "systemId"] {
                    let v1 = this_obj.get(boa_engine::js_string!(*prop), ctx).unwrap_or_default();
                    let v2 = other_obj.get(boa_engine::js_string!(*prop), ctx).unwrap_or_default();
                    if v1 != v2 { return Ok(JsValue::from(false)); }
                }
                Ok(JsValue::from(true))
            }
            // Element: compare namespaceURI, prefix, localName, attributes
            1 => {
                for prop in &["namespaceURI", "prefix", "localName"] {
                    let v1 = this_obj.get(boa_engine::js_string!(*prop), ctx).unwrap_or_default();
                    let v2 = other_obj.get(boa_engine::js_string!(*prop), ctx).unwrap_or_default();
                    if v1 != v2 { return Ok(JsValue::from(false)); }
                }
                // Compare attributes count (simplified).
                let a1 = this_obj.get(boa_engine::js_string!("attributes"), ctx).ok()
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(boa_engine::js_string!("length"), ctx).ok())
                    .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                let a2 = other_obj.get(boa_engine::js_string!("attributes"), ctx).ok()
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(boa_engine::js_string!("length"), ctx).ok())
                    .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                if a1 != a2 { return Ok(JsValue::from(false)); }
                Ok(JsValue::from(true))
            }
            // Text/Comment/PI: compare data/textContent
            3 | 8 | 7 => {
                let tc1 = this_obj.get(boa_engine::js_string!("textContent"), ctx).unwrap_or_default();
                let tc2 = other_obj.get(boa_engine::js_string!("textContent"), ctx).unwrap_or_default();
                Ok(JsValue::from(tc1 == tc2))
            }
            _ => Ok(JsValue::from(true)),
        }
    });
    init.function(is_equal_node, boa_engine::js_string!("isEqualNode"), 1);

    // isSameNode(other) — reference equality.
    let is_same_node = NativeFunction::from_copy_closure(|this, args, _ctx| {
        let this_obj = this.as_object();
        let other_obj = args.first().and_then(|v| v.as_object());
        match (this_obj, other_obj) {
            (Some(a), Some(b)) => Ok(JsValue::from(boa_engine::object::JsObject::equals(&a, &b))),
            _ => Ok(JsValue::from(false)),
        }
    });
    init.function(is_same_node, boa_engine::js_string!("isSameNode"), 1);

    // compareDocumentPosition(other) — returns 0 if same, otherwise a bitmask.
    let compare_pos = NativeFunction::from_copy_closure(|this, args, _ctx| {
        let this_obj = this.as_object();
        let other_obj = args.first().and_then(|v| v.as_object());
        match (this_obj, other_obj) {
            (Some(a), Some(b)) => {
                if boa_engine::object::JsObject::equals(&a, &b) {
                    Ok(JsValue::from(0u32))
                } else {
                    // Return DOCUMENT_POSITION_DISCONNECTED | DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC
                    Ok(JsValue::from(0x01u32 | 0x20u32))
                }
            }
            _ => Ok(JsValue::from(0u32)),
        }
    });
    init.function(compare_pos, boa_engine::js_string!("compareDocumentPosition"), 1);

    // getRootNode() — returns this (simplified; no composed tree walking).
    let get_root_node = NativeFunction::from_copy_closure(|this, _args, ctx| {
        let mut current = this.as_object();
        for _ in 0..100 {
            let parent = current.as_ref().and_then(|o| {
                o.get(boa_engine::js_string!("parentNode"), ctx).ok()
                    .and_then(|v| v.as_object())
            });
            match parent {
                Some(p) => current = Some(p),
                None => break,
            }
        }
        match current {
            Some(o) => Ok(JsValue::from(o)),
            None => Ok(this.clone()),
        }
    });
    init.function(get_root_node, boa_engine::js_string!("getRootNode"), 0);

    // lookupNamespaceURI(prefix) — returns namespace URI for given prefix.
    // Simplified: always null except 'xml' → XML namespace, 'xmlns' → xmlns namespace.
    let lookup_ns_uri = NativeFunction::from_copy_closure(|this, args, ctx| {
        let prefix = args.first().and_then(|v| {
            if v.is_null() { None } else { v.as_string().map(|s| s.to_std_string_escaped()) }
        });
        let prefix_str = prefix.as_deref().unwrap_or("");
        const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
        const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

        // Special prefixes always return their well-known namespaces.
        if prefix_str == "xml" {
            return Ok(JsValue::from(boa_engine::js_string!(XML_NS)));
        }
        if prefix_str == "xmlns" {
            return Ok(JsValue::from(boa_engine::js_string!(XMLNS_NS)));
        }

        let obj = match this.as_object() {
            Some(o) => o,
            None => return Ok(JsValue::null()),
        };

        // Walk up the parent chain looking for namespace declarations.
        let mut current = Some(obj);
        for _ in 0..100 {
            let node = match current {
                Some(n) => n,
                None => break,
            };
            let nt = node.get(boa_engine::js_string!("nodeType"), ctx).ok()
                .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;

            // For element nodes (type 1), check namespace declarations.
            if nt == 1 {
                // Check if this element's own prefix matches.
                if !prefix_str.is_empty() {
                    let elem_prefix = node.get(boa_engine::js_string!("prefix"), ctx).ok()
                        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()));
                    if elem_prefix.as_deref() == Some(prefix_str) {
                        let ns = node.get(boa_engine::js_string!("namespaceURI"), ctx).ok();
                        return Ok(ns.filter(|v| !v.is_null()).unwrap_or(JsValue::null()));
                    }
                }
                // Check xmlns attributes.
                if let Ok(attrs) = node.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(ao) = attrs.as_object() {
                        let alen = ao.get(boa_engine::js_string!("length"), ctx).ok()
                            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                        for i in 0..alen {
                            if let Ok(a) = ao.get(i as u32, ctx) {
                                if let Some(a_obj) = a.as_object() {
                                    let aname = a_obj.get(boa_engine::js_string!("name"), ctx).ok()
                                        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                                        .unwrap_or_default();
                                    let aval = a_obj.get(boa_engine::js_string!("value"), ctx).ok()
                                        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                                        .unwrap_or_default();
                                    // xmlns:prefix → matches non-empty prefix
                                    if prefix_str.is_empty() {
                                        if aname == "xmlns" {
                                            return Ok(JsValue::from(boa_engine::js_string!(aval)));
                                        }
                                    } else {
                                        if aname == format!("xmlns:{}", prefix_str) {
                                            return Ok(JsValue::from(boa_engine::js_string!(aval)));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Move to parent.
            current = node.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
        }
        Ok(JsValue::null())
    });
    init.function(lookup_ns_uri, boa_engine::js_string!("lookupNamespaceURI"), 1);

    // lookupPrefix(namespace) — returns prefix for given namespace (simplified: null).
    let lookup_prefix = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::null()));
    init.function(lookup_prefix, boa_engine::js_string!("lookupPrefix"), 1);

    // isDefaultNamespace(namespace) — checks if lookupNamespaceURI(null) === namespace.
    let is_default_ns = NativeFunction::from_copy_closure(|this, args, ctx| {
        let ns_val = args.first().cloned().unwrap_or(JsValue::null());
        let ns_str = if ns_val.is_null() {
            String::new()
        } else {
            ns_val.as_string().map(|s| s.to_std_string_escaped()).unwrap_or_default()
        };
        // Get the default namespace by calling lookupNamespaceURI(null).
        let lookup_fn = this.as_object()
            .and_then(|o| o.get(boa_engine::js_string!("lookupNamespaceURI"), ctx).ok())
            .and_then(|v| v.as_object());
        if let Some(lf) = lookup_fn {
            if let Ok(result) = lf.call(this, &[JsValue::null()], ctx) {
                let result_str = result.as_string().map(|s| s.to_std_string_escaped()).unwrap_or_default();
                return Ok(JsValue::from(result_str == ns_str));
            }
        }
        // Fallback: default namespace is null/empty.
        Ok(JsValue::from(ns_str.is_empty()))
    });
    init.function(is_default_ns, boa_engine::js_string!("isDefaultNamespace"), 1);

    // normalize() — no-op.
    let normalize_fn = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
    init.function(normalize_fn, boa_engine::js_string!("normalize"), 0);

    // remove() — removes this node from its parent's _children.
    let remove_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                remove_from_children(&parent_obj, &obj, ctx);
                let pd = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val).writable(true).enumerable(true).configurable(true).build()
                };
                let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
                let _ = obj.insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(remove_fn, boa_engine::js_string!("remove"), 0);

    // append(...nodes) — appends all arguments as children.
    let append_children = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(parent_obj) = this.as_object() {
            for arg in args.iter() {
                let node = value_to_node(arg, ctx);
                if let Some(child_obj) = node.as_object() {
                    insert_into_children(&parent_obj, &child_obj, None, ctx);
                }
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(append_children, boa_engine::js_string!("append"), 0);

    // prepend(...nodes) — inserts all arguments before the first child.
    let prepend_children = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(parent_obj) = this.as_object() {
            // Get first child as reference.
            let first_child = parent_obj.get(boa_engine::js_string!("_children"), ctx).ok()
                .and_then(|v| v.as_object())
                .and_then(|a| a.get(0u32, ctx).ok())
                .and_then(|v| v.as_object());
            for arg in args.iter() {
                let node = value_to_node(arg, ctx);
                if let Some(child_obj) = node.as_object() {
                    insert_into_children(&parent_obj, &child_obj, first_child.clone(), ctx);
                }
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(prepend_children, boa_engine::js_string!("prepend"), 0);

    // after(...nodes) — inserts siblings after this node in parent._children.
    let after_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                // Find next sibling as reference for insertion.
                let next_sibling = get_next_sibling(&parent_obj, &obj, ctx);
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&parent_obj, &child_obj, next_sibling.clone(), ctx);
                    }
                }
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(after_fn, boa_engine::js_string!("after"), 0);

    // before(...nodes) — inserts siblings before this node in parent._children.
    let before_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&parent_obj, &child_obj, Some(obj.clone()), ctx);
                    }
                }
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(before_fn, boa_engine::js_string!("before"), 0);

    // replaceWith(...nodes) — replaces this node with the arguments.
    let replace_with_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                // Insert all args before this node.
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&parent_obj, &child_obj, Some(obj.clone()), ctx);
                    }
                }
                // Remove this node.
                remove_from_children(&parent_obj, &obj, ctx);
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(replace_with_fn, boa_engine::js_string!("replaceWith"), 0);

    // closest(selector) — walks up parentNode chain matching selector.
    let closest_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let selector = arg_string(args, 0);
        if selector.is_empty() { return Ok(JsValue::null()); }
        let mut current = this.as_object();
        for _ in 0..1000 {
            let node = match current.as_ref() {
                Some(o) => o.clone(),
                None => break,
            };
            if element_matches_selector(&node, &selector, ctx) {
                return Ok(JsValue::from(node));
            }
            // Move to parent.
            current = node.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
        }
        Ok(JsValue::null())
    });
    init.function(closest_fn, boa_engine::js_string!("closest"), 1);

    // matches(selector) — check if this element matches a selector.
    let matches_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let selector = arg_string(args, 0);
        if selector.is_empty() { return Ok(JsValue::from(false)); }
        let node = match this.as_object() {
            Some(o) => o,
            None => return Ok(JsValue::from(false)),
        };
        Ok(JsValue::from(element_matches_selector(&node, &selector, ctx)))
    });
    init.function(matches_fn, boa_engine::js_string!("matches"), 1);

    // insertAdjacentElement(position, element) — inserts element relative to this.
    let iae_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let position = arg_string(args, 0).to_ascii_lowercase();
        let element = args.get(1).cloned().unwrap_or(JsValue::null());
        if let (Some(obj), Some(el_obj)) = (this.as_object(), element.as_object()) {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            match position.as_str() {
                "beforebegin" => {
                    if let Some(p) = parent {
                        insert_into_children(&p, &el_obj, Some(obj.clone()), ctx);
                    }
                }
                "afterend" => {
                    if let Some(p) = parent {
                        let next = get_next_sibling(&p, &obj, ctx);
                        insert_into_children(&p, &el_obj, next, ctx);
                    }
                }
                "afterbegin" => {
                    let first_child = if let Ok(cv) = obj.get(boa_engine::js_string!("_children"), ctx) {
                        if let Some(ca) = cv.as_object() {
                            let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                            if v.is_undefined() { None } else { v.as_object() }
                        } else { None }
                    } else { None };
                    insert_into_children(&obj, &el_obj, first_child, ctx);
                }
                "beforeend" => {
                    insert_into_children(&obj, &el_obj, None, ctx);
                }
                _ => {}
            }
        }
        Ok(element)
    });
    init.function(iae_fn, boa_engine::js_string!("insertAdjacentElement"), 2);

    // insertAdjacentText(position, text) — creates text node and inserts it.
    let iat_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let position = arg_string(args, 0).to_ascii_lowercase();
        let text = arg_to_string(args, 1, ctx);
        let pd = |v: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(v).writable(true).enumerable(true).configurable(true).build() };
        // Create a text node.
        let tn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = tn.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32)));
        let _ = tn.insert_property(boa_engine::js_string!("_data"), pd(JsValue::from(boa_engine::js_string!(text.clone()))));
        let _ = tn.insert_property(boa_engine::js_string!("data"), pd(JsValue::from(boa_engine::js_string!(text.clone()))));
        let _ = tn.insert_property(boa_engine::js_string!("nodeValue"), pd(JsValue::from(boa_engine::js_string!(text.clone()))));
        let _ = tn.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(boa_engine::js_string!(text))));
        let _ = tn.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("#text"))));
        if let Ok(text_ctor) = ctx.global_object().get(boa_engine::js_string!("Text"), ctx) {
            if let Some(tc) = text_ctor.as_object() {
                if let Ok(pv) = tc.get(boa_engine::js_string!("prototype"), ctx) {
                    if let Some(p) = pv.as_object() { let _ = tn.set_prototype(Some(p)); }
                }
            }
        }
        if let Some(obj) = this.as_object() {
            let parent = obj.get(boa_engine::js_string!("parentNode"), ctx).ok()
                .and_then(|v| v.as_object());
            match position.as_str() {
                "beforebegin" => {
                    if let Some(p) = parent { insert_into_children(&p, &tn, Some(obj.clone()), ctx); }
                }
                "afterend" => {
                    if let Some(p) = parent {
                        let next = get_next_sibling(&p, &obj, ctx);
                        insert_into_children(&p, &tn, next, ctx);
                    }
                }
                "afterbegin" => {
                    let first_child = if let Ok(cv) = obj.get(boa_engine::js_string!("_children"), ctx) {
                        if let Some(ca) = cv.as_object() {
                            let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                            if v.is_undefined() { None } else { v.as_object() }
                        } else { None }
                    } else { None };
                    insert_into_children(&obj, &tn, first_child, ctx);
                }
                "beforeend" => {
                    insert_into_children(&obj, &tn, None, ctx);
                }
                _ => {}
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(iat_fn, boa_engine::js_string!("insertAdjacentText"), 2);

    // getElementsByTagName — returns array of matching element handles.
    let by_tag = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let tag = arg_string(args, 0).to_lowercase();
            let bb = b.borrow();
            let matching: Vec<u32> = bb.node_props.iter()
                .filter(|(_, s)| {
                    s.node_type == 1 && (tag == "*" || s.tag_name.to_lowercase() == tag)
                })
                .map(|(&id, _)| id)
                .collect();
            drop(bb);
            let arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val).writable(true).enumerable(true).configurable(true).build()
            };
            for (i, nid) in matching.iter().enumerate() {
                let snap = b.borrow().node_props.get(nid).cloned();
                let handle = make_element_handle(ctx, Gc::clone(b), *nid, None)?;
                if let Some(s) = snap {
                    populate_props(&handle, &s, ctx);
                    set_element_prototype(&handle, &s.tag_name, ctx);
                }
                let _ = arr.insert_property(i as u32, pd(handle.into()));
            }
            let _ = arr.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(matching.len() as u32)));
            // item(index) method.
            let item_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(-1.0) as i32;
                if idx < 0 { return Ok(JsValue::null()); }
                if let Some(o) = this.as_object() {
                    let len = o.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    if (idx as u32) < len {
                        return Ok(o.get(idx as u32, ctx).unwrap_or(JsValue::null()));
                    }
                }
                Ok(JsValue::null())
            });
            let _ = arr.insert_property(boa_engine::js_string!("item"),
                pd(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), item_fn).build())));
            // Set HTMLCollection.prototype.
            if let Ok(hc_ctor) = ctx.global_object().get(boa_engine::js_string!("HTMLCollection"), ctx) {
                if let Some(hc) = hc_ctor.as_object() {
                    if let Ok(proto_val) = hc.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = arr.set_prototype(Some(proto));
                        }
                    }
                }
            }
            Ok(arr.into())
        },
        Gc::clone(&bridge),
    );
    init.function(by_tag, boa_engine::js_string!("getElementsByTagName"), 1);

    // getElementsByTagNameNS(ns, localName) — returns matching elements.
    // Simplified: matches by localName (namespace check omitted).
    let by_tag_ns = NativeFunction::from_copy_closure(|_this, args, ctx| {
        let _ns = arg_string(args, 0);
        let local = arg_string(args, 1).to_ascii_lowercase();
        let arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val).writable(true).enumerable(true).configurable(true).build()
        };
        let _ = arr.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
        Ok(arr.into())
    });
    init.function(by_tag_ns, boa_engine::js_string!("getElementsByTagNameNS"), 2);

    // getElementsByClassName — returns array of matching element handles.
    let by_class = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let cls = arg_string(args, 0);
            let wanted: Vec<&str> = cls.split_whitespace().collect();
            let bb = b.borrow();
            let matching: Vec<u32> = bb.node_props.iter()
                .filter(|(_, s)| {
                    s.node_type == 1 && wanted.iter().all(|w| {
                        s.class_name.split_whitespace().any(|c| c == *w)
                    })
                })
                .map(|(&id, _)| id)
                .collect();
            drop(bb);
            let arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val).writable(true).enumerable(true).configurable(true).build()
            };
            for (i, nid) in matching.iter().enumerate() {
                let snap = b.borrow().node_props.get(nid).cloned();
                let handle = make_element_handle(ctx, Gc::clone(b), *nid, None)?;
                if let Some(s) = snap {
                    populate_props(&handle, &s, ctx);
                    set_element_prototype(&handle, &s.tag_name, ctx);
                }
                let _ = arr.insert_property(i as u32, pd(handle.into()));
            }
            let _ = arr.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(matching.len() as u32)));
            Ok(arr.into())
        },
        Gc::clone(&bridge),
    );
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

    let obj = init.build();

    // innerHTML as plain data property (accessor causes stack overflow).
    // innerHTML as accessor: getter serializes _children, setter is no-op.
    let ih_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        let html = if let Some(o) = this.as_object() {
            serialize_children(&o, ctx)
        } else {
            String::new()
        };
        Ok(JsValue::from(boa_engine::js_string!(html)))
    });
    let ih_set = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
    let ih_get_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ih_get).build();
    let ih_set_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ih_set).build();
    let _ = obj.insert_property(
        boa_engine::js_string!("innerHTML"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(ih_get_fn)
            .set(ih_set_fn)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // textContent as accessor: getter concatenates descendant text, setter replaces children.
    let tc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(o) = this.as_object() {
            return Ok(JsValue::from(boa_engine::js_string!(collect_text_content(&o, ctx, 0))));
        }
        Ok(JsValue::from(boa_engine::js_string!("")))
    });
    let tc_set = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(o) = this.as_object() {
            let val = args.first().map(|v| v.clone()).unwrap_or(JsValue::null());
            let s = if val.is_null() {
                String::new()
            } else {
                val.to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_default()
            };
            let pd = |v: JsValue| { boa_engine::property::PropertyDescriptor::builder().value(v).writable(true).enumerable(true).configurable(true).build() };
            // Clear parentNode on all existing children.
            if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                if let Some(ca) = cv.as_object() {
                    let ca = ca.clone();
                    let len = ca.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(child) = ca.get(i as u32, ctx) {
                            if let Some(co) = child.as_object() {
                                let co = co.clone();
                                let _ = co.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
                            }
                        }
                    }
                }
            }
            if s.is_empty() {
                // Empty string → no children.
                let empty_arr = JsArray::new(ctx);
                let _ = o.insert_property(boa_engine::js_string!("_children"), pd(empty_arr.into()));
            } else {
                // Replace all children with a single text node.
                let text_node = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let _ = text_node.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32)));
                let _ = text_node.insert_property(boa_engine::js_string!("_data"), pd(JsValue::from(boa_engine::js_string!(s.clone()))));
                let _ = text_node.insert_property(boa_engine::js_string!("data"), pd(JsValue::from(boa_engine::js_string!(s.clone()))));
                let _ = text_node.insert_property(boa_engine::js_string!("nodeValue"), pd(JsValue::from(boa_engine::js_string!(s.clone()))));
                let _ = text_node.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(boa_engine::js_string!(s))));
                let _ = text_node.insert_property(boa_engine::js_string!("nodeName"), pd(JsValue::from(boa_engine::js_string!("#text"))));
                let _ = text_node.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::from(o.clone())));
                // Set Text.prototype so instanceof Text works.
                if let Ok(text_ctor) = ctx.global_object().get(boa_engine::js_string!("Text"), ctx) {
                    if let Some(tc) = text_ctor.as_object() {
                        if let Ok(proto_val) = tc.get(boa_engine::js_string!("prototype"), ctx) {
                            if let Some(proto) = proto_val.as_object() {
                                let _ = text_node.set_prototype(Some(proto));
                            }
                        }
                    }
                }
                let new_arr = JsArray::new(ctx);
                let _ = new_arr.push(JsValue::from(text_node), ctx);
                let _ = o.insert_property(boa_engine::js_string!("_children"), pd(new_arr.into()));
            }
        }
        Ok(JsValue::undefined())
    });
    let tc_get_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_get).build();
    let tc_set_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_set).build();
    let _ = obj.insert_property(
        boa_engine::js_string!("textContent"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(tc_get_fn)
            .set(tc_set_fn)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // firstChild/lastChild as accessors: read from _children array.
    let fc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(o) = this.as_object() {
            if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                if let Some(ca) = cv.as_object() {
                    let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                    if v.is_undefined() { return Ok(JsValue::null()); }
                    return Ok(v);
                }
            }
        }
        Ok(JsValue::null())
    });
    let lc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(o) = this.as_object() {
            if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                if let Some(ca) = cv.as_object() {
                    let len = ca.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    if len > 0 {
                        return Ok(ca.get(len - 1, ctx).unwrap_or(JsValue::null()));
                    }
                }
            }
        }
        Ok(JsValue::null())
    });
    let fc_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), fc_get).build();
    let lc_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), lc_get).build();
    for (name, getter) in &[("firstChild", fc_fn.clone()), ("lastChild", lc_fn.clone())] {
        let _ = obj.insert_property(
            boa_engine::js_string!(*name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter.clone())
                .enumerable(true)
                .configurable(true)
                .build(),
        );
    }
    // nodeValue for elements: always null (getter returns null, setter is no-op per spec).
    let nv_null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
    let nv_noop_set = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
    let nv_ng = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nv_null_get).build();
    let nv_ns = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nv_noop_set).build();
    let _ = obj.insert_property(
        boa_engine::js_string!("nodeValue"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(nv_ng).set(nv_ns)
            .enumerable(true).configurable(true).build(),
    );
    // childNodes as accessor: returns _children array (or empty array).
    let cn_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(o) = this.as_object() {
            if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                if cv.is_object() {
                    return Ok(cv);
                }
            }
        }
        Ok(JsValue::from(JsArray::new(ctx)))
    });
    let cn_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cn_get).build();
    let _ = obj.insert_property(
        boa_engine::js_string!("childNodes"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(cn_fn)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // previousSibling/nextSibling as accessors: find this in parent's _children.
    let ps_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let (Some(obj), Some(parent)) = (this.as_object(),
            this.as_object().and_then(|o| o.get(boa_engine::js_string!("parentNode"), ctx).ok()).and_then(|v| v.as_object()))
        {
            if let Ok(cv) = parent.get(boa_engine::js_string!("_children"), ctx) {
                if let Some(ca) = cv.as_object() {
                    let len = ca.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(child) = ca.get(i as u32, ctx) {
                            if let Some(co) = child.as_object() {
                                if boa_engine::object::JsObject::equals(&co, &obj) {
                                    if i > 0 {
                                        return Ok(ca.get(i - 1, ctx).unwrap_or(JsValue::null()));
                                    }
                                    return Ok(JsValue::null());
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(JsValue::null())
    });
    let ns_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let (Some(obj), Some(parent)) = (this.as_object(),
            this.as_object().and_then(|o| o.get(boa_engine::js_string!("parentNode"), ctx).ok()).and_then(|v| v.as_object()))
        {
            if let Ok(cv) = parent.get(boa_engine::js_string!("_children"), ctx) {
                if let Some(ca) = cv.as_object() {
                    let len = ca.get(boa_engine::js_string!("length"), ctx).ok()
                        .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(child) = ca.get(i as u32, ctx) {
                            if let Some(co) = child.as_object() {
                                if boa_engine::object::JsObject::equals(&co, &obj) {
                                    if i + 1 < len {
                                        return Ok(ca.get(i + 1, ctx).unwrap_or(JsValue::null()));
                                    }
                                    return Ok(JsValue::null());
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(JsValue::null())
    });
    let ps_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ps_get).build();
    let ns_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ns_get).build();
    for (name, getter) in &[("previousSibling", ps_fn.clone()), ("nextSibling", ns_fn.clone())] {
        let _ = obj.insert_property(
            boa_engine::js_string!(*name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter.clone())
                .enumerable(true)
                .configurable(true)
                .build(),
        );
    }

    Ok(obj)
}
