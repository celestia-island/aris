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
    /// Scratch the addEventListener closure writes (node, global-name) pairs
    /// into; drained into JsRuntime.listeners after each script/listener run.
    new_listeners: Vec<(u32, String)>,
    /// Scratch for setTimeout/setInterval: (global_name, delay_ms, interval_ms).
    /// Drained into JsRuntime.timers after each script/listener run.
    new_timers: Vec<(String, u64, Option<u64>)>,
    /// Canvas 2D buffers keyed by a synthetic canvas id.
    canvases: HashMap<u32, crate::canvas::Canvas2D>,
    /// Next canvas id.
    next_canvas_id: u32,
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
        {
            let mut b = self.bridge.borrow_mut();
            b.ids = collect_ids(doc);
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
        let names = match self.listeners.get(&node_id) {
            Some(n) => n.clone(),
            None => return,
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

    /// Count red pixels across all canvas buffers (for testing).
    pub fn canvas_red_pixels(&self) -> usize {
        let b = self.bridge.borrow();
        b.canvases
            .values()
            .flat_map(|c| c.rgba.chunks_exact(4))
            .filter(|px| px[0] > 200 && px[1] < 50 && px[2] < 50)
            .count()
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
    use boa_engine::property::Attribute;

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

    // getElementById — reads the id snapshot from the bridge.
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

    // createElement
    let create_el = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let tag = arg_string(args, 0);
            // Special-case <canvas>: return a canvas handle with getContext.
            if tag == "canvas" {
                let mut bb = b.borrow_mut();
                let cid = bb.next_canvas_id;
                bb.next_canvas_id += 1;
                drop(bb);
                return Ok(make_canvas_handle(ctx, Gc::clone(b), cid)?.into());
            }
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

/// Build a JS canvas handle object: has `width`, `height`, and `getContext('2d')`.
/// The canvas pixels live in the bridge's `canvases` map.
fn make_canvas_handle(
    ctx: &mut Context,
    bridge: Gc<GcRefCell<Bridge>>,
    canvas_id: u32,
) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;
    use boa_engine::property::Attribute;

    // Allocate a default 300×150 canvas if not already present.
    {
        let mut b = bridge.borrow_mut();
        b.canvases
            .entry(canvas_id)
            .or_insert_with(|| crate::canvas::Canvas2D::new(300, 150));
    }

    let get_context = NativeFunction::from_copy_closure_with_captures(
        move |_this, args, b, ctx| {
            let kind = arg_string(args, 0);
            if kind != "2d" {
                return Ok(JsValue::null());
            }
            Ok(make_context_2d(ctx, Gc::clone(b), canvas_id)?.into())
        },
        Gc::clone(&bridge),
    );

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

/// Build a JS CanvasRenderingContext2D object bound to canvas_id's buffer.
fn make_context_2d(
    ctx: &mut Context,
    bridge: Gc<GcRefCell<Bridge>>,
    canvas_id: u32,
) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;
    

    // fillRect(x, y, w, h)
    let fill_rect = NativeFunction::from_copy_closure_with_captures(
        move |_this, args, b, _ctx| {
            let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
            let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
            let w = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0);
            let h = args.get(3).and_then(|v| v.as_number()).unwrap_or(0.0);
            if let Some(canvas) = b.borrow_mut().canvases.get_mut(&canvas_id) {
                canvas.fill_rect(x, y, w, h);
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );

    // clearRect(x, y, w, h)
    let clear_rect = NativeFunction::from_copy_closure_with_captures(
        move |_this, args, b, _ctx| {
            let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
            let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
            let w = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0);
            let h = args.get(3).and_then(|v| v.as_number()).unwrap_or(0.0);
            if let Some(canvas) = b.borrow_mut().canvases.get_mut(&canvas_id) {
                canvas.clear_rect(x, y, w, h);
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );

    // fillStyle getter/setter
    let fs_getter = NativeFunction::from_copy_closure_with_captures(
        move |_this, _args, b, _ctx| {
            let color = b
                .borrow()
                .canvases
                .get(&canvas_id)
                .map(|c| format!("#{:02x}{:02x}{:02x}", c.fill[0], c.fill[1], c.fill[2]))
                .unwrap_or_default();
            Ok(JsValue::from(boa_engine::js_string!(color)))
        },
        Gc::clone(&bridge),
    );
    let fs_setter = NativeFunction::from_copy_closure_with_captures(
        move |_this, args, b, _ctx| {
            let color = arg_string(args, 0);
            if let Some(canvas) = b.borrow_mut().canvases.get_mut(&canvas_id) {
                canvas.set_fill_style(&color);
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );

    let obj = ObjectInitializer::new(ctx)
        .function(fill_rect, boa_engine::js_string!("fillRect"), 4)
        .function(clear_rect, boa_engine::js_string!("clearRect"), 4)
        .build();
    // fillStyle as a property with getter/setter (accessor needs JsFunction).
    let fs_get_fn = fs_getter.to_js_function(ctx.realm());
    let fs_set_fn = fs_setter.to_js_function(ctx.realm());
    let _ = obj.define_property_or_throw(
        boa_engine::js_string!("fillStyle"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(fs_get_fn)
            .set(fs_set_fn)
            .enumerable(true)
            .configurable(true)
            .build(),
        ctx,
    );
    Ok(obj)
}

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

    // getContext('2d') — works on any element handle; creates a canvas buffer
    // keyed by the node id (for page-level <canvas>) or a synthetic id.
    let get_ctx = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let kind = arg_string(args, 0);
            if kind != "2d" {
                return Ok(JsValue::null());
            }
            // Use the node id as the canvas key (page-level <canvas>). For
            // created elements (pending), use the pending id offset by 1M.
            let cid = read_handle_id(_this)
                .or_else(|| read_pending(_this).map(|p| p + 1_000_000))
                .unwrap_or(0);
            {
                let mut bb = b.borrow_mut();
                bb.canvases
                    .entry(cid)
                    .or_insert_with(|| crate::canvas::Canvas2D::new(300, 150));
            }
            Ok(make_context_2d(ctx, Gc::clone(b), cid)?.into())
        },
        Gc::clone(&bridge),
    );
    init.function(get_ctx, boa_engine::js_string!("getContext"), 1);

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

    // setHTML(html) — sets innerHTML. Tags are stripped to plain text (a
    // full re-parse would need the html parser provider; this covers the
    // common "inject some text" case).
    let set_html = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let raw = arg_string(args, 0);
            // Strip HTML tags crudely → plain text.
            let text = strip_tags(&raw);
            let handle_id = read_handle_id(this);
            let pid = read_pending(this);
            if let Some(pid) = pid {
                if let Some(e) = b.borrow_mut().pending.get_mut(&pid) {
                    e.1 = text;
                }
            } else if let Some(nid) = handle_id {
                b.borrow_mut().ops.push(Op::SetText {
                    node_id: nid,
                    value: text,
                });
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(set_html, boa_engine::js_string!("setHTML"), 1);

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

    // addEventListener(type, handler): only 'click' is honored. The handler is
    // stashed as raw source (we can't easily persist a JsFunction); we store
    // (node_id, handler_src) in the bridge for harvest.
    let add_listener = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, _ctx| {
            let kind = arg_string(args, 0);
            if kind != "click" {
                return Ok(JsValue::undefined());
            }
            let handle_id = read_handle_id(this);
            // The handler source: capture the function expression text. We can't
            // get that back from a JsValue, so we expect the script to pass a
            // function; we stash it as a global via a round-trip.
            if let (Some(nid), Some(handler)) = (handle_id, args.get(1)) {
                // Serialize the handler object to a global so we can re-call it
                // later. We store the JsObject itself by assigning it to a
                // unique global name.
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
