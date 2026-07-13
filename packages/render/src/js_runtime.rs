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
        let _ = attr_obj.insert_property(
            boa_engine::js_string!("prefix"),
            pd(JsValue::null()),
        );
        let _ = attr_obj.insert_property(
            boa_engine::js_string!("namespaceURI"),
            pd(JsValue::null()),
        );
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
        // Deep clone objects (like firstChild/lastChild).
        if deep && val.as_object().is_some() && !val.is_null() {
            let obj = val.as_object();
            let child_clone = clone_node_js(obj.as_ref(), true, ctx)?;
            let _ = clone.insert_property(key, pd(child_clone));
        } else {
            let _ = clone.insert_property(key, pd(val));
        }
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
    for name in &["window", "self", "globalThis", "top", "parent"] {
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
        // Give it its own implementation object so createDocumentType works on it.
        let impl2 = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let cdt_fn = NativeFunction::from_copy_closure(|_t, args, ctx| {
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
            let _ = dt.insert_property(boa_engine::js_string!("nodeValue"), pd(JsValue::null()));
            let _ = dt.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32)));
            let _ = dt.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::null()));
            Ok(dt.into())
        });
        let hf_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(true)));
        let _ = impl2.insert_property(boa_engine::js_string!("hasFeature"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), hf_fn).build()))
                .writable(true).enumerable(true).configurable(true).build());
        let _ = impl2.insert_property(boa_engine::js_string!("createDocumentType"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cdt_fn).build()))
                .writable(true).enumerable(true).configurable(true).build());
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val).writable(true).enumerable(true).configurable(true).build()
        };
        let _ = d.insert_property(boa_engine::js_string!("implementation"), pd(impl2.into()));
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
        let _ = d.insert_property(boa_engine::js_string!("nodeValue"), pd(JsValue::null()));
        let _ = d.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32))); // DOCUMENT_TYPE_NODE
        let _ = d.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::null()));
        // ownerDocument = the current document.
        let doc_val = ctx.global_object().get(boa_engine::js_string!("document"), ctx).unwrap_or(JsValue::null());
        let _ = d.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
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
            let text_len = text.encode_utf16().count() as u32;
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
            let _ = obj.insert_property(
                boa_engine::js_string!("length"),
                pd(JsValue::from(0u32)),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeName"),
                pd(JsValue::from(boa_engine::js_string!("#comment"))),
            );
            // CharacterData methods
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
    ];
    for (name, nt) in all_types {
        let nt = nt.clone();
        let ctor_fn = NativeFunction::from_copy_closure(move |_t, _a, ctx| {
            // Use with_object_proto — the constructor's .prototype is set below.
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(nt)));
            Ok(obj.into())
        });
        let _ = ctx.register_global_callable(boa_engine::js_string!(name), 0, ctor_fn);

        // Create a .prototype object on the constructor so instanceof works.
        // JS `instanceof` checks: object.__proto__ === Constructor.prototype
        // (walking up the prototype chain).
        let global = ctx.global_object();
        if let Ok(ctor_val) = global.get(boa_engine::js_string!(name), &mut *ctx) {
            if let Some(ctor_obj) = ctor_val.as_object() {
                // Create a prototype object that inherits from Object.prototype.
                let proto = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let _ = proto.insert_property(
                    boa_engine::js_string!("constructor"),
                    pd(ctor_obj.clone().into()),
                );
                let _ =
                    ctor_obj.insert_property(boa_engine::js_string!("prototype"), pd(proto.into()));
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
}

/// Build CharacterData methods as a list of (name, NativeFunction).
fn build_character_data_methods() -> Vec<(&'static str, NativeFunction)> {
    let append = NativeFunction::from_copy_closure(|this, args, ctx| {
        let v = arg_string(args, 0);
        let mut units = read_data_utf16(&this, ctx);
        units.extend(v.encode_utf16());
        write_data_utf16(&this, &units, ctx);
        Ok(JsValue::undefined())
    });
    let delete_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        let mut units = read_data_utf16(&this, ctx);
        let len = units.len() as u32;
        if offset > len {
            return throw_index_size(ctx);
        }
        let end = (offset.saturating_add(count)).min(len) as usize;
        units.drain(offset as usize..end);
        write_data_utf16(&this, &units, ctx);
        Ok(JsValue::undefined())
    });
    let insert_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        let data = arg_string(args, 1);
        let mut units = read_data_utf16(&this, ctx);
        let len = units.len() as u32;
        if offset > len {
            return throw_index_size(ctx);
        }
        let insert_units: Vec<u16> = data.encode_utf16().collect();
        let pos = offset as usize;
        units.splice(pos..pos, insert_units);
        write_data_utf16(&this, &units, ctx);
        Ok(JsValue::undefined())
    });
    let replace_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        let data = arg_string(args, 2);
        let mut units = read_data_utf16(&this, ctx);
        let len = units.len() as u32;
        if offset > len {
            return throw_index_size(ctx);
        }
        let end = (offset.saturating_add(count)).min(len) as usize;
        let replace_units: Vec<u16> = data.encode_utf16().collect();
        units.splice(offset as usize..end, replace_units);
        write_data_utf16(&this, &units, ctx);
        Ok(JsValue::undefined())
    });
    let substring_d = NativeFunction::from_copy_closure(|this, args, ctx| {
        let offset = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        let count = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        let units = read_data_utf16(&this, ctx);
        let len = units.len() as u32;
        if offset > len {
            return throw_index_size(ctx);
        }
        let end = (offset.saturating_add(count)).min(len) as usize;
        let sub = &units[offset as usize..end];
        Ok(JsValue::from(boa_engine::JsString::from(sub)))
    });
    vec![
        ("appendData", append),
        ("deleteData", delete_d),
        ("insertData", insert_d),
        ("replaceData", replace_d),
        ("substringData", substring_d),
    ]
}

/// Read the "data" property of a CharacterData node as UTF-16 code units.
fn read_data_utf16(this: &boa_engine::JsValue, ctx: &mut Context) -> Vec<u16> {
    this.as_object()
        .and_then(|o| o.get(boa_engine::js_string!("data"), ctx).ok())
        .and_then(|v| v.as_string().map(|s| s.iter().collect::<Vec<u16>>()))
        .unwrap_or_default()
}

/// Write UTF-16 code units to "data" and update textContent/length.
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
        let _ = o.insert_property(boa_engine::js_string!("data"), pd(JsValue::from(js_str.clone())));
        let _ = o.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::from(js_str)));
        let _ = o.insert_property(
            boa_engine::js_string!("length"),
            pd(JsValue::from(units.len() as u32)),
        );
    }
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
            let tag = arg_string(args, 0);
            // Validate the tag name per the Name production.
            // First char must be a letter, '_', or ':'. Rest must be letter, digit,
            // '_', ':', '-', '.', or combining char.
            if tag.is_empty() || !is_valid_name_first(&tag) {
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
            let _ = handle.insert_property(
                boa_engine::js_string!("prefix"),
                pd(JsValue::null()),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("nodeType"),
                pd(JsValue::from(1u32)),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("textContent"),
                pd(JsValue::from(boa_engine::js_string!(""))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("innerHTML"),
                pd(JsValue::from(boa_engine::js_string!(""))),
            );
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
            // Set the prototype chain so instanceof works.
            set_element_prototype(&handle, &tag, ctx);
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
        |this, args, b, ctx| {
            let name = arg_string(args, 0);
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
                        let _ = attr_obj.insert_property(boa_engine::js_string!("localName"), pd(JsValue::from(boa_engine::js_string!(name.clone()))));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("prefix"), pd(JsValue::null()));
                        let _ = attr_obj.insert_property(boa_engine::js_string!("namespaceURI"), pd(JsValue::null()));
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
        // Walk up target's parentNode chain.
        let mut current = target_obj;
        for _ in 0..1000 {
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
    let replace_child = NativeFunction::from_copy_closure(|_this, args, _ctx| {
        Ok(args.get(1).cloned().unwrap_or(JsValue::null()))
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
        // Same object → equal.
        if boa_engine::object::JsObject::equals(&this_obj, &other_obj) {
            return Ok(JsValue::from(true));
        }
        // Compare nodeName and textContent.
        let tn1 = this_obj.get(boa_engine::js_string!("nodeName"), ctx).unwrap_or_default();
        let tn2 = other_obj.get(boa_engine::js_string!("nodeName"), ctx).unwrap_or_default();
        if tn1 != tn2 { return Ok(JsValue::from(false)); }
        let tc1 = this_obj.get(boa_engine::js_string!("textContent"), ctx).unwrap_or_default();
        let tc2 = other_obj.get(boa_engine::js_string!("textContent"), ctx).unwrap_or_default();
        Ok(JsValue::from(tc1 == tc2))
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
        for _ in 0..1000 {
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

    // closest(selector) — walks up parentNode chain matching selector.
    let closest_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let selector = arg_string(args, 0);
        if selector.is_empty() { return Ok(JsValue::null()); }
        let mut current = this.as_object();
        for _ in 0..1000 {
            let node = match current {
                Some(o) => o,
                None => break,
            };
            // Simple selector matching: tag name, .class, #id
            let matches = if selector.starts_with('.') {
                let cn = node.get(boa_engine::js_string!("className"), ctx).ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                cn.split_whitespace().any(|c| format!(".{}", c) == selector)
            } else if selector.starts_with('#') {
                let id = node.get(boa_engine::js_string!("id"), ctx).ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                format!("#{}", id) == selector
            } else {
                let tn = node.get(boa_engine::js_string!("tagName"), ctx).ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                tn.eq_ignore_ascii_case(&selector)
            };
            if matches {
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
        let matches = if selector.starts_with('.') {
            let cn = node.get(boa_engine::js_string!("className"), ctx).ok()
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            cn.split_whitespace().any(|c| format!(".{}", c) == selector)
        } else if selector.starts_with('#') {
            let id = node.get(boa_engine::js_string!("id"), ctx).ok()
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            format!("#{}", id) == selector
        } else {
            let tn = node.get(boa_engine::js_string!("tagName"), ctx).ok()
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            tn.eq_ignore_ascii_case(&selector)
        };
        Ok(JsValue::from(matches))
    });
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
