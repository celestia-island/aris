//! Persistent Boa JS runtime for a document, supporting `addEventListener`.
//!
//! The runtime owns a single Boa [`Context`] that lives for the document's
//! lifetime, so listeners registered during `<script>` execution (and element
//! handles from `getElementById`) survive to be fired on click.
//!
//! Listener handlers are stashed as named globals on the context
//! (`__aris_listener_<node>_<type>_<seq>`), which keeps them GC-rooted without
//! us holding raw `JsFunction`s across calls. `JsRuntime::listeners` maps node
//! id → its registrations (event type + global name), and the bridge's
//! `listener_registry` holds (node, type, name, handler) so removal can match
//! by handler identity.
//!
//! Scope: `addEventListener('click', fn)`, `getElementById`, `createElement`,
//! `appendChild`, `setAttribute`, `textContent` — the same bridge operations as
//! the retired one-shot bridge, but persistent so `<script>`-registered
//! listeners survive to fire on click.

#![cfg(feature = "js")]

use std::collections::HashMap;

use boa_engine::property::Attribute;

// Thread-local canvas buffer map: Boa closures can't capture non-Trace types,
// so canvas buffers are accessed via this thread_local. The render loop is
// single-threaded, so this is safe.
thread_local! {
    pub(crate) static CANVASES: std::cell::RefCell<HashMap<u64, crate::canvas::Canvas2D>> =
        std::cell::RefCell::new(HashMap::new());
    /// DOM node id → canvas allocation id. Buffers live in CANVASES keyed by
    /// the allocation id (stable for the buffer's lifetime, so captured
    /// 2D-context closures keep writing to the same slot); this map is the
    /// only place that knows which DOM node a buffer composites into.
    /// Inline canvases register at getContext time; created canvases
    /// register when their AttachCanvas op is replayed (L940 leftover ②).
    pub(crate) static CANVAS_HOME: std::cell::RefCell<HashMap<u64, u64>> =
        std::cell::RefCell::new(HashMap::new());
    static NEXT_CANVAS_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Resolve the allocation id a DOM node's canvas buffer lives under.
pub(crate) fn canvas_home_for(node_id: u64) -> Option<u64> {
    CANVAS_HOME.with(|home| home.borrow().get(&node_id).copied())
}

/// Allocate a new canvas id and create the buffer.
fn alloc_canvas(width: u32, height: u32) -> u64 {
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
use boa_engine::object::builtins::JsArray;

/// `JsArray::new` is fallible in boa 0.22 (allocation can fail under a
/// configured runtime limit). aris runs without limits, so a failure here is
/// OOM and panics with a clear message.
fn new_array(ctx: &mut Context) -> JsArray {
    JsArray::new(ctx).expect("JsArray allocation failed")
}
use boa_engine::{Context, JsObject, JsResult, JsValue, NativeFunction, Source};
use boa_gc::{Finalize, Gc, GcRefCell, Trace};

/// One `addEventListener` registration in the fire_click registry.
#[derive(Clone)]
struct ListenerEntry {
    /// Event type this handler listens for (e.g. "click").
    event_type: String,
    /// Global name holding the callback (GC root).
    name: String,
    /// Node this listener is attached to — the dispatch loop uses it both as
    /// the lookup key and as the `this` for the call, so an ancestor's
    /// handler sees its own node, not the click target (DOM currentTarget).
    node_id: u64,
}

/// A persistent JS runtime bound to one document.
pub struct JsRuntime {
    ctx: Context,
    /// node id → its registrations, in registration order. Several handlers
    /// may listen to the same (node, type) (L940 registry redesign).
    listeners: HashMap<u64, Vec<ListenerEntry>>,
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
    ids: HashMap<String, u64>,
    ops: Vec<Op>,
    next_pending: u32,
    pending: HashMap<u32, (String, String, Vec<(String, String)>)>,
    query_by_tag: HashMap<String, u64>,
    query_by_class: HashMap<String, u64>,
    query_by_id: HashMap<String, u64>,
    /// Snapshot of node properties: node_id → (tag_name, text_content, id_attr, class_attr, node_type)
    node_props: HashMap<u64, NodePropSnapshot>,
    /// Listener actions in SCRIPT ORDER (add / remove), so a same-window
    /// add→remove or remove→add collapses to the final script state instead
    /// of an arbitrary pass order (L940 leftover ②-cycle, R3/R4 findings).
    listener_actions: Vec<ListenerAction>,
    /// Monotonic counter for AD-HOC registrations only (inline `onclick` and
    /// other one-off evaluation). Page-script registrations are numbered by
    /// `listener_slots` instead; this counter keeps the two ranges disjoint.
    next_listener_reg: u32,
    /// Per-(node, type) slot index for THIS run. A replay re-uses the same
    /// slot numbers, which is what makes a fixed-order page idempotent; a
    /// changed pattern is reconciled after the run (see `reconcile_slots`).
    listener_slots: std::collections::HashMap<(u64, String), u32>,
    /// How many slots each (node, type) held at the END of the previous run,
    /// so a re-run that registers FEWER listeners can drop the stale tail
    /// instead of leaving old handlers firing.
    listener_slots_prev: std::collections::HashMap<(u64, String), u32>,
    /// True only while a PAGE script run is executing: ad-hoc evaluation
    /// (inline onclick) must not consume the page's registration slots.
    slots_active: bool,
    /// Authoritative registration table (node, type, name, handler) so
    /// removeEventListener can match by handler identity and drop exactly
    /// one registration.
    listener_registry: Vec<(u64, String, String, JsValue)>,
    new_timers: Vec<(String, u64, Option<u64>)>,
}

/// One listener registration/removal in script order.
#[derive(Clone, Trace, Finalize)]
enum ListenerAction {
    Add {
        node_id: u64,
        name: String,
        event_type: String,
        handler: JsValue,
    },
    Remove {
        node_id: u64,
        name: String,
    },
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
    child_ids: Vec<u64>,
    /// The subset of `child_ids` that are ELEMENTS, in order. `children` /
    /// `childElementCount` must count only these — counting the total would
    /// report text nodes as elements (round-2 S2).
    element_child_ids: Vec<u64>,
    /// Parent node ID (0 = no parent / root).
    parent_id: u64,
}

#[derive(Clone, Debug, Trace, Finalize)]
enum Op {
    SetText {
        node_id: u64,
        value: String,
    },
    SetAttr {
        node_id: u64,
        name: String,
        value: String,
    },
    AppendChild {
        parent_id: u64,
        pending_id: u32,
    },
    /// A created canvas handle was appended: create a real `<canvas>` node
    /// under `parent_id` and home the existing buffer `alloc_id` to it.
    AttachCanvas {
        parent_id: u64,
        alloc_id: u64,
    },
    /// `parent.removeChild(child)` on real tree nodes. Without this the call
    /// only updated the JS `_children` mirror and the parsed tree kept the
    /// child — so even a rebind could not see a removal that never happened
    /// (L940, PR #85 R7).
    RemoveChild {
        parent_id: u64,
        child_id: u64,
    },
    /// `parent.insertBefore(child, ref)` on real tree nodes. Same family as
    /// RemoveChild/AppendChild: without an op the parsed tree kept the old
    /// shape and a remove+insert "move" DESTROYED the node instead of
    /// relocating it (L940, PR #86 R2).
    InsertBefore {
        parent_id: u64,
        /// A PENDING id for a freshly created node, or the existing node's id.
        child_id: u64,
        /// Id of the node to insert before; 0 = append at the end.
        anchor_id: u64,
    },
    /// `ref.before(child)` / `ref.after(child)` on real tree nodes —
    /// sibling-positioning inserts whose target parent is derived from the
    /// ref's own parent at apply time (the JS closure cannot resolve it:
    /// the mirror parentNode is a bare stub for parsed elements).
    InsertAdjacent {
        ref_id: u64,
        /// The nodes to move, in DOM order. Inserted as ONE batch so the
        /// anchor cannot drift between them (round-6 S0: per-arg ops made
        /// mixed argument lists land on the wrong side of the ref).
        child_ids: Vec<u64>,
        /// true = insert AFTER the ref, false = BEFORE it.
        after: bool,
    },
    /// `ref.replaceWith(...children)` on real tree nodes — the batch lands
    /// where the ref was and the ref is DETACHED (remove_node, not drop: a
    /// replaced node stays re-insertable, matching #87's decision).
    ReplaceWith {
        ref_id: u64,
        child_ids: Vec<u64>,
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
        self.bind_and_run_with_url_mode(doc, script_src, url, false)
    }

    /// Run the PAGE's `<script>` blocks. `replay` marks a run whose
    /// registrations should be idempotent against the previous one (resize /
    /// zoom / reload re-execute the same scripts): slots are numbered per
    /// (node, type) and start from zero, so an unchanged page re-uses the
    /// same global names, and the bind then reconciles any slot the run did
    /// not re-declare. Prefer this over [`Self::bind_and_run_with_url`] for
    /// page scripts; that entry point registers in a separate ad-hoc slot
    /// range for one-off evaluation (an inline `onclick` must not consume or
    /// trim the page's own slots).
    pub fn bind_page_scripts(
        &mut self,
        doc: &mut BaseDocument,
        scripts: &[String],
        url: &str,
        replay: bool,
    ) {
        let combined = scripts.join("\n;\n");
        self.bind_and_run_with_url_mode(doc, &combined, url, replay);
    }

    fn bind_and_run_with_url_mode(
        &mut self,
        doc: &mut BaseDocument,
        script_src: &str,
        url: &str,
        replay: bool,
    ) {
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
            // Keep `next_pending` MONOTONIC across binds: resetting it to 0
            // re-issues pids that stale handles still hold, and their writes
            // would land on a DIFFERENT, freshly-created node (round-2 S2 —
            // a silent misroute, worse than the drop it replaced). Entries
            // from the previous run are cleared, so those stale writes find
            // no tuple and drop, exactly like master's dropped writes.
            b.pending.clear();
            // Actions belong to the ticket that recorded them; a later
            // re-registration must not be killed by a stale record
            // (R3 S1: cross-ticket remove→re-add left the listener dead).
            b.listener_actions.clear();
            b.slots_active = replay;
            if replay {
                // A page run starts a fresh slot generation; the previous
                // run's shape lives in listener_slots_prev for reconciliation.
                b.listener_slots.clear();
            }
        }
        // Deliberately NOT clearing self.listeners here: this path re-runs
        // for every onclick-attribute click (fire_click → bind_and_run), and
        // clearing would permanently unregister every addEventListener
        // listener on the page. Harvest dedupes on the deterministic names,
        // so the re-run's re-registrations collapse instead of stacking.
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
        if replay {
            self.reconcile_slots();
        }
        // The page run is over: further registrations are ad-hoc (inline
        // onclick) and must not consume page slots. Leaving the flag set
        // relied on every entry point re-assigning it — made explicit here.
        self.bridge.borrow_mut().slots_active = false;
        self.harvest_timers();
    }

    /// Fire any click listeners attached to `node_id`. Each listener function
    /// is invoked with the current element handle as `this`. Recorded ops are
    /// applied after all listeners run.
    pub fn fire_click(&mut self, doc: &mut BaseDocument, node_id: u64) {
        self.apply_ops(doc);

        // 1. Inline onclick attribute: evaluate it in the runtime.
        let onclick_src = doc
            .get_node(blitz_traits::NodeId::from_u64(node_id))
            .and_then(|n| {
                n.attr(blitz_dom::local_name!("onclick"))
                    .map(|s| s.to_string())
            });
        if let Some(src) = onclick_src {
            // DOM: an inline handler's `this` is the element. Evaluate it as a
            // function invoked with the element handle, not as bare global
            // script (where `this` would be the global object).
            let mut handle =
                make_element_handle(&mut self.ctx, Gc::clone(&self.bridge), node_id, None)
                    .unwrap_or_else(|_| {
                        boa_engine::object::JsObject::with_object_proto(self.ctx.intrinsics())
                    });
            if let Some(snap) = self.bridge.borrow().node_props.get(&node_id).cloned() {
                populate_props(&handle, &snap, &mut self.ctx);
            }
            let wrapped = format!("(function(){{ {src}\n}}).call(__aris_this);");
            let _ = self.ctx.global_object().insert_property(
                boa_engine::js_string!("__aris_this"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(handle)
                    .writable(true)
                    .enumerable(false)
                    .configurable(true)
                    .build(),
            );
            self.bind_and_run(doc, &wrapped);
        }

        // 2. Registered addEventListener listeners for "click", target first,
        // then bubbling up the ancestor chain to `document` — a listener on
        // the document only ever saw `dispatchEvent` before, so
        // `document.addEventListener('click', fn)` never fired on a real
        // click (L940 follow-up).
        //
        // Phase note: only the BUBBLE phase exists here. A handler registered
        // with `capture: true` is dispatched at its bubble position (target
        // first, then outward), not before the target — DOM would run it in
        // the capture phase. `capture` is not read anywhere in this runtime;
        // implementing real capture/`stopPropagation` is a separate item
        // (L940), not a regression introduced by this walk.
        let mut entries: Vec<ListenerEntry> = Vec::new();
        {
            let mut cur = Some(blitz_traits::NodeId::from_u64(node_id));
            while let Some(id) = cur {
                if let Some(list) = self.listeners.get(&id.as_u64()) {
                    entries.extend(
                        list.iter()
                            .filter(|e| e.event_type.is_empty() || e.event_type == "click")
                            .cloned(),
                    );
                }
                cur = doc.get_node(id).and_then(|n| n.parent);
            }
        }
        if entries.is_empty() {
            // Even with no node-level listener the document may have one.
            self.dispatch_document_click();
            self.apply_ops(doc);
            self.harvest_listeners();
            self.harvest_timers();
            return;
        }
        for entry in entries {
            let name = entry.name.clone();
            // For a FUNCTION listener, `this` is the node the listener is
            // attached to (DOM currentTarget), not the click target. A
            // `handleEvent` OBJECT instead gets `this` = the listener object
            // (DOM), so it receives no node handle at all — and this runtime
            // passes no event argument either, so such a listener cannot see
            // which element fired (L940: event-object model follow-up).
            let this_node = entry.node_id;
            // Look up the stashed callback and call it with a fresh element
            // handle as `this` (functions) or with the listener object as
            // `this` (handleEvent objects).
            let mut this = match make_element_handle(
                &mut self.ctx,
                Gc::clone(&self.bridge),
                this_node,
                None,
            ) {
                Ok(o) => o,
                Err(_) => continue,
            };
            // Populate the handle's properties from the node snapshot: a bare
            // make_element_handle leaves `id` / `tagName` / `textContent`
            // empty, so a handler branching on `this.id` silently saw "".
            if let Some(snap) = self.bridge.borrow().node_props.get(&this_node).cloned() {
                populate_props(&this, &snap, &mut self.ctx);
            }
            let key = boa_engine::js_string!(name.clone());
            let func_val = match self.ctx.global_object().get(key.clone(), &mut self.ctx) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(func) = func_val.as_object() else {
                continue;
            };
            let this_val = JsValue::from(this);
            // DOM: an event listener is either a function or an object with a
            // `handleEvent` method. The callable-only guard silently dropped
            // the latter (L940 follow-up).
            if func.is_callable() {
                let _ = func.call(&this_val, &[], &mut self.ctx);
            // The `is_callable()` check is defence in depth: invoking a
            // non-callable would throw a TypeError that the `let _ =` below
            // swallows, so it is not page-observable on its own.
            } else if let Ok(h) = func.get(boa_engine::js_string!("handleEvent"), &mut self.ctx)
                && let Some(hf) = h.as_object()
                && hf.is_callable()
            {
                // DOM: inside `handleEvent`, `this` is the listener object.
                let listener = JsValue::from(func.clone());
                let _ = hf.call(&listener, &[], &mut self.ctx);
            }
        }
        // 3. Document-level listeners: they live on the document object's
        // `_js_listeners` map (installed by the document's own
        // addEventListener), not in this registry, so they are dispatched
        // separately at the end of the bubble path.
        self.dispatch_document_click();
        self.apply_ops(doc);
        self.harvest_listeners();
        self.harvest_timers();
    }

    /// Run the document's `click` listeners (the `_js_listeners` entries on
    /// the document object). Kept separate from the node walk because the
    /// document is not a blitz node: `fire_click` starts from a node id and
    /// the walk steps through `node.parent`, so the document object is never
    /// reachable as a walk step (it has no `_arisId` and no parent).
    fn dispatch_document_click(&mut self) {
        let Ok(doc_val) = self
            .ctx
            .global_object()
            .get(boa_engine::js_string!("document"), &mut self.ctx)
        else {
            return;
        };
        let Some(doc_obj) = doc_val.as_object() else {
            return;
        };
        let Ok(lmap) = doc_obj.get(boa_engine::js_string!("_js_listeners"), &mut self.ctx) else {
            return;
        };
        let Some(lm) = lmap.as_object() else {
            return;
        };
        let Ok(arr) = lm.get(boa_engine::js_string!("click"), &mut self.ctx) else {
            return;
        };
        let Some(arr) = arr.as_object() else {
            return;
        };
        let len = arr
            .get(boa_engine::js_string!("length"), &mut self.ctx)
            .ok()
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as u32;
        let mut handlers = Vec::new();
        for i in 0..len {
            if let Ok(entry) = arr.get(i, &mut self.ctx)
                && let Some(obj) = entry.as_object()
                && let Ok(cb) = obj.get(boa_engine::js_string!("callback"), &mut self.ctx)
            {
                handlers.push(cb);
            }
        }
        // DOM: `this` in a document listener is the document object. Passing
        // null would be coerced to the global object by sloppy-mode functions
        // (a page-observable wrong-object bug), so pass the document itself.
        let this_val = self
            .ctx
            .global_object()
            .get(boa_engine::js_string!("document"), &mut self.ctx)
            .unwrap_or(JsValue::null());
        for cb in handlers {
            let Some(f) = cb.as_object() else { continue };
            // DOM: function or object with `handleEvent`.
            if f.is_callable() {
                let _ = f.call(&this_val, &[], &mut self.ctx);
            } else if let Ok(h) = f.get(boa_engine::js_string!("handleEvent"), &mut self.ctx)
                && let Some(hf) = h.as_object()
                && hf.is_callable()
            {
                // DOM: inside `handleEvent`, `this` is the listener object
                // (not the document). No event argument is passed yet, so the
                // listener cannot read the event/currentTarget — see L940.
                let listener = JsValue::from(f.clone());
                let _ = hf.call(&listener, &[], &mut self.ctx);
            }
        }
    }

    /// Move pending ops from the bridge into the document.
    fn apply_ops(&mut self, doc: &mut BaseDocument) {
        // DRAIN, not clone: every call consumes the ops recorded so far.
        // Cloning here made every apply_ops REPLAY the whole accumulated log,
        // and `Op::AppendChild` mints a fresh node per replay — one inline
        // onclick append landed two nodes and each further click added four
        // more, an unbounded leak (L940 S1). Callers that re-apply on purpose
        // do not exist: every push site is followed by an apply within the
        // same script run.
        let ops: Vec<Op> = std::mem::take(&mut self.bridge.borrow_mut().ops);
        let pending: HashMap<u32, (String, String, Vec<(String, String)>)> =
            { self.bridge.borrow().pending.clone() };
        for op in ops {
            apply_op(doc, op, &pending);
        }
    }

    /// Drop slot entries the current run did not re-declare.
    ///
    /// Slots are numbered per (node, type). A replay that registers the SAME
    /// number reuses every name and the harvest collapses it; a replay that
    /// registers FEWER leaves the previous run's higher slots behind, and
    /// those old handlers would keep firing alongside the new ones. This
    /// trims that stale tail and records the shape for the next run.
    fn reconcile_slots(&mut self) {
        let (cur, prev) = {
            let bb = self.bridge.borrow();
            (bb.listener_slots.clone(), bb.listener_slots_prev.clone())
        };
        for ((node_id, type_name), prev_n) in prev.iter() {
            let cur_n = cur
                .get(&(*node_id, type_name.clone()))
                .copied()
                .unwrap_or(0);
            if cur_n >= *prev_n {
                continue;
            }
            // Slots cur_n..prev_n of the previous run are stale.
            if let Some(list) = self.listeners.get_mut(node_id) {
                let mut kept: Vec<ListenerEntry> = Vec::with_capacity(list.len());
                for e in list.iter() {
                    let stale = e.event_type == *type_name
                        && match slot_of(&e.name) {
                            Some(i) => i >= cur_n && i < *prev_n,
                            None => false,
                        };
                    if !stale {
                        kept.push(e.clone());
                    }
                }
                *list = kept;
                if list.is_empty() {
                    self.listeners.remove(node_id);
                }
            }
            let removed: Vec<String> = {
                let bb = self.bridge.borrow();
                bb.listener_registry
                    .iter()
                    .filter(|(nid, ty, name, _)| {
                        nid == node_id
                            && ty == type_name
                            && match slot_of(name) {
                                Some(i) => i >= cur_n && i < *prev_n,
                                None => false,
                            }
                    })
                    .map(|(_, _, name, _)| name.clone())
                    .collect()
            };
            for name in removed {
                let global = self.ctx.global_object();
                let _ = global
                    .delete_property_or_throw(boa_engine::js_string!(name.clone()), &mut self.ctx);
                let mut bb = self.bridge.borrow_mut();
                bb.listener_registry.retain(|(_, _, n, _)| *n != name);
            }
        }
        let mut bb = self.bridge.borrow_mut();
        bb.listener_slots_prev = cur;
        bb.listener_slots.clear();
    }

    /// Pull newly-registered listeners out of the bridge into self.listeners.
    /// The handler objects were already stashed as named globals during
    /// addEventListener; here we just record node → name.
    fn harvest_listeners(&mut self) {
        let actions: Vec<ListenerAction> =
            std::mem::take(&mut self.bridge.borrow_mut().listener_actions);
        for action in actions {
            // ListenerAction derives Trace/Finalize (Drop), so match by
            // reference and clone the fields we keep.
            match &action {
                ListenerAction::Add {
                    node_id,
                    name,
                    event_type,
                    handler,
                } => {
                    let node_id = *node_id;
                    let name = name.clone();
                    let event_type = event_type.clone();
                    let handler = handler.clone();
                    let handler_for_stash = handler.clone();
                    // Re-stash when an earlier removal in this window deleted
                    // the global (remove → re-add in one ticket).
                    let global = self.ctx.global_object();
                    let present = global
                        .get(boa_engine::js_string!(name.clone()), &mut self.ctx)
                        .ok()
                        .map(|v| v.is_object() || v.is_callable())
                        .unwrap_or(false);
                    if !present {
                        let _ = global.insert_property(
                            boa_engine::js_string!(name.clone()),
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(handler_for_stash)
                                .writable(true)
                                .enumerable(false)
                                .configurable(true)
                                .build(),
                        );
                    }
                    // Deterministic name: re-registering dedupes instead of
                    // stacking duplicate firings.
                    let entry = self.listeners.entry(node_id).or_default();
                    if !entry.iter().any(|e| e.name == name) {
                        entry.push(ListenerEntry {
                            event_type: event_type.clone(),
                            name: name.clone(),
                            node_id,
                        });
                    }
                    {
                        let mut bb = self.bridge.borrow_mut();
                        // A replayed script re-registers the same slot under
                        // the same name — replace the row instead of stacking
                        // a duplicate (registry half of the idempotence fix).
                        let row = (node_id, event_type, name, handler);
                        match bb
                            .listener_registry
                            .iter()
                            .position(|(_, _, n, _)| *n == row.2)
                        {
                            Some(i) => bb.listener_registry[i] = row,
                            None => bb.listener_registry.push(row),
                        }
                    }
                }
                ListenerAction::Remove { node_id, name } => {
                    let node_id = *node_id;
                    let name: String = name.clone();
                    if let Some(entry) = self.listeners.get_mut(&node_id) {
                        entry.retain(|e| e.name != name);
                    }
                    {
                        let mut bb = self.bridge.borrow_mut();
                        bb.listener_registry.retain(|(_, _, n, _)| *n != name);
                    }
                    let global = self.ctx.global_object();
                    let _ = global
                        .delete_property_or_throw(boa_engine::js_string!(name), &mut self.ctx);
                }
            }
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
            // (A second `harvest_timers()` used to sit here — dead: the first
            // call `std::mem::take`s `new_timers`, so the second always took
            // an empty vec. Found by PR #78's round-2 review; removed in
            // PR #82.)
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
    fn populate_doc_elements(&mut self, snapshot: &HashMap<u64, NodePropSnapshot>) {
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };

        // Find html, body, head node IDs.
        let mut html_id: Option<u64> = None;
        let mut head_id: Option<u64> = None;
        let mut body_id: Option<u64> = None;
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
        let mk = |nid: u64, snap: &NodePropSnapshot, ctx: &mut Context| {
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

    /// Number of children recorded in the node-prop snapshot for `node_id`
    /// (for testing: the same child_ids that feed the `childNodes` property).
    pub fn child_nodes_count(&self, node_id: u64) -> Option<usize> {
        self.bridge
            .borrow()
            .node_props
            .get(&node_id)
            .map(|s| s.child_ids.len())
    }
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Does this node id still resolve in the document tree?
///
/// A handle held across a child removal keeps its `_arisId`, but
/// `remove_and_drop_node` FREES that slab slot — so an op carrying the stale
/// id would index `doc.nodes[id]` inside blitz and panic ("invalid SlotMap
/// key used"). Master never hit this because it never removed children;
/// replacing `textContent` does. Dropping such ops matches the tree's own
/// `get_node` semantics (absent ⇒ no-op) instead of aborting the process.
fn node_is_live(doc: &BaseDocument, node_id: u64) -> bool {
    doc.get_node(blitz_traits::NodeId::from_u64(node_id))
        .is_some()
}

fn apply_op(
    doc: &mut BaseDocument,
    op: Op,
    pending: &HashMap<u32, (String, String, Vec<(String, String)>)>,
) {
    // Every op carries a node id that a page handle captured earlier; that
    // node may have been removed since (textContent replace, removeChild).
    let target = match &op {
        Op::SetText { node_id, .. }
        | Op::SetAttr { node_id, .. }
        | Op::AppendChild {
            parent_id: node_id, ..
        }
        | Op::AttachCanvas {
            parent_id: node_id, ..
        }
        // RemoveChild's guard target is the CHILD (it is the one that must
        // still resolve; the parent is not indexed by remove_and_drop_node).
        | Op::RemoveChild {
            child_id: node_id, ..
        }
        | Op::InsertBefore {
            parent_id: node_id, ..
        }
        | Op::InsertAdjacent {
            ref_id: node_id, ..
        }
        | Op::ReplaceWith {
            ref_id: node_id, ..
        } => *node_id,
    };
    if !node_is_live(doc, target) {
        return;
    }
    match &op {
        Op::SetText { node_id, value } => set_text_content(doc, *node_id, value),
        Op::SetAttr {
            node_id,
            name,
            value,
        } => set_attribute(doc, *node_id, name, value),
        Op::AppendChild {
            parent_id,
            pending_id,
        } => {
            if let Some((tag, text, attrs)) = pending.get(pending_id) {
                create_and_append(doc, *parent_id, tag, text, attrs);
            }
        }
        Op::AttachCanvas {
            parent_id,
            alloc_id,
        } => attach_canvas_node(doc, *parent_id, *alloc_id),
        Op::RemoveChild {
            parent_id: _,
            child_id,
        } => {
            // The child must leave the TREE, not just the JS mirror.
            // DETACH, not drop: the DOM lets a removed node be re-inserted
            // later (the remove→insert "move" idiom). `remove_and_drop_node`
            // would FREE the slab slot, so the node could never come back and
            // a page-held handle would go dangling. `remove_node` only
            // detaches; on an already-detached node `parent.take()` yields
            // None and the call is a safe no-op.
            let child = blitz_traits::NodeId::from_u64(*child_id);
            doc.mutate().remove_node(child);
        }
        Op::InsertBefore {
            parent_id,
            child_id,
            anchor_id,
        } => {
            // The guard above covers the CHILD; "anchor 0" legitimately means
            // append-at-end (a null anchor and a PENDING anchor both arrive as
            // 0 — `read_handle_id` filters the zero id of createElement
            // handles). Every id blitz indexes directly must resolve, or the
            // process aborts (#84's lesson applied to this op's ids).
            if !node_is_live(doc, *child_id) {
                return;
            }
            // Self-insert and insert/appending a node into its OWN subtree
            // build a parent cycle (blitz accepts it silently; the tree then
            // aborts bind_and_run with a stack overflow). Walk the target
            // parent's ancestor chain — for the insert branch the parentage
            // check below pins the anchor's parent to this same parent, so
            // one walk covers both.
            if parent_id == child_id {
                return;
            }
            {
                let mut cur = blitz_traits::NodeId::from_u64(*parent_id);
                loop {
                    if cur.as_u64() == *child_id {
                        return;
                    }
                    match doc.get_node(cur).and_then(|n| n.parent) {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
            }
            let with_anchor = *anchor_id != 0;
            if with_anchor && !node_is_live(doc, *anchor_id) {
                return;
            }
            let parent = blitz_traits::NodeId::from_u64(*parent_id);
            let nid = blitz_traits::NodeId::from_u64(*child_id);
            if with_anchor {
                // The anchor's REAL parent must be the requested one; using
                // the anchor's parent instead silently moved nodes into a
                // different subtree (round-2 S2).
                let anchor = blitz_traits::NodeId::from_u64(*anchor_id);
                if doc.get_node(anchor).and_then(|n| n.parent) != Some(parent) {
                    return;
                }
                // DOM no-ops: inserting a node before ITSELF, or before its
                // own next sibling (already in the requested position), must
                // not reach blitz — `insert_nodes_before` detaches the child
                // first and then `index_of_child(anchor)` panics on the
                // severed anchor (PR #89 round-2 S0: `prepend(firstChild)`).
                if child_id == anchor_id {
                    return;
                }
                let already_in_position = doc
                    .get_node(anchor)
                    .and_then(|n| n.parent)
                    .and_then(|pid| doc.get_node(pid))
                    .map(|p| match p.children.iter().position(|&c| c == anchor) {
                        Some(i) if i > 0 => p.children[i - 1] == nid,
                        _ => false,
                    })
                    .unwrap_or(false);
                if already_in_position {
                    return;
                }
                doc.mutate().insert_nodes_before(anchor, &[nid]);
            } else {
                doc.mutate().append_children(parent, &[nid]);
            }
        }
        Op::InsertAdjacent {
            ref_id,
            child_ids,
            after,
        } => {
            // The guard above covers the REF (blitz indexes it first). Each
            // CHILD needs its own liveness check — and, crucially, the whole
            // list is inserted as ONE batch so the anchor cannot drift
            // between arguments (round-6 S0).
            if child_ids.is_empty() || child_ids.contains(ref_id) {
                return;
            }
            // Dedup preserving order: two handles can resolve to the SAME
            // NodeId (`a.after(x, x2)`), and inserting a node twice into one
            // parent duplicates it (round-8 S1).
            let mut live: Vec<blitz_traits::NodeId> = Vec::new();
            for id in child_ids {
                if *id == *ref_id {
                    return; // batch containing the ref: reject wholesale
                }
                let nid = blitz_traits::NodeId::from_u64(*id);
                if node_is_live(doc, *id) && !live.contains(&nid) {
                    live.push(nid);
                }
            }
            if live.is_empty() {
                return;
            }
            let ref_node = blitz_traits::NodeId::from_u64(*ref_id);
            // The effective target parent is the REF's parent. A cycle
            // exists iff one of the children is ancestor-or-self of it.
            let parent = match doc.get_node(ref_node).and_then(|n| n.parent) {
                Some(p) => p,
                None => return, // detached ref: nowhere to insert
            };
            for child in &live {
                let mut cur = parent;
                loop {
                    if cur == *child {
                        return;
                    }
                    match doc.get_node(cur).and_then(|n| n.parent) {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
            }
            // DOM no-ops: every child already sits in the requested
            // position. blitz detaches first and then
            // index_of_child(anchor).unwrap() panics on a severed anchor.
            // Positions + the sibling the batch must land before, read from
            // ONE immutable snapshot of the parent's children.
            let (ref_idx, next_after_ref) = match doc.get_node(parent) {
                Some(p) => match p.children.iter().position(|&c| c == ref_node) {
                    Some(i) => (i, p.children.get(i + 1).copied()),
                    None => return,
                },
                None => return,
            };
            let kids = match doc.get_node(parent) {
                Some(p) => p.children.clone(),
                None => return,
            };
            if *after {
                // No-op when the batch already occupies the slot right after
                // the ref, in order.
                let already = kids
                    .iter()
                    .skip(ref_idx + 1)
                    .take(live.len())
                    .copied()
                    .eq(live.iter().copied());
                if already {
                    return;
                }
            } else {
                // No-op when the batch already occupies the slots right
                // before the ref, in order.
                let start = ref_idx.saturating_sub(live.len());
                let already = kids
                    .iter()
                    .skip(start)
                    .take(live.len())
                    .copied()
                    .eq(live.iter().copied())
                    && start + live.len() == ref_idx;
                if already {
                    return;
                }
            }
            let mut mutator = doc.mutate();
            if *after {
                match next_after_ref {
                    Some(anchor) => {
                        // The anchor may itself be a batch member
                        // (`a.after(x, y)` with x already next to a).
                        // add_children_to_parent detaches batch members
                        // BEFORE computing the splice index, so the anchor
                        // must not be among the inserted ids — insert the
                        // rest before it instead (round-7 S0).
                        let to_insert: Vec<blitz_traits::NodeId> =
                            live.iter().copied().filter(|id| *id != anchor).collect();
                        if to_insert.is_empty() {
                            return;
                        }
                        mutator.insert_nodes_before(anchor, &to_insert);
                    }
                    None => mutator.append_children(parent, &live),
                }
            } else {
                mutator.insert_nodes_before(ref_node, &live);
            }
        }
        Op::ReplaceWith { ref_id, child_ids } => {
            // Guards per the hard step: blitz indexes the REF (parent_id
            // lookup + viable_sibling + replace_node_with's
            // insert_nodes_before + remove_node) and each CHILD (detach in
            // add_children_to_parent / detach_all).
            let ref_node = blitz_traits::NodeId::from_u64(*ref_id);
            if !node_is_live(doc, *ref_id) {
                return;
            }
            // Dedup preserving order; a batch member that IS the ref makes
            // it a DOM no-op (skip); an ancestor-of-parent member would
            // create a cycle — reject wholesale.
            let parent = match doc.get_node(ref_node).and_then(|n| n.parent) {
                Some(p) => p,
                None => return, // detached ref: replaceWith is a no-op
            };
            let mut live: Vec<blitz_traits::NodeId> = Vec::new();
            let mut rejected = false;
            for id in child_ids {
                if *id == *ref_id {
                    continue; // self-replace: that member contributes nothing
                }
                let nid = blitz_traits::NodeId::from_u64(*id);
                if !node_is_live(doc, *id) {
                    continue;
                }
                let mut cur = parent;
                loop {
                    if cur == nid {
                        rejected = true;
                        break;
                    }
                    match doc.get_node(cur).and_then(|n| n.parent) {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
                if rejected {
                    break;
                }
                if !live.contains(&nid) {
                    live.push(nid);
                }
            }
            if rejected {
                return;
            }
            let mut mutator = doc.mutate();
            mutator.replace_with_nodes(ref_node, &live);
        }
    }
}

/// Create a real `<canvas>` element under `parent_id` and home the already
/// allocated buffer `alloc_id` to the new node id, so the compositor finds
/// it (createElement handles carry the allocation id, not a node id).
fn attach_canvas_node(doc: &mut BaseDocument, parent_id: u64, alloc_id: u64) {
    use blitz_dom::QualName;
    let parent = blitz_traits::NodeId::from_u64(parent_id);
    // Mutator session: same materialization semantics as create_and_append
    // (stylo data + IS_IN_DOCUMENT + damage) so the created canvas actually
    // participates in layout instead of staying a 0×0 uncomposited stub.
    let mut mutator = doc.mutate();
    let new_id = mutator.create_element(
        QualName::new(None, blitz_dom::ns!(html), "canvas".into()),
        Vec::new(),
    );
    mutator.append_children(parent, &[new_id]);
    CANVAS_HOME.with(|home| {
        home.borrow_mut().insert(new_id.as_u64(), alloc_id);
    });
}

// ── DOM helpers ────────────────────────────────────────────

fn collect_ids(doc: &BaseDocument) -> HashMap<String, u64> {
    let mut r = HashMap::new();
    for (id, node) in doc.tree().iter() {
        if let Some(id_attr) = node.attr(local_name!("id")) {
            r.insert(id_attr.to_string(), id.as_u64());
        }
    }
    r
}

/// Snapshot all node properties into a map for JS-side access.
fn collect_node_props(doc: &BaseDocument) -> HashMap<u64, NodePropSnapshot> {
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
        let child_ids: Vec<u64> = node.children.iter().map(|&c| c.as_u64()).collect();
        let element_child_ids: Vec<u64> = node
            .children
            .iter()
            .copied()
            .filter(|&c| {
                doc.get_node(c)
                    .map(|cn| cn.element_data().is_some())
                    .unwrap_or(false)
            })
            .map(|c| c.as_u64())
            .collect();
        let parent_id = node.parent.map(|p| p.as_u64()).unwrap_or(0);
        r.insert(
            id.as_u64(),
            NodePropSnapshot {
                tag_name: tag,
                text_content: text,
                id: id_attr,
                class_name: class,
                node_type,
                attrs,
                child_ids,
                element_child_ids,
                parent_id,
            },
        );
    }
    r
}

fn collect_query_index(
    doc: &BaseDocument,
) -> (
    HashMap<String, u64>,
    HashMap<String, u64>,
    HashMap<String, u64>,
) {
    let mut by_tag = HashMap::new();
    let mut by_class = HashMap::new();
    let mut by_id = HashMap::new();
    for (id, node) in doc.tree().iter() {
        if let Some(el) = node.element_data() {
            let dbg = format!("{:?}", el.name.local);
            let tag = if let Some(rest) = dbg.strip_prefix("Atom('") {
                if let Some(end) = rest.find('\'') {
                    rest[..end].to_string()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            by_tag.entry(tag).or_insert(id.as_u64());
            if let Some(cls) = node.attr(local_name!("class")) {
                for c in cls.split_whitespace() {
                    by_class.entry(c.to_string()).or_insert(id.as_u64());
                }
            }
        }
        if let Some(id_attr) = node.attr(local_name!("id")) {
            by_id.entry(id_attr.to_string()).or_insert(id.as_u64());
        }
    }
    (by_tag, by_class, by_id)
}

fn set_text_content(doc: &mut BaseDocument, node_id: u64, value: &str) {
    let nid = blitz_traits::NodeId::from_u64(node_id);
    // REPLACE semantics, per DOM: assigning `textContent` removes every child
    // and inserts a single text node holding the new value. The previous
    // implementation only rewrote the first text child (or appended one), so
    // old element children survived and the DOM ended up holding old + new
    // while the JS mirror held exactly the new value — the held/fresh handle
    // split L940 recorded.
    let mut mutator = doc.mutate();
    // Remove every existing child first. NOTE: `remove_and_drop_node` also
    // FREES the child's slab slot (unlike `remove_node`, which only detaches),
    // so a handle the page captured earlier becomes dangling — `apply_op`'s
    // `node_is_live` guard drops writes through such handles instead of
    // letting blitz index a freed key and panic.
    if let Some(node) = mutator.doc.get_node(nid) {
        let existing: Vec<blitz_traits::NodeId> = node.children.iter().copied().collect();
        for child in existing {
            mutator.remove_and_drop_node(child);
        }
    }
    if !value.is_empty() {
        let text_id = mutator.create_text_node(value);
        mutator.append_children(nid, &[text_id]);
    }
}

fn set_attribute(doc: &mut BaseDocument, node_id: u64, name: &str, value: &str) {
    let qname = blitz_dom::QualName::new(None, blitz_dom::ns!(html), name.into());
    doc.mutate()
        .set_attribute(blitz_traits::NodeId::from_u64(node_id), qname, value);
}

fn create_and_append(
    doc: &mut BaseDocument,
    parent_id: u64,
    tag: &str,
    text: &str,
    attrs: &[(String, String)],
) {
    let attrs_vec = attrs
        .iter()
        .map(|(k, v)| blitz_dom::Attribute {
            name: blitz_dom::QualName::new(None, blitz_dom::ns!(html), k.as_str().into()),
            value: v.as_str().into(),
        })
        .collect();
    // Mutator session: create_element materializes stylo style data, and
    // append_children runs process_added_subtree (IS_IN_DOCUMENT + damage +
    // id map + per-tag post-processing) — the materialization the raw
    // create_node + children.push dance skipped, leaving replay-created
    // nodes at 0×0 and uncomposited (L940 leftover ①-cycle).
    let mut mutator = doc.mutate();
    let new_id = mutator.create_element(
        blitz_dom::QualName::new(None, blitz_dom::ns!(html), tag.into()),
        attrs_vec,
    );
    mutator.append_children(blitz_traits::NodeId::from_u64(parent_id), &[new_id]);
    if !text.is_empty() {
        let text_id = mutator.create_text_node(text);
        mutator.append_children(new_id, &[text_id]);
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
    if let Ok(ctor_val) = ctx
        .global_object()
        .get(boa_engine::js_string!(ctor_name), ctx)
    {
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

/// Trailing `_<slot>` of a listener global name
/// (`__aris_listener_<node>_<type>_<slot>`). The slot is always the last
/// `_`-separated component, so a type containing `_` is harmless.
fn slot_of(name: &str) -> Option<u32> {
    name.rsplit_once('_')
        .and_then(|(_, tail)| tail.parse().ok())
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
        let doc_val = ctx
            .global_object()
            .get(boa_engine::js_string!("document"), ctx)
            .ok();
        doc_val
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|d| d.get(boa_engine::js_string!("body"), ctx).ok())
            .and_then(|b| b.as_object())
            .and_then(|b| b.get(boa_engine::js_string!("_arisId"), ctx).ok())
            .and_then(|v| v.as_number())
            .map(|id| id as u64 == s.parent_id)
            .unwrap_or(false)
    };
    let parent_is_html = {
        let doc_val = ctx
            .global_object()
            .get(boa_engine::js_string!("document"), ctx)
            .ok();
        doc_val
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|d| d.get(boa_engine::js_string!("documentElement"), ctx).ok())
            .and_then(|b| b.as_object())
            .and_then(|b| b.get(boa_engine::js_string!("_arisId"), ctx).ok())
            .and_then(|v| v.as_number())
            .map(|id| id as u64 == s.parent_id)
            .unwrap_or(false)
    };
    let snapshot_parent_tag = if parent_is_body {
        "body"
    } else if parent_is_html {
        "html"
    } else {
        ""
    };
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
    // Do NOT write `textContent` here: `make_element_handle` installs it as an
    // accessor, and a data property would replace it — silently disabling the
    // setter (`el.textContent = x` would then only change the JS object).
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
    let _ = obj.insert_property(boa_engine::js_string!("prefix"), pd(JsValue::null()));
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

    // Store all attributes as JS properties. `style` is special: the handle
    // carries a style OBJECT whose `cssText` getter reads the
    // `_arisCssTextCache` — inserting a string data property here would
    // clobber that object on EVERY freshly-fetched handle (getElementById),
    // the exact symptom this PR fixes for setAttribute. Seeding the cache
    // also gives parsed style attributes a cssText readback.
    for (k, v) in &s.attrs {
        if k == "style" {
            if let Ok(style_val) = obj.get(boa_engine::js_string!("style"), ctx)
                && let Some(style_obj) = style_val.as_object()
            {
                let _ = style_obj.insert_property(
                    boa_engine::js_string!("_arisCssTextCache"),
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(JsValue::from(boa_engine::js_string!(v.clone())))
                        .writable(true)
                        .enumerable(false)
                        .configurable(true)
                        .build(),
                );
            }
            continue;
        }
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
        let _ = attr_obj.insert_property(boa_engine::js_string!("prefix"), pd(JsValue::null()));
        let _ =
            attr_obj.insert_property(boa_engine::js_string!("namespaceURI"), pd(JsValue::null()));
        let _ =
            attr_obj.insert_property(boa_engine::js_string!("specified"), pd(JsValue::from(true)));
        let _ = attr_obj.insert_property(
            boa_engine::js_string!("ownerElement"),
            pd(JsValue::from(obj.clone())),
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
    // `childNodes` is an array-like: without `length` the page reads
    // `undefined` and the whole collection is unusable (L940 F3 — a fresh
    // handle had `childNodes.length === undefined` while the parsed tree did
    // have children).
    let _ = child_arr.insert_property(
        boa_engine::js_string!("length"),
        pd(JsValue::from(s.child_ids.len() as u32)),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("childNodes"),
        pd(child_arr.clone().into()),
    );
    // children / childElementCount count ELEMENT children only — using the
    // total child count would report text nodes as elements (round-2 S2).
    let elem_arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    for (i, &cid) in s.element_child_ids.iter().enumerate() {
        let child_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ =
            child_obj.insert_property(boa_engine::js_string!("_arisId"), pd(JsValue::from(cid)));
        let _ = elem_arr.insert_property(i as u32, pd(child_obj.into()));
    }
    let _ = elem_arr.insert_property(
        boa_engine::js_string!("length"),
        pd(JsValue::from(s.element_child_ids.len() as u32)),
    );
    let _ = obj.insert_property(boa_engine::js_string!("children"), pd(elem_arr.into()));
    let _ = obj.insert_property(
        boa_engine::js_string!("childElementCount"),
        pd(JsValue::from(s.element_child_ids.len() as u32)),
    );
    // (childElementCount is set above from the derived element count.)
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
        let doc_val = ctx
            .global_object()
            .get(boa_engine::js_string!("document"), ctx)
            .ok();
        let doc_obj = doc_val.as_ref().and_then(|v| v.as_object());
        let parent_val = if parent_tag == "body" {
            doc_obj
                .and_then(|d| d.get(boa_engine::js_string!("body"), ctx).ok())
                .filter(|v| !v.is_null() && !v.is_undefined())
                .unwrap_or_else(|| {
                    let pn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let _ = pn.insert_property(
                        boa_engine::js_string!("_arisId"),
                        pd(JsValue::from(s.parent_id)),
                    );
                    JsValue::from(pn)
                })
        } else if parent_tag == "html" {
            doc_obj
                .and_then(|d| d.get(boa_engine::js_string!("documentElement"), ctx).ok())
                .filter(|v| !v.is_null() && !v.is_undefined())
                .unwrap_or_else(|| {
                    let pn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let _ = pn.insert_property(
                        boa_engine::js_string!("_arisId"),
                        pd(JsValue::from(s.parent_id)),
                    );
                    JsValue::from(pn)
                })
        } else {
            let pn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = pn.insert_property(
                boa_engine::js_string!("_arisId"),
                pd(JsValue::from(s.parent_id)),
            );
            JsValue::from(pn)
        };
        let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(parent_val.clone()));
        let _ = obj.insert_property(boa_engine::js_string!("parentElement"), pd(parent_val));
    } else {
        let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
        let _ = obj.insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
    }
}

fn read_handle_id(v: &JsValue) -> Option<u64> {
    let obj = v.as_object()?;
    let id = obj
        .get(boa_engine::js_string!("_arisId"), &mut Context::default())
        .ok()?;
    id.as_number().map(|n| n as u64).filter(|&n| n != 0)
}

fn read_pending(v: &JsValue) -> Option<u32> {
    let obj = v.as_object()?;
    let p = obj
        .get(boa_engine::js_string!("_pending"), &mut Context::default())
        .ok()?;
    p.as_number().map(|n| n as u32)
}

/// Allocation id carried by created canvas handles (`_canvasAlloc`), so
/// appendChild can route an AttachCanvas op before a DOM node exists.
fn read_canvas_alloc(v: &JsValue) -> Option<u64> {
    let obj = v.as_object()?;
    let a = obj
        .get(
            boa_engine::js_string!("_canvasAlloc"),
            &mut Context::default(),
        )
        .ok()?;
    a.as_number().map(|n| n as u64)
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
            || key_str.contains("_data")
        // Internal CharacterData store
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
            || key_str.contains("\"doctype\"")
        // Circular: doc → doctype → doc
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
                // Copy each entry under the SAME numeric key it had, and carry
                // `length` across explicitly: `length` is a number, so the
                // object-only filter below silently dropped it, leaving a clone
                // whose `childNodes.length` was `undefined` and whose
                // `hasChildNodes()` therefore reported false for a node that
                // demonstrably had children (round-4 S1).
                for ak in arr_keys.iter() {
                    let Ok(child_val) = arr.get(ak.clone(), ctx) else {
                        continue;
                    };
                    if let Some(child_obj) = child_val.as_object() {
                        let child_clone = clone_node_js(Some(&child_obj), true, ctx)?;
                        let _ = new_arr.insert_property(ak.clone(), pd(child_clone));
                    }
                }
                if let Ok(len_val) = arr.get(boa_engine::js_string!("length"), ctx)
                    && let Some(len) = len_val.as_number()
                {
                    let _ = new_arr.insert_property(
                        boa_engine::js_string!("length"),
                        pd(JsValue::from(len as u32)),
                    );
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
        let _ = obj.insert_property(
            boa_engine::js_string!("returnValue"),
            pd(JsValue::from(true)),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("cancelBubble"),
            pd(JsValue::from(false)),
        );
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
                let cancelable = o
                    .get(boa_engine::js_string!("cancelable"), ctx)
                    .ok()
                    .and_then(|v| v.as_boolean())
                    .unwrap_or(false);
                let passive = o
                    .get(boa_engine::js_string!("_passive"), ctx)
                    .ok()
                    .and_then(|v| v.as_boolean())
                    .unwrap_or(false);
                if cancelable && !passive {
                    let pd2 = |val: JsValue| {
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(val)
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build()
                    };
                    let _ = o.insert_property(
                        boa_engine::js_string!("defaultPrevented"),
                        pd2(JsValue::from(true)),
                    );
                    let _ = o.insert_property(
                        boa_engine::js_string!("returnValue"),
                        pd2(JsValue::from(false)),
                    );
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
        // The three methods used to be stubs: add/remove discarded their
        // arguments and dispatchEvent always returned true, so a listener on
        // a self-made EventTarget never ran (L940 follow-up). They now keep
        // per-target `_js_listeners` (same shape as the document/element
        // surfaces) and dispatch synchronously.
        let add_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let event_type = arg_to_string(args, 0, ctx);
            let handler = args.get(1).cloned().unwrap_or(JsValue::undefined());
            if let Some(o) = this.as_object() {
                let lmap = o
                    .get(boa_engine::js_string!("_js_listeners"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let lm = match lmap {
                    Some(m) => m,
                    None => {
                        let m = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = o.insert_property(
                            boa_engine::js_string!("_js_listeners"),
                            pd(m.clone().into()),
                        );
                        m
                    }
                };
                let existing = lm
                    .get(boa_engine::js_string!(event_type.clone()), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let arr = match existing {
                    Some(a) => a,
                    None => {
                        let a = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = a.insert_property(
                            boa_engine::js_string!("length"),
                            pd(JsValue::from(0u32)),
                        );
                        let _ = lm.insert_property(
                            boa_engine::js_string!(event_type.clone()),
                            pd(a.clone().into()),
                        );
                        a
                    }
                };
                let len = arr
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                let _ = arr.insert_property(len, pd(handler));
                let _ = arr
                    .insert_property(boa_engine::js_string!("length"), pd(JsValue::from(len + 1)));
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("addEventListener"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), add_fn).build(),
            )),
        );
        // removeEventListener — drop the matching entry by handler identity.
        let rm_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let event_type = arg_to_string(args, 0, ctx);
            let handler_obj = args.get(1).and_then(|v| v.as_object());
            if let Some(o) = this.as_object()
                && let Ok(lmap) = o.get(boa_engine::js_string!("_js_listeners"), ctx)
                && let Some(lm) = lmap.as_object()
                && let Ok(type_val) = lm.get(boa_engine::js_string!(event_type), ctx)
                && let Some(arr) = type_val.as_object()
            {
                let len = arr
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                let mut kept: Vec<JsValue> = Vec::new();
                for i in 0..len {
                    if let Ok(entry) = arr.get(i, ctx) {
                        let same = match (entry.as_object(), handler_obj.as_ref()) {
                            (Some(e), Some(w)) => boa_engine::object::JsObject::equals(&e, w),
                            _ => false,
                        };
                        if !same {
                            kept.push(entry);
                        }
                    }
                }
                for (i, v) in kept.iter().enumerate() {
                    let _ = arr.insert_property(i as u32, pd(v.clone()));
                }
                for stale in kept.len() as u32..len {
                    let _ = arr
                        .delete_property_or_throw(boa_engine::js_string!(stale.to_string()), ctx);
                }
                let _ = arr.insert_property(
                    boa_engine::js_string!("length"),
                    pd(JsValue::from(kept.len() as u32)),
                );
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("removeEventListener"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), rm_fn).build(),
            )),
        );
        // dispatchEvent — run this target's listeners for the event's type.
        let disp_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let event = args.first().cloned().unwrap_or(JsValue::null());
            let event_type = event
                .as_object()
                .and_then(|o| o.get(boa_engine::js_string!("type"), ctx).ok())
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            // DOM sets target/currentTarget at dispatch start, BEFORE any
            // listener lookup — an event dispatched on a listener-less target
            // (or one with only other types) still has a target (round-3 S3).
            if let Some(ev_obj) = event.as_object()
                && let Some(o) = this.as_object()
            {
                let pd_ev = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let t = JsValue::from(o.clone());
                let _ = ev_obj.insert_property(boa_engine::js_string!("target"), pd_ev(t.clone()));
                let _ = ev_obj.insert_property(boa_engine::js_string!("currentTarget"), pd_ev(t));
            }
            if let Some(o) = this.as_object()
                && let Ok(lmap) = o.get(boa_engine::js_string!("_js_listeners"), ctx)
                && let Some(lm) = lmap.as_object()
                && let Ok(type_val) = lm.get(boa_engine::js_string!(event_type), ctx)
                && let Some(arr) = type_val.as_object()
            {
                let len = arr
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                // Snapshot first: like the document surface, removal during
                // dispatch does not cancel an already-snapshotted handler.
                let mut handlers = Vec::new();
                for i in 0..len {
                    if let Ok(v) = arr.get(i, ctx) {
                        handlers.push(v);
                    }
                }
                let this_val = JsValue::from(o.clone());
                for cb in handlers {
                    let Some(f) = cb.as_object() else { continue };
                    if f.is_callable() {
                        let _ = f.call(&this_val, std::slice::from_ref(&event), ctx);
                    } else if let Ok(h) = f.get(boa_engine::js_string!("handleEvent"), ctx)
                        && let Some(hf) = h.as_object()
                        && hf.is_callable()
                    {
                        let _ = hf.call(&JsValue::from(f), std::slice::from_ref(&event), ctx);
                    }
                }
            }
            // WHATWG: currentTarget is null once dispatch completes.
            if let Some(ev_obj) = event.as_object() {
                let _ = ev_obj.insert_property(
                    boa_engine::js_string!("currentTarget"),
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(JsValue::null())
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build(),
                );
            }
            Ok(JsValue::from(true))
        });
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
        let pd2 = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = d.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(9u32)));
        let _ = d.insert_property(
            boa_engine::js_string!("nodeName"),
            pd2(JsValue::from(boa_engine::js_string!("#document"))),
        );
        let _ = d.insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::null()));
        let _ = d.insert_property(boa_engine::js_string!("textContent"), pd2(JsValue::null()));
        let _ = d.insert_property(
            boa_engine::js_string!("contentType"),
            pd2(JsValue::from(boa_engine::js_string!("application/xml"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("URL"),
            pd2(JsValue::from(boa_engine::js_string!("about:blank"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("documentURI"),
            pd2(JsValue::from(boa_engine::js_string!("about:blank"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("ownerDocument"),
            pd2(JsValue::null()),
        );
        let _ = d.insert_property(boa_engine::js_string!("defaultView"), pd2(JsValue::null()));
        // createElement (no uppercase for XML)
        let doc_ref = d.clone();
        let ce_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args, doc_ref, ctx| {
                let tag = arg_to_string(args, 0, ctx);
                let el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let pd3 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = el.insert_property(
                    boa_engine::js_string!("tagName"),
                    pd3(JsValue::from(boa_engine::js_string!(tag.clone()))),
                );
                let _ = el.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd3(JsValue::from(boa_engine::js_string!(tag.clone()))),
                );
                let _ = el.insert_property(
                    boa_engine::js_string!("localName"),
                    pd3(JsValue::from(boa_engine::js_string!(tag))),
                );
                let _ = el
                    .insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(1u32)));
                let _ =
                    el.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::null()));
                let _ = el.insert_property(
                    boa_engine::js_string!("ownerDocument"),
                    pd3(JsValue::from(doc_ref.clone())),
                );
                let _ = el
                    .insert_property(boa_engine::js_string!("namespaceURI"), pd3(JsValue::null()));
                let _ = el.insert_property(boa_engine::js_string!("prefix"), pd3(JsValue::null()));
                add_dom_methods(&el, ctx);
                Ok(el.into())
            },
            doc_ref,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createElement"),
            pd2(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ce_fn).build(),
            )),
        );
        // createTextNode
        let doc_ref2 = d.clone();
        let ct_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args, doc_ref2, ctx| {
                let text = arg_string(args, 0);
                let tn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let pd3 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = tn
                    .insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(3u32)));
                let _ = tn.insert_property(
                    boa_engine::js_string!("_data"),
                    pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = tn.insert_property(
                    boa_engine::js_string!("data"),
                    pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = tn.insert_property(
                    boa_engine::js_string!("nodeValue"),
                    pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = tn.insert_property(
                    boa_engine::js_string!("textContent"),
                    pd3(JsValue::from(boa_engine::js_string!(text))),
                );
                let _ = tn.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd3(JsValue::from(boa_engine::js_string!("#text"))),
                );
                let _ = tn.insert_property(
                    boa_engine::js_string!("ownerDocument"),
                    pd3(JsValue::from(doc_ref2.clone())),
                );
                Ok(tn.into())
            },
            doc_ref2,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createTextNode"),
            pd2(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ct_fn).build(),
            )),
        );
        // createComment
        let doc_ref3 = d.clone();
        let cc_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args, doc_ref3, ctx| {
                let text = arg_string(args, 0);
                let cn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let pd3 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = cn
                    .insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(8u32)));
                let _ = cn.insert_property(
                    boa_engine::js_string!("_data"),
                    pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = cn.insert_property(
                    boa_engine::js_string!("data"),
                    pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = cn.insert_property(
                    boa_engine::js_string!("nodeValue"),
                    pd3(JsValue::from(boa_engine::js_string!(text))),
                );
                let _ = cn.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd3(JsValue::from(boa_engine::js_string!("#comment"))),
                );
                let _ = cn.insert_property(
                    boa_engine::js_string!("ownerDocument"),
                    pd3(JsValue::from(doc_ref3.clone())),
                );
                Ok(cn.into())
            },
            doc_ref3,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createComment"),
            pd2(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cc_fn).build(),
            )),
        );
        // createProcessingInstruction
        let doc_ref4 = d.clone();
        let cpi_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args, doc_ref4, ctx| {
                let target = arg_string(args, 0);
                let target2 = target.clone();
                let data = arg_string(args, 1);
                let pi = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let pd3 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = pi
                    .insert_property(boa_engine::js_string!("nodeType"), pd3(JsValue::from(7u32)));
                let _ = pi.insert_property(
                    boa_engine::js_string!("_data"),
                    pd3(JsValue::from(boa_engine::js_string!(data.clone()))),
                );
                let _ = pi.insert_property(
                    boa_engine::js_string!("data"),
                    pd3(JsValue::from(boa_engine::js_string!(data.clone()))),
                );
                let _ = pi.insert_property(
                    boa_engine::js_string!("nodeValue"),
                    pd3(JsValue::from(boa_engine::js_string!(data))),
                );
                let _ = pi.insert_property(
                    boa_engine::js_string!("target"),
                    pd3(JsValue::from(boa_engine::js_string!(target))),
                );
                let _ = pi.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd3(JsValue::from(boa_engine::js_string!(target2))),
                );
                let _ = pi.insert_property(
                    boa_engine::js_string!("ownerDocument"),
                    pd3(JsValue::from(doc_ref4.clone())),
                );
                Ok(pi.into())
            },
            doc_ref4,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createProcessingInstruction"),
            pd2(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cpi_fn).build(),
            )),
        );
        // createDocumentFragment
        let df_fn = NativeFunction::from_copy_closure(|_t, _a, ctx| {
            let df = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd3 = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = df.insert_property(
                boa_engine::js_string!("nodeType"),
                pd3(JsValue::from(11u32)),
            );
            let _ = df.insert_property(
                boa_engine::js_string!("nodeName"),
                pd3(JsValue::from(boa_engine::js_string!("#document-fragment"))),
            );
            let _ = df.insert_property(boa_engine::js_string!("nodeValue"), pd3(JsValue::null()));
            add_dom_methods(&df, ctx);
            Ok(df.into())
        });
        let _ = d.insert_property(
            boa_engine::js_string!("createDocumentFragment"),
            pd2(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_fn).build(),
            )),
        );
        // createRange
        let doc_ref5 = d.clone();
        let cr_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, _a, doc_ref5, ctx| {
                let pd3 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let r = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let _ = r.insert_property(
                    boa_engine::js_string!("startContainer"),
                    pd3(JsValue::from(doc_ref5.clone())),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("endContainer"),
                    pd3(JsValue::from(doc_ref5.clone())),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("startOffset"),
                    pd3(JsValue::from(0u32)),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("endOffset"),
                    pd3(JsValue::from(0u32)),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("collapsed"),
                    pd3(JsValue::from(true)),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("commonAncestorContainer"),
                    pd3(JsValue::from(doc_ref5.clone())),
                );
                Ok(r.into())
            },
            doc_ref5,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createRange"),
            pd2(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cr_fn).build(),
            )),
        );
        // Add DOM methods (appendChild etc.) so xmlDoc.appendChild works.
        add_dom_methods(&d, ctx);
        // Set Document.prototype for instanceof + Node method inheritance.
        if let Ok(ctor_val) = ctx
            .global_object()
            .get(boa_engine::js_string!("Document"), ctx)
        {
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
        let app_fn = NativeFunction::from_copy_closure_with_captures(
            move |this, args, _doc_ref_app, ctx| {
                if let Some(o) = this.as_object() {
                    for arg in args.iter() {
                        let node = value_to_node(arg, ctx);
                        if let Some(child_obj) = node.as_object() {
                            insert_into_children(&o, &child_obj, None, ctx);
                        }
                    }
                }
                Ok(JsValue::undefined())
            },
            doc_ref_app,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("append"),
            pd2(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), app_fn).build(),
            )),
        );
        let prep_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            if let Some(o) = this.as_object() {
                let first_child = if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                    if let Some(ca) = cv.as_object() {
                        let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                        if v.is_undefined() {
                            None
                        } else {
                            v.as_object()
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        insert_into_children(&o, &child_obj, first_child.clone(), ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        });
        let _ = d.insert_property(
            boa_engine::js_string!("prepend"),
            pd2(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), prep_fn).build(),
            )),
        );
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
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        // Build a proper document with doctype, head, title, body.
        let doctype = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = doctype.insert_property(
            boa_engine::js_string!("name"),
            pd(JsValue::from(boa_engine::js_string!("html"))),
        );
        let _ = doctype.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("html"))),
        );
        let _ = doctype.insert_property(
            boa_engine::js_string!("publicId"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        let _ = doctype.insert_property(
            boa_engine::js_string!("systemId"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        let _ =
            doctype.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32)));
        let _ = doctype.insert_property(
            boa_engine::js_string!("ownerDocument"),
            pd(JsValue::from(d.clone())),
        );
        let _ = d.insert_property(boa_engine::js_string!("doctype"), pd(doctype.into()));
        let _ = d.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(9u32)));
        let _ = d.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("#document"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("contentType"),
            pd(JsValue::from(boa_engine::js_string!("text/html"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("URL"),
            pd(JsValue::from(boa_engine::js_string!("about:blank"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("documentURI"),
            pd(JsValue::from(boa_engine::js_string!("about:blank"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("compatMode"),
            pd(JsValue::from(boa_engine::js_string!("CSS1Compat"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("characterSet"),
            pd(JsValue::from(boa_engine::js_string!("UTF-8"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("charset"),
            pd(JsValue::from(boa_engine::js_string!("UTF-8"))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("inputEncoding"),
            pd(JsValue::from(boa_engine::js_string!("UTF-8"))),
        );
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
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = dt.insert_property(
                    boa_engine::js_string!("name"),
                    pd(JsValue::from(boa_engine::js_string!(name.clone()))),
                );
                let _ = dt.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd(JsValue::from(boa_engine::js_string!(name))),
                );
                let _ = dt.insert_property(
                    boa_engine::js_string!("publicId"),
                    pd(JsValue::from(boa_engine::js_string!(public_id))),
                );
                let _ = dt.insert_property(
                    boa_engine::js_string!("systemId"),
                    pd(JsValue::from(boa_engine::js_string!(system_id))),
                );
                // nodeValue and textContent for DocumentType: always null (setter is no-op).
                let null_get =
                    NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
                let noop_set =
                    NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
                let ng_fn =
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_get).build();
                let ns_fn =
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_set).build();
                let _ = dt.insert_property(
                    boa_engine::js_string!("nodeValue"),
                    boa_engine::property::PropertyDescriptor::builder()
                        .get(ng_fn.clone())
                        .set(ns_fn.clone())
                        .enumerable(true)
                        .configurable(true)
                        .build(),
                );
                let _ = dt
                    .insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32)));
                let _ = dt.insert_property(
                    boa_engine::js_string!("textContent"),
                    boa_engine::property::PropertyDescriptor::builder()
                        .get(ng_fn)
                        .set(ns_fn)
                        .enumerable(true)
                        .configurable(true)
                        .build(),
                );
                let _ = dt.insert_property(
                    boa_engine::js_string!("ownerDocument"),
                    pd(JsValue::from(doc_ref.clone())),
                );
                // Set DocumentType.prototype.
                if let Ok(ctor_val) = ctx
                    .global_object()
                    .get(boa_engine::js_string!("DocumentType"), ctx)
                {
                    if let Some(ctor_obj) = ctor_val.as_object() {
                        if let Ok(proto_val) =
                            ctor_obj.get(boa_engine::js_string!("prototype"), ctx)
                        {
                            if let Some(proto) = proto_val.as_object() {
                                let _ = dt.set_prototype(Some(proto));
                            }
                        }
                    }
                }
                Ok(dt.into())
            },
            doc_ref,
        );
        let hf_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::from(true)));
        let _ = impl2.insert_property(
            boa_engine::js_string!("hasFeature"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), hf_fn).build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let _ = impl2.insert_property(
            boa_engine::js_string!("createDocumentType"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cdt_fn).build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let _ = d.insert_property(boa_engine::js_string!("implementation"), pd(impl2.into()));
        // head + body stubs.
        let head = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = head.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("HEAD"))),
        );
        let title_el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = title_el.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("TITLE"))),
        );
        let _ = title_el.insert_property(
            boa_engine::js_string!("textContent"),
            pd(JsValue::from(boa_engine::js_string!(title))),
        );
        let _ = d.insert_property(boa_engine::js_string!("head"), pd(head.into()));
        let body = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = body.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("BODY"))),
        );
        let _ = body.insert_property(
            boa_engine::js_string!("tagName"),
            pd(JsValue::from(boa_engine::js_string!("BODY"))),
        );
        let _ = body.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(1u32)));
        add_dom_methods(&body, ctx);
        let _ = d.insert_property(boa_engine::js_string!("body"), pd(body.into()));
        let html_el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = html_el.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("HTML"))),
        );
        let _ = html_el.insert_property(
            boa_engine::js_string!("tagName"),
            pd(JsValue::from(boa_engine::js_string!("HTML"))),
        );
        let _ =
            html_el.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(1u32)));
        add_dom_methods(&html_el, ctx);
        let _ = d.insert_property(
            boa_engine::js_string!("documentElement"),
            pd(html_el.into()),
        );
        // createElement on this document (sets ownerDocument = this doc).
        let doc_for_ce = d.clone();
        let ce_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args, doc_for_ce, ctx| {
                let tag = arg_to_string(args, 0, ctx);
                let upper: String = tag
                    .chars()
                    .map(|c| {
                        if c.is_ascii_lowercase() {
                            c.to_ascii_uppercase()
                        } else {
                            c
                        }
                    })
                    .collect();
                let lower: String = tag
                    .chars()
                    .map(|c| {
                        if c.is_ascii_uppercase() {
                            c.to_ascii_lowercase()
                        } else {
                            c
                        }
                    })
                    .collect();
                let el = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let pd2 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = el.insert_property(
                    boa_engine::js_string!("tagName"),
                    pd2(JsValue::from(boa_engine::js_string!(upper.clone()))),
                );
                let _ = el.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd2(JsValue::from(boa_engine::js_string!(upper))),
                );
                let _ = el.insert_property(
                    boa_engine::js_string!("localName"),
                    pd2(JsValue::from(boa_engine::js_string!(lower))),
                );
                let _ = el.insert_property(
                    boa_engine::js_string!("namespaceURI"),
                    pd2(JsValue::from(boa_engine::js_string!(
                        "http://www.w3.org/1999/xhtml"
                    ))),
                );
                let _ = el.insert_property(boa_engine::js_string!("prefix"), pd2(JsValue::null()));
                let _ = el
                    .insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(1u32)));
                let _ =
                    el.insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::null()));
                let _ = el.insert_property(
                    boa_engine::js_string!("ownerDocument"),
                    pd2(JsValue::from(doc_for_ce.clone())),
                );
                add_dom_methods(&el, ctx);
                Ok(el.into())
            },
            doc_for_ce,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createElement"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ce_fn).build(),
            )),
        );
        // createTextNode
        let doc_for_ct = d.clone();
        let ct_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args, doc_for_ct, ctx| {
                let text = arg_string(args, 0);
                let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let pd2 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = obj
                    .insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(3u32)));
                let _ = obj.insert_property(
                    boa_engine::js_string!("_data"),
                    pd2(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = obj.insert_property(
                    boa_engine::js_string!("data"),
                    pd2(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = obj.insert_property(
                    boa_engine::js_string!("textContent"),
                    pd2(JsValue::from(boa_engine::js_string!(text))),
                );
                let _ = obj.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd2(JsValue::from(boa_engine::js_string!("#text"))),
                );
                let _ = obj.insert_property(
                    boa_engine::js_string!("ownerDocument"),
                    pd2(JsValue::from(doc_for_ct.clone())),
                );
                Ok(obj.into())
            },
            doc_for_ct,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createTextNode"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ct_fn).build(),
            )),
        );
        // createComment
        let doc_for_cc = d.clone();
        let cc_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args, doc_for_cc, ctx| {
                let text = arg_string(args, 0);
                let cn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let pd2 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = cn
                    .insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(8u32)));
                let _ = cn.insert_property(
                    boa_engine::js_string!("_data"),
                    pd2(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = cn.insert_property(
                    boa_engine::js_string!("data"),
                    pd2(JsValue::from(boa_engine::js_string!(text.clone()))),
                );
                let _ = cn.insert_property(
                    boa_engine::js_string!("nodeValue"),
                    pd2(JsValue::from(boa_engine::js_string!(text))),
                );
                let _ = cn.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd2(JsValue::from(boa_engine::js_string!("#comment"))),
                );
                let _ = cn.insert_property(
                    boa_engine::js_string!("ownerDocument"),
                    pd2(JsValue::from(doc_for_cc.clone())),
                );
                Ok(cn.into())
            },
            doc_for_cc,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createComment"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cc_fn).build(),
            )),
        );
        // createDocumentFragment
        let df_fn = NativeFunction::from_copy_closure(|_t, _a, ctx| {
            let df = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = df.insert_property(
                boa_engine::js_string!("nodeType"),
                pd2(JsValue::from(11u32)),
            );
            let _ = df.insert_property(
                boa_engine::js_string!("nodeName"),
                pd2(JsValue::from(boa_engine::js_string!("#document-fragment"))),
            );
            // nodeValue: always null (setter is no-op per spec).
            let null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
            let noop_set =
                NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
            let ng_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_get).build();
            let ns_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_set).build();
            let _ = df.insert_property(
                boa_engine::js_string!("nodeValue"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(ng_fn.clone())
                    .set(ns_fn.clone())
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // Initialize _children for textContent setter support.
            let _ = df.insert_property(
                boa_engine::js_string!("_children"),
                pd2(new_array(ctx).into()),
            );
            // textContent accessor: getter concatenates children text, setter replaces children.
            let df_tc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    return Ok(JsValue::from(boa_engine::js_string!(collect_text_content(
                        &o, ctx, 0
                    ))));
                }
                Ok(JsValue::from(boa_engine::js_string!("")))
            });
            let df_tc_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    let val = args.first().map(|v| v.clone()).unwrap_or(JsValue::null());
                    let s = if val.is_null() {
                        String::new()
                    } else {
                        val.to_string(ctx)
                            .map(|s| s.to_std_string_escaped())
                            .unwrap_or_default()
                    };
                    let pd3 = |v: JsValue| {
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(v)
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build()
                    };
                    if s.is_empty() {
                        let empty_arr = new_array(ctx);
                        let _ = o.insert_property(
                            boa_engine::js_string!("_children"),
                            pd3(empty_arr.into()),
                        );
                    } else {
                        let text_node =
                            boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = text_node.insert_property(
                            boa_engine::js_string!("nodeType"),
                            pd3(JsValue::from(3u32)),
                        );
                        let _ = text_node.insert_property(
                            boa_engine::js_string!("_data"),
                            pd3(JsValue::from(boa_engine::js_string!(s.clone()))),
                        );
                        let _ = text_node.insert_property(
                            boa_engine::js_string!("data"),
                            pd3(JsValue::from(boa_engine::js_string!(s.clone()))),
                        );
                        let _ = text_node.insert_property(
                            boa_engine::js_string!("nodeValue"),
                            pd3(JsValue::from(boa_engine::js_string!(s.clone()))),
                        );
                        let _ = text_node.insert_property(
                            boa_engine::js_string!("textContent"),
                            pd3(JsValue::from(boa_engine::js_string!(s))),
                        );
                        let _ = text_node.insert_property(
                            boa_engine::js_string!("nodeName"),
                            pd3(JsValue::from(boa_engine::js_string!("#text"))),
                        );
                        let _ = text_node.insert_property(
                            boa_engine::js_string!("parentNode"),
                            pd3(JsValue::from(o.clone())),
                        );
                        if let Ok(text_ctor) =
                            ctx.global_object().get(boa_engine::js_string!("Text"), ctx)
                        {
                            if let Some(tc) = text_ctor.as_object() {
                                if let Ok(proto_val) =
                                    tc.get(boa_engine::js_string!("prototype"), ctx)
                                {
                                    if let Some(proto) = proto_val.as_object() {
                                        let _ = text_node.set_prototype(Some(proto));
                                    }
                                }
                            }
                        }
                        let new_arr = new_array(ctx);
                        let _ = new_arr.push(JsValue::from(text_node), ctx);
                        let _ = o.insert_property(
                            boa_engine::js_string!("_children"),
                            pd3(new_arr.into()),
                        );
                    }
                }
                Ok(JsValue::undefined())
            });
            let df_tg =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_tc_get).build();
            let df_ts =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_tc_set).build();
            let _ = df.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(df_tg)
                    .set(df_ts)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // firstChild/lastChild/childNodes as accessors on _children.
            let fc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if let Some(ca) = cv.as_object() {
                            let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                            if v.is_undefined() {
                                return Ok(JsValue::null());
                            }
                            return Ok(v);
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let cn_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if cv.is_object() {
                            return Ok(cv);
                        }
                    }
                }
                Ok(JsValue::from(new_array(ctx)))
            });
            let lc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if let Some(ca) = cv.as_object() {
                            let len = ca
                                .get(boa_engine::js_string!("length"), ctx)
                                .ok()
                                .and_then(|v| v.as_number())
                                .unwrap_or(0.0) as u32;
                            if len == 0 {
                                return Ok(JsValue::null());
                            }
                            let v = ca.get(len - 1, ctx).unwrap_or(JsValue::null());
                            if v.is_undefined() {
                                return Ok(JsValue::null());
                            }
                            return Ok(v);
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let fc_fn2 =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), fc_get).build();
            let cn_fn2 =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cn_get).build();
            let lc_fn2 =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), lc_get).build();
            let _ = df.insert_property(
                boa_engine::js_string!("firstChild"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(fc_fn2.clone())
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            let _ = df.insert_property(
                boa_engine::js_string!("lastChild"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(lc_fn2)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            let _ = df.insert_property(
                boa_engine::js_string!("childNodes"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cn_fn2)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // Set DocumentFragment.prototype.
            if let Ok(ctor_val) = ctx
                .global_object()
                .get(boa_engine::js_string!("DocumentFragment"), ctx)
            {
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
        let _ = d.insert_property(
            boa_engine::js_string!("createDocumentFragment"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_fn).build(),
            )),
        );
        // Add DOM methods to the document itself (appendChild, etc.)
        add_dom_methods(&d, ctx);
        // createRange on this document.
        let doc_for_cr = d.clone();
        let cr_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, _a, doc_for_cr, ctx| {
                let pd2 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let r = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let _ = r.insert_property(
                    boa_engine::js_string!("startContainer"),
                    pd2(JsValue::from(doc_for_cr.clone())),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("endContainer"),
                    pd2(JsValue::from(doc_for_cr.clone())),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("startOffset"),
                    pd2(JsValue::from(0u32)),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("endOffset"),
                    pd2(JsValue::from(0u32)),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("collapsed"),
                    pd2(JsValue::from(true)),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("commonAncestorContainer"),
                    pd2(JsValue::from(doc_for_cr.clone())),
                );
                // setStart/setEnd
                let ss_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                    if let Some(o) = this.as_object() {
                        let pd3 = |val: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(val)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let container = args.first().cloned().unwrap_or(JsValue::null());
                        let offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                        let _ = o.insert_property(
                            boa_engine::js_string!("startContainer"),
                            pd3(container),
                        );
                        let _ = o.insert_property(
                            boa_engine::js_string!("startOffset"),
                            pd3(JsValue::from(offset)),
                        );
                    }
                    Ok(JsValue::undefined())
                });
                let se_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                    if let Some(o) = this.as_object() {
                        let pd3 = |val: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(val)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let container = args.first().cloned().unwrap_or(JsValue::null());
                        let offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                        let _ = o.insert_property(
                            boa_engine::js_string!("endContainer"),
                            pd3(container),
                        );
                        let _ = o.insert_property(
                            boa_engine::js_string!("endOffset"),
                            pd3(JsValue::from(offset)),
                        );
                    }
                    Ok(JsValue::undefined())
                });
                let _ = r.insert_property(
                    boa_engine::js_string!("setStart"),
                    pd2(JsValue::from(
                        boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ss_fn).build(),
                    )),
                );
                let _ = r.insert_property(
                    boa_engine::js_string!("setEnd"),
                    pd2(JsValue::from(
                        boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), se_fn).build(),
                    )),
                );
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
                                    let _ = push_fn.call(
                                        &JsValue::from(arr.clone()),
                                        &[JsValue::from(r.clone())],
                                        ctx,
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(r.into())
            },
            doc_for_cr,
        );
        let _ = d.insert_property(
            boa_engine::js_string!("createRange"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cr_fn).build(),
            )),
        );
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
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = d.insert_property(
            boa_engine::js_string!("name"),
            pd(JsValue::from(boa_engine::js_string!(name.clone()))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!(name.clone()))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("publicId"),
            pd(JsValue::from(boa_engine::js_string!(public_id))),
        );
        let _ = d.insert_property(
            boa_engine::js_string!("systemId"),
            pd(JsValue::from(boa_engine::js_string!(system_id))),
        );
        // nodeValue and textContent for DocumentType: always null (setter is no-op).
        let null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
        let noop_set = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let ng_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_get).build();
        let ns_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_set).build();
        let _ = d.insert_property(
            boa_engine::js_string!("nodeValue"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(ng_fn.clone())
                .set(ns_fn.clone())
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let _ = d.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32))); // DOCUMENT_TYPE_NODE
        let _ = d.insert_property(
            boa_engine::js_string!("textContent"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(ng_fn)
                .set(ns_fn)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        // ownerDocument = the current document.
        let doc_val = ctx
            .global_object()
            .get(boa_engine::js_string!("document"), ctx)
            .unwrap_or(JsValue::null());
        let _ = d.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
        // Set DocumentType.prototype for instanceof + Node method inheritance.
        if let Ok(ctor_val) = ctx
            .global_object()
            .get(boa_engine::js_string!("DocumentType"), ctx)
        {
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
            let _ = obj.insert_property(
                boa_engine::js_string!("length"),
                pd(JsValue::from(text_len)),
            );
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
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = new_obj
                    .insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32)));
                let js_str = boa_engine::JsString::from(&second[..]);
                let _ = new_obj.insert_property(
                    boa_engine::js_string!("data"),
                    pd(JsValue::from(js_str.clone())),
                );
                let _ = new_obj.insert_property(
                    boa_engine::js_string!("textContent"),
                    pd(JsValue::from(js_str)),
                );
                let _ = new_obj.insert_property(
                    boa_engine::js_string!("length"),
                    pd(JsValue::from(second.len() as u32)),
                );
                let _ = new_obj.insert_property(
                    boa_engine::js_string!("nodeName"),
                    pd(JsValue::from(boa_engine::js_string!("#text"))),
                );
                // Add CharacterData methods
                for (mname, mfn) in build_character_data_methods() {
                    let _ = new_obj.insert_property(
                        boa_engine::js_string!(mname),
                        pd(JsValue::from(
                            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), mfn)
                                .build(),
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
            let nv_get_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nv_get).build();
            let nv_set_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nv_set).build();
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
            let dg_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), data_get).build();
            let ds_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), data_set).build();
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
            let tcg_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_get).build();
            let tcs_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_set).build();
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
            if let Ok(doc_val) = ctx
                .global_object()
                .get(boa_engine::js_string!("document"), ctx)
            {
                let _ = obj.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
            }
            // Text nodes have no children.
            let empty_arr = new_array(ctx);
            let _ = obj.insert_property(boa_engine::js_string!("childNodes"), pd(empty_arr.into()));
            let _ = obj.insert_property(boa_engine::js_string!("firstChild"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("lastChild"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
            let has_child_nodes =
                NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::from(false)));
            let _ = obj.insert_property(
                boa_engine::js_string!("hasChildNodes"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), has_child_nodes)
                        .build(),
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
            let cd_get_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cd_get).build();
            let cd_set_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cd_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("data"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cd_get_fn.clone())
                    .set(cd_set_fn.clone())
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeValue"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cd_get_fn.clone())
                    .set(cd_set_fn.clone())
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // textContent as accessor (same get/set as data/nodeValue).
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cd_get_fn)
                    .set(cd_set_fn)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
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
            if let Ok(ctor_val) = ctx
                .global_object()
                .get(boa_engine::js_string!("Comment"), ctx)
            {
                if let Some(ctor_obj) = ctor_val.as_object() {
                    if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(proto) = proto_val.as_object() {
                            let _ = obj.set_prototype(Some(proto));
                        }
                    }
                }
            }
            // ownerDocument — the global document object.
            if let Ok(doc_val) = ctx
                .global_object()
                .get(boa_engine::js_string!("document"), ctx)
            {
                let _ = obj.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
            }
            // Comment nodes have no children.
            let empty_arr2 = new_array(ctx);
            let _ =
                obj.insert_property(boa_engine::js_string!("childNodes"), pd(empty_arr2.into()));
            let _ = obj.insert_property(boa_engine::js_string!("firstChild"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("lastChild"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
            let has_child_nodes2 =
                NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::from(false)));
            let _ = obj.insert_property(
                boa_engine::js_string!("hasChildNodes"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), has_child_nodes2)
                        .build(),
                )),
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
            let noop_set =
                NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
            let ng_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), null_get).build();
            let ns_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeValue"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(ng_fn.clone())
                    .set(ns_fn.clone())
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // Initialize _children for textContent/firstChild/childNodes support.
            let _ = obj.insert_property(
                boa_engine::js_string!("_children"),
                pd(new_array(ctx).into()),
            );
            // textContent accessor.
            let df_tc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    return Ok(JsValue::from(boa_engine::js_string!(collect_text_content(
                        &o, ctx, 0
                    ))));
                }
                Ok(JsValue::from(boa_engine::js_string!("")))
            });
            let df_tc_set = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    let val = args.first().map(|v| v.clone()).unwrap_or(JsValue::null());
                    let s = if val.is_null() {
                        String::new()
                    } else {
                        val.to_string(ctx)
                            .map(|s| s.to_std_string_escaped())
                            .unwrap_or_default()
                    };
                    let pd3 = |v: JsValue| {
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(v)
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build()
                    };
                    if s.is_empty() {
                        let empty_arr = new_array(ctx);
                        let _ = o.insert_property(
                            boa_engine::js_string!("_children"),
                            pd3(empty_arr.into()),
                        );
                    } else {
                        let tn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = tn.insert_property(
                            boa_engine::js_string!("nodeType"),
                            pd3(JsValue::from(3u32)),
                        );
                        let _ = tn.insert_property(
                            boa_engine::js_string!("_data"),
                            pd3(JsValue::from(boa_engine::js_string!(s.clone()))),
                        );
                        let _ = tn.insert_property(
                            boa_engine::js_string!("data"),
                            pd3(JsValue::from(boa_engine::js_string!(s.clone()))),
                        );
                        let _ = tn.insert_property(
                            boa_engine::js_string!("nodeValue"),
                            pd3(JsValue::from(boa_engine::js_string!(s.clone()))),
                        );
                        let _ = tn.insert_property(
                            boa_engine::js_string!("textContent"),
                            pd3(JsValue::from(boa_engine::js_string!(s))),
                        );
                        let _ = tn.insert_property(
                            boa_engine::js_string!("nodeName"),
                            pd3(JsValue::from(boa_engine::js_string!("#text"))),
                        );
                        let _ = tn.insert_property(
                            boa_engine::js_string!("parentNode"),
                            pd3(JsValue::from(o.clone())),
                        );
                        if let Ok(text_ctor) =
                            ctx.global_object().get(boa_engine::js_string!("Text"), ctx)
                        {
                            if let Some(tc) = text_ctor.as_object() {
                                if let Ok(pv) = tc.get(boa_engine::js_string!("prototype"), ctx) {
                                    if let Some(p) = pv.as_object() {
                                        let _ = tn.set_prototype(Some(p));
                                    }
                                }
                            }
                        }
                        let new_arr = new_array(ctx);
                        let _ = new_arr.push(JsValue::from(tn), ctx);
                        let _ = o.insert_property(
                            boa_engine::js_string!("_children"),
                            pd3(new_arr.into()),
                        );
                    }
                }
                Ok(JsValue::undefined())
            });
            let df_tg =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_tc_get).build();
            let df_ts =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), df_tc_set).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(df_tg)
                    .set(df_ts)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // firstChild/lastChild/childNodes accessors.
            let fc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if let Some(ca) = cv.as_object() {
                            let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                            if v.is_undefined() {
                                return Ok(JsValue::null());
                            }
                            return Ok(v);
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let cn_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if cv.is_object() {
                            return Ok(cv);
                        }
                    }
                }
                Ok(JsValue::from(new_array(ctx)))
            });
            let fc_fn3 =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), fc_get).build();
            let lc_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                        if let Some(ca) = cv.as_object() {
                            let len = ca
                                .get(boa_engine::js_string!("length"), ctx)
                                .ok()
                                .and_then(|v| v.as_number())
                                .unwrap_or(0.0) as u32;
                            if len == 0 {
                                return Ok(JsValue::null());
                            }
                            let v = ca.get(len - 1, ctx).unwrap_or(JsValue::null());
                            if v.is_undefined() {
                                return Ok(JsValue::null());
                            }
                            return Ok(v);
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let cn_fn3 =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cn_get).build();
            let lc_fn3 =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), lc_get).build();
            let _ = obj.insert_property(
                boa_engine::js_string!("firstChild"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(fc_fn3.clone())
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("lastChild"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(lc_fn3)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("childNodes"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cn_fn3)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // Set DocumentFragment.prototype.
            if let Ok(ctor_val) = ctx
                .global_object()
                .get(boa_engine::js_string!("DocumentFragment"), ctx)
            {
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
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), doc_append).build(),
            )),
        );
        // document.prepend(...nodes) — ParentNode method.
        let doc_prepend = NativeFunction::from_copy_closure(|this, args, ctx| {
            if let Some(o) = this.as_object() {
                // Find first child (reference node).
                let first_child = if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                    if let Some(ca) = cv.as_object() {
                        let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                        if v.is_undefined() {
                            None
                        } else {
                            v.as_object()
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
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
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), doc_prepend).build(),
            )),
        );

        // document.createProcessingInstruction(target, data)
        let create_pi = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let target = arg_string(args, 0);
            let data = arg_string(args, 1);
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ =
                obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(7u32))); // PROCESSING_INSTRUCTION_NODE
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeName"),
                pd(JsValue::from(boa_engine::js_string!(target.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("target"),
                pd(JsValue::from(boa_engine::js_string!(target))),
            );
            // data and nodeValue are the same for PI. Use accessors that read/write _data.
            let _ = obj.insert_property(
                boa_engine::js_string!("_data"),
                pd(JsValue::from(boa_engine::js_string!(data))),
            );
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
            let pi_dg =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pi_data_get).build();
            let pi_ds =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pi_data_set).build();
            for prop_name in &["data", "nodeValue", "textContent"] {
                let _ = obj.insert_property(
                    boa_engine::js_string!(*prop_name),
                    boa_engine::property::PropertyDescriptor::builder()
                        .get(pi_dg.clone())
                        .set(pi_ds.clone())
                        .enumerable(true)
                        .configurable(true)
                        .build(),
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
            let pd2 = |v: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(v)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ =
                obj.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(4u32)));
            let _ = obj.insert_property(
                boa_engine::js_string!("_data"),
                pd2(JsValue::from(boa_engine::js_string!(data.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("data"),
                pd2(JsValue::from(boa_engine::js_string!(data.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeValue"),
                pd2(JsValue::from(boa_engine::js_string!(data.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                pd2(JsValue::from(boa_engine::js_string!(data))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeName"),
                pd2(JsValue::from(boa_engine::js_string!("#cdata-section"))),
            );
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createCDATASection"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_cdata).build(),
            )),
        );

        // document.createAttribute(name)
        let create_attr = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let name = arg_string(args, 0);
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |v: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(v)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = obj.insert_property(
                boa_engine::js_string!("name"),
                pd2(JsValue::from(boa_engine::js_string!(name.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeName"),
                pd2(JsValue::from(boa_engine::js_string!(name.clone()))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("localName"),
                pd2(JsValue::from(boa_engine::js_string!(name))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("value"),
                pd2(JsValue::from(boa_engine::js_string!(""))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("nodeValue"),
                pd2(JsValue::from(boa_engine::js_string!(""))),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("textContent"),
                pd2(JsValue::from(boa_engine::js_string!(""))),
            );
            let _ = obj.insert_property(boa_engine::js_string!("prefix"), pd2(JsValue::null()));
            let _ =
                obj.insert_property(boa_engine::js_string!("namespaceURI"), pd2(JsValue::null()));
            let _ = obj.insert_property(
                boa_engine::js_string!("specified"),
                pd2(JsValue::from(true)),
            );
            let _ =
                obj.insert_property(boa_engine::js_string!("ownerElement"), pd2(JsValue::null()));
            let _ =
                obj.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(2u32)));
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
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_attr).build(),
            )),
        );

        // document.createTreeWalker(root, whatToShow, filter)
        let create_tw = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let root = args.first().cloned().unwrap_or(JsValue::null());
            let what_to_show = args.get(1).cloned().unwrap_or(JsValue::from(0xFFFFFFFFu32));
            let filter = args.get(2).cloned().unwrap_or(JsValue::null());
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(false)
                    .enumerable(true)
                    .configurable(false)
                    .build()
            };
            let _ = obj.insert_property(boa_engine::js_string!("root"), pd2(root.clone()));
            let _ = obj.insert_property(boa_engine::js_string!("whatToShow"), pd2(what_to_show));
            let _ = obj.insert_property(boa_engine::js_string!("filter"), pd2(filter));
            let _ = obj.insert_property(
                boa_engine::js_string!("currentNode"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(root)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // parentNode, firstChild, lastChild, previousSibling, nextSibling — stubs returning null.
            let null_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::null()));
            for mname in &[
                "parentNode",
                "firstChild",
                "lastChild",
                "previousSibling",
                "nextSibling",
            ] {
                let _ = obj.insert_property(
                    boa_engine::js_string!(*mname),
                    pd2(JsValue::from(
                        boa_engine::object::FunctionObjectBuilder::new(
                            ctx.realm(),
                            null_fn.clone(),
                        )
                        .build(),
                    )),
                );
            }
            // Set TreeWalker.prototype and Symbol.toStringTag.
            if let Ok(ctor_val) = ctx
                .global_object()
                .get(boa_engine::js_string!("TreeWalker"), ctx)
            {
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
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_tw).build(),
            )),
        );

        // document.createNodeIterator(root, whatToShow, filter)
        let create_ni = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let root = args.first().cloned().unwrap_or(JsValue::null());
            let what_to_show = args.get(1).cloned().unwrap_or(JsValue::from(0xFFFFFFFFu32));
            let filter = args.get(2).cloned().unwrap_or(JsValue::null());
            let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(false)
                    .enumerable(true)
                    .configurable(false)
                    .build()
            };
            let _ = obj.insert_property(boa_engine::js_string!("root"), pd2(root.clone()));
            let _ = obj.insert_property(boa_engine::js_string!("whatToShow"), pd2(what_to_show));
            let _ = obj.insert_property(boa_engine::js_string!("filter"), pd2(filter));
            let _ = obj.insert_property(
                boa_engine::js_string!("referenceNode"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(root)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            let null_fn = NativeFunction::from_copy_closure(|_t, _a, _c| Ok(JsValue::null()));
            for mname in &["nextNode", "previousNode", "detach"] {
                let _ = obj.insert_property(
                    boa_engine::js_string!(*mname),
                    pd2(JsValue::from(
                        boa_engine::object::FunctionObjectBuilder::new(
                            ctx.realm(),
                            null_fn.clone(),
                        )
                        .build(),
                    )),
                );
            }
            Ok(obj.into())
        });
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("createNodeIterator"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), create_ni).build(),
            )),
        );

        // document.adoptNode(node) — removes from parent, sets ownerDocument, returns node.
        let adopt_node = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let node = args.first().cloned().unwrap_or(JsValue::null());
            if let Some(node_obj) = node.as_object() {
                let pd2 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                // Remove from parent.
                let parent = node_obj
                    .get(boa_engine::js_string!("parentNode"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                if let Some(parent_obj) = parent {
                    remove_from_children(&parent_obj, &node_obj, ctx);
                }
                let _ = node_obj
                    .insert_property(boa_engine::js_string!("parentNode"), pd2(JsValue::null()));
                let _ = node_obj.insert_property(
                    boa_engine::js_string!("parentElement"),
                    pd2(JsValue::null()),
                );
                // Set ownerDocument to this document.
                let doc_val = ctx
                    .global_object()
                    .get(boa_engine::js_string!("document"), ctx)
                    .unwrap_or(JsValue::null());
                let _ =
                    node_obj.insert_property(boa_engine::js_string!("ownerDocument"), pd2(doc_val));
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
                // WheelEvent is NOT in the legacy createEvent table per DOM spec.
                // It should throw NOT_SUPPORTED_ERR.
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
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = obj.insert_property(
                boa_engine::js_string!("type"),
                pd(JsValue::from(boa_engine::js_string!(""))),
            );
            let _ =
                obj.insert_property(boa_engine::js_string!("bubbles"), pd(JsValue::from(false)));
            let _ = obj.insert_property(
                boa_engine::js_string!("cancelable"),
                pd(JsValue::from(false)),
            );
            let _ =
                obj.insert_property(boa_engine::js_string!("composed"), pd(JsValue::from(false)));
            let _ = obj.insert_property(
                boa_engine::js_string!("defaultPrevented"),
                pd(JsValue::from(false)),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("returnValue"),
                pd(JsValue::from(true)),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("isTrusted"),
                pd(JsValue::from(false)),
            );
            let _ = obj.insert_property(boa_engine::js_string!("target"), pd(JsValue::null()));
            let _ =
                obj.insert_property(boa_engine::js_string!("currentTarget"), pd(JsValue::null()));
            let _ = obj.insert_property(boa_engine::js_string!("srcElement"), pd(JsValue::null()));
            let _ =
                obj.insert_property(boa_engine::js_string!("timeStamp"), pd(JsValue::from(0u32)));
            let _ = obj.insert_property(
                boa_engine::js_string!("eventPhase"),
                pd(JsValue::from(0u32)),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("cancelBubble"),
                pd(JsValue::from(false)),
            );

            // Look up the constructor on the global object and set prototype.
            if let Ok(ctor_val) = ctx
                .global_object()
                .get(boa_engine::js_string!(iface_name), ctx)
            {
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
                            .value(val)
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build()
                    };
                    let t = arg_string(args, 0);
                    let b = args.get(1).and_then(|v| v.as_boolean()).unwrap_or(false);
                    let c = args.get(2).and_then(|v| v.as_boolean()).unwrap_or(false);
                    let _ = o.insert_property(
                        boa_engine::js_string!("type"),
                        pd(JsValue::from(boa_engine::js_string!(t))),
                    );
                    let _ =
                        o.insert_property(boa_engine::js_string!("bubbles"), pd(JsValue::from(b)));
                    let _ = o.insert_property(
                        boa_engine::js_string!("cancelable"),
                        pd(JsValue::from(c)),
                    );
                    let _ = o.insert_property(
                        boa_engine::js_string!("defaultPrevented"),
                        pd(JsValue::from(false)),
                    );
                    let _ = o.insert_property(
                        boa_engine::js_string!("returnValue"),
                        pd(JsValue::from(true)),
                    );
                    let _ = o.insert_property(
                        boa_engine::js_string!("isTrusted"),
                        pd(JsValue::from(false)),
                    );
                    let _ =
                        o.insert_property(boa_engine::js_string!("target"), pd(JsValue::null()));
                    let _ = o
                        .insert_property(boa_engine::js_string!("srcElement"), pd(JsValue::null()));
                }
                Ok(JsValue::undefined())
            });
            let _ = obj.insert_property(
                boa_engine::js_string!("initEvent"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), init_ev).build(),
                )),
            );
            // preventDefault with _passive check
            let pd_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    let cancelable = o
                        .get(boa_engine::js_string!("cancelable"), ctx)
                        .ok()
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(false);
                    let passive = o
                        .get(boa_engine::js_string!("_passive"), ctx)
                        .ok()
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(false);
                    if cancelable && !passive {
                        let pd2 = |val: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(val)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = o.insert_property(
                            boa_engine::js_string!("defaultPrevented"),
                            pd2(JsValue::from(true)),
                        );
                        let _ = o.insert_property(
                            boa_engine::js_string!("returnValue"),
                            pd2(JsValue::from(false)),
                        );
                    }
                }
                Ok(JsValue::undefined())
            });
            let _ = obj.insert_property(
                boa_engine::js_string!("preventDefault"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pd_fn).build(),
                )),
            );
            let stop_fn =
                NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
            let _ = obj.insert_property(
                boa_engine::js_string!("stopPropagation"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), stop_fn.clone())
                        .build(),
                )),
            );
            let _ = obj.insert_property(
                boa_engine::js_string!("stopImmediatePropagation"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), stop_fn).build(),
                )),
            );
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
                                .value(val)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let t = arg_string(args, 0);
                        let b = args.get(1).and_then(|v| v.as_boolean()).unwrap_or(false);
                        let c = args.get(2).and_then(|v| v.as_boolean()).unwrap_or(false);
                        let detail = args.get(3).cloned().unwrap_or(JsValue::null());
                        let _ = o.insert_property(
                            boa_engine::js_string!("type"),
                            pd2(JsValue::from(boa_engine::js_string!(t))),
                        );
                        let _ = o.insert_property(
                            boa_engine::js_string!("bubbles"),
                            pd2(JsValue::from(b)),
                        );
                        let _ = o.insert_property(
                            boa_engine::js_string!("cancelable"),
                            pd2(JsValue::from(c)),
                        );
                        let _ = o.insert_property(boa_engine::js_string!("detail"), pd2(detail));
                    }
                    Ok(JsValue::undefined())
                });
                let _ = obj.insert_property(
                    boa_engine::js_string!("initCustomEvent"),
                    pd(JsValue::from(
                        boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ice_fn).build(),
                    )),
                );
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
        let tc_noop_set =
            NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let tc_ng =
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_null_get).build();
        let tc_ns =
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tc_noop_set).build();
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("textContent"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(tc_ng.clone())
                .set(tc_ns.clone())
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        // Document.nodeValue is always null (setter is no-op).
        let _ = doc_obj.insert_property(
            boa_engine::js_string!("nodeValue"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(tc_ng)
                .set(tc_ns)
                .enumerable(true)
                .configurable(true)
                .build(),
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
        let _ =
            doc_obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(9u32)));
        let _ = doc_obj.insert_property(boa_engine::js_string!("textContent"), pd(JsValue::null()));
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
        let _ = doctype_obj
            .insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(10u32)));
        // nodeValue and textContent for DocumentType: always null (setter is no-op).
        let dt_null_get = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
        let dt_noop_set =
            NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let dt_ng =
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), dt_null_get).build();
        let dt_ns =
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), dt_noop_set).build();
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("textContent"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(dt_ng.clone())
                .set(dt_ns.clone())
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let _ = doctype_obj.insert_property(
            boa_engine::js_string!("nodeValue"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(dt_ng)
                .set(dt_ns)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let _ = doc_obj.insert_property(boa_engine::js_string!("doctype"), pd(doctype_obj.into()));
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
        ("MutationObserver", 1),
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
        let ctor_fn = NativeFunction::from_copy_closure_with_captures(
            move |this, _a, name_for_proto, ctx| {
                // Use `this` from Boa's OrdinaryCallConstruct (has correct prototype).
                let obj = this.as_object().unwrap_or_else(|| {
                    boa_engine::object::JsObject::with_object_proto(ctx.intrinsics())
                });
                // Explicitly set prototype to ConstructorName.prototype so instanceof works.
                // Boa's OrdinaryCallConstruct may not set the prototype correctly for native closures.
                let global = ctx.global_object();
                if let Ok(ctor_val) = global.get(
                    boa_engine::JsString::from(name_for_proto.as_str()),
                    &mut *ctx,
                ) {
                    if let Some(ctor_obj) = ctor_val.as_object() {
                        if let Ok(proto_val) =
                            ctor_obj.get(boa_engine::js_string!("prototype"), &mut *ctx)
                        {
                            if let Some(proto) = proto_val.as_object() {
                                let _ = obj.set_prototype(Some(proto));
                            }
                        }
                    }
                }
                let _ =
                    obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(nt)));
                // For Document constructor, add document-level methods.
                if name == "Document" {
                    let pd2 = |val: JsValue| {
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(val)
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build()
                    };
                    let _ = obj.insert_property(
                        boa_engine::js_string!("nodeName"),
                        pd2(JsValue::from(boa_engine::js_string!("#document"))),
                    );
                    let _ = obj
                        .insert_property(boa_engine::js_string!("nodeValue"), pd2(JsValue::null()));
                    let _ = obj.insert_property(
                        boa_engine::js_string!("textContent"),
                        pd2(JsValue::null()),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("contentType"),
                        pd2(JsValue::from(boa_engine::js_string!("application/xml"))),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("URL"),
                        pd2(JsValue::from(boa_engine::js_string!("about:blank"))),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("documentURI"),
                        pd2(JsValue::from(boa_engine::js_string!("about:blank"))),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("compatMode"),
                        pd2(JsValue::from(boa_engine::js_string!("CSS1Compat"))),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("characterSet"),
                        pd2(JsValue::from(boa_engine::js_string!("UTF-8"))),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("charset"),
                        pd2(JsValue::from(boa_engine::js_string!("UTF-8"))),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("inputEncoding"),
                        pd2(JsValue::from(boa_engine::js_string!("UTF-8"))),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("ownerDocument"),
                        pd2(JsValue::null()),
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("defaultView"),
                        pd2(ctx.global_object().into()),
                    );
                    // createElement — new Document() is XML, so no ASCII lowercasing, constructor is Element
                    let doc_ref = obj.clone();
                    let ce_fn = NativeFunction::from_copy_closure_with_captures(
                        move |_t, args, doc_ref, ctx| {
                            let tag = arg_to_string(args, 0, ctx);
                            let el =
                                boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                            let pd3 = |val: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(val)
                                    .writable(true)
                                    .enumerable(true)
                                    .configurable(true)
                                    .build()
                            };
                            let _ = el.insert_property(
                                boa_engine::js_string!("tagName"),
                                pd3(JsValue::from(boa_engine::js_string!(tag.clone()))),
                            );
                            let _ = el.insert_property(
                                boa_engine::js_string!("nodeName"),
                                pd3(JsValue::from(boa_engine::js_string!(tag.clone()))),
                            );
                            let _ = el.insert_property(
                                boa_engine::js_string!("localName"),
                                pd3(JsValue::from(boa_engine::js_string!(tag.clone()))),
                            );
                            let _ = el.insert_property(
                                boa_engine::js_string!("nodeType"),
                                pd3(JsValue::from(1u32)),
                            );
                            let _ = el.insert_property(
                                boa_engine::js_string!("nodeValue"),
                                pd3(JsValue::null()),
                            );
                            let _ = el.insert_property(
                                boa_engine::js_string!("ownerDocument"),
                                pd3(JsValue::from(doc_ref.clone())),
                            );
                            let _ = el.insert_property(
                                boa_engine::js_string!("namespaceURI"),
                                pd3(JsValue::null()),
                            );
                            let _ = el.insert_property(
                                boa_engine::js_string!("prefix"),
                                pd3(JsValue::null()),
                            );
                            // Set prototype to Element.prototype so constructor === Element
                            let global = ctx.global_object();
                            if let Ok(elem_ctor_val) =
                                global.get(boa_engine::js_string!("Element"), &mut *ctx)
                            {
                                if let Some(elem_ctor) = elem_ctor_val.as_object() {
                                    if let Ok(elem_proto_val) = elem_ctor
                                        .get(boa_engine::js_string!("prototype"), &mut *ctx)
                                    {
                                        if let Some(elem_proto) = elem_proto_val.as_object() {
                                            let _ = el.set_prototype(Some(elem_proto));
                                        }
                                    }
                                }
                            }
                            add_dom_methods(&el, ctx);
                            Ok(el.into())
                        },
                        doc_ref,
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("createElement"),
                        pd2(JsValue::from(
                            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ce_fn)
                                .build(),
                        )),
                    );
                    // createTextNode
                    let doc_ref2 = obj.clone();
                    let ct_fn = NativeFunction::from_copy_closure_with_captures(
                        move |_t, args, doc_ref2, ctx| {
                            let text = arg_string(args, 0);
                            let tn =
                                boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                            let pd3 = |val: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(val)
                                    .writable(true)
                                    .enumerable(true)
                                    .configurable(true)
                                    .build()
                            };
                            let _ = tn.insert_property(
                                boa_engine::js_string!("nodeType"),
                                pd3(JsValue::from(3u32)),
                            );
                            let _ = tn.insert_property(
                                boa_engine::js_string!("_data"),
                                pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                            );
                            let _ = tn.insert_property(
                                boa_engine::js_string!("data"),
                                pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                            );
                            let _ = tn.insert_property(
                                boa_engine::js_string!("nodeValue"),
                                pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                            );
                            let _ = tn.insert_property(
                                boa_engine::js_string!("textContent"),
                                pd3(JsValue::from(boa_engine::js_string!(text))),
                            );
                            let _ = tn.insert_property(
                                boa_engine::js_string!("nodeName"),
                                pd3(JsValue::from(boa_engine::js_string!("#text"))),
                            );
                            let _ = tn.insert_property(
                                boa_engine::js_string!("ownerDocument"),
                                pd3(JsValue::from(doc_ref2.clone())),
                            );
                            Ok(tn.into())
                        },
                        doc_ref2,
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("createTextNode"),
                        pd2(JsValue::from(
                            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ct_fn)
                                .build(),
                        )),
                    );
                    // createComment
                    let doc_ref3 = obj.clone();
                    let cc_fn = NativeFunction::from_copy_closure_with_captures(
                        move |_t, args, doc_ref3, ctx| {
                            let text = arg_string(args, 0);
                            let cn =
                                boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                            let pd3 = |val: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(val)
                                    .writable(true)
                                    .enumerable(true)
                                    .configurable(true)
                                    .build()
                            };
                            let _ = cn.insert_property(
                                boa_engine::js_string!("nodeType"),
                                pd3(JsValue::from(8u32)),
                            );
                            let _ = cn.insert_property(
                                boa_engine::js_string!("_data"),
                                pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                            );
                            let _ = cn.insert_property(
                                boa_engine::js_string!("data"),
                                pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                            );
                            let _ = cn.insert_property(
                                boa_engine::js_string!("nodeValue"),
                                pd3(JsValue::from(boa_engine::js_string!(text.clone()))),
                            );
                            let _ = cn.insert_property(
                                boa_engine::js_string!("textContent"),
                                pd3(JsValue::from(boa_engine::js_string!(text))),
                            );
                            let _ = cn.insert_property(
                                boa_engine::js_string!("nodeName"),
                                pd3(JsValue::from(boa_engine::js_string!("#comment"))),
                            );
                            let _ = cn.insert_property(
                                boa_engine::js_string!("ownerDocument"),
                                pd3(JsValue::from(doc_ref3.clone())),
                            );
                            Ok(cn.into())
                        },
                        doc_ref3,
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("createComment"),
                        pd2(JsValue::from(
                            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cc_fn)
                                .build(),
                        )),
                    );
                    // createCDATASection
                    let doc_ref4 = obj.clone();
                    let cd_fn = NativeFunction::from_copy_closure_with_captures(
                        move |_t, args, doc_ref4, ctx| {
                            let data = arg_string(args, 0);
                            let cd =
                                boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                            let pd3 = |val: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(val)
                                    .writable(true)
                                    .enumerable(true)
                                    .configurable(true)
                                    .build()
                            };
                            let _ = cd.insert_property(
                                boa_engine::js_string!("nodeType"),
                                pd3(JsValue::from(4u32)),
                            );
                            let _ = cd.insert_property(
                                boa_engine::js_string!("_data"),
                                pd3(JsValue::from(boa_engine::js_string!(data.clone()))),
                            );
                            let _ = cd.insert_property(
                                boa_engine::js_string!("data"),
                                pd3(JsValue::from(boa_engine::js_string!(data.clone()))),
                            );
                            let _ = cd.insert_property(
                                boa_engine::js_string!("nodeValue"),
                                pd3(JsValue::from(boa_engine::js_string!(data.clone()))),
                            );
                            let _ = cd.insert_property(
                                boa_engine::js_string!("textContent"),
                                pd3(JsValue::from(boa_engine::js_string!(data))),
                            );
                            let _ = cd.insert_property(
                                boa_engine::js_string!("nodeName"),
                                pd3(JsValue::from(boa_engine::js_string!("#cdata-section"))),
                            );
                            let _ = cd.insert_property(
                                boa_engine::js_string!("ownerDocument"),
                                pd3(JsValue::from(doc_ref4.clone())),
                            );
                            Ok(cd.into())
                        },
                        doc_ref4,
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("createCDATASection"),
                        pd2(JsValue::from(
                            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cd_fn)
                                .build(),
                        )),
                    );
                    // createRange
                    let doc_ref5 = obj.clone();
                    let cr_fn = NativeFunction::from_copy_closure_with_captures(
                        move |_t, _a, doc_ref5, _ctx| {
                            let pd3 = |val: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(val)
                                    .writable(true)
                                    .enumerable(true)
                                    .configurable(true)
                                    .build()
                            };
                            let r =
                                boa_engine::object::JsObject::with_object_proto(_ctx.intrinsics());
                            let _ = r.insert_property(
                                boa_engine::js_string!("startContainer"),
                                pd3(JsValue::from(doc_ref5.clone())),
                            );
                            let _ = r.insert_property(
                                boa_engine::js_string!("endContainer"),
                                pd3(JsValue::from(doc_ref5.clone())),
                            );
                            let _ = r.insert_property(
                                boa_engine::js_string!("startOffset"),
                                pd3(JsValue::from(0u32)),
                            );
                            let _ = r.insert_property(
                                boa_engine::js_string!("endOffset"),
                                pd3(JsValue::from(0u32)),
                            );
                            let _ = r.insert_property(
                                boa_engine::js_string!("collapsed"),
                                pd3(JsValue::from(true)),
                            );
                            let _ = r.insert_property(
                                boa_engine::js_string!("commonAncestorContainer"),
                                pd3(JsValue::from(doc_ref5.clone())),
                            );
                            Ok(r.into())
                        },
                        doc_ref5,
                    );
                    let _ = obj.insert_property(
                        boa_engine::js_string!("createRange"),
                        pd2(JsValue::from(
                            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cr_fn)
                                .build(),
                        )),
                    );
                }
                // Add empty childNodes/firstChild/lastChild for Document constructor.
                let empty_arr = new_array(ctx);
                let pd_doc = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ = obj.insert_property(
                    boa_engine::js_string!("childNodes"),
                    pd_doc(empty_arr.into()),
                );
                let _ = obj.insert_property(
                    boa_engine::js_string!("firstChild"),
                    pd_doc(JsValue::null()),
                );
                let _ = obj
                    .insert_property(boa_engine::js_string!("lastChild"), pd_doc(JsValue::null()));
                let _ =
                    obj.insert_property(boa_engine::js_string!("doctype"), pd_doc(JsValue::null()));
                let _ = obj.insert_property(
                    boa_engine::js_string!("documentElement"),
                    pd_doc(JsValue::null()),
                );
                let _ = obj
                    .insert_property(boa_engine::js_string!("location"), pd_doc(JsValue::null()));
                let _ =
                    obj.insert_property(boa_engine::js_string!("head"), pd_doc(JsValue::null()));
                let _ =
                    obj.insert_property(boa_engine::js_string!("body"), pd_doc(JsValue::null()));
                Ok(obj.into())
            },
            name_for_proto,
        );
        let _ = ctx.register_global_callable(boa_engine::js_string!(name), 0, ctor_fn);

        // Use the .prototype already created by register_global_callable.
        // Just add constructor back-reference if not present.
        let global = ctx.global_object();
        if let Ok(ctor_val) = global.get(boa_engine::js_string!(name), &mut *ctx) {
            if let Some(ctor_obj) = ctor_val.as_object() {
                if let Ok(proto_val) = ctor_obj.get(boa_engine::js_string!("prototype"), &mut *ctx)
                {
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
                .value(val)
                .writable(false)
                .enumerable(false)
                .configurable(false)
                .build()
        };
        let _ = node_proto.insert_property(
            boa_engine::js_string!("ELEMENT_NODE"),
            pd(JsValue::from(1u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("ATTRIBUTE_NODE"),
            pd(JsValue::from(2u32)),
        );
        let _ = node_proto
            .insert_property(boa_engine::js_string!("TEXT_NODE"), pd(JsValue::from(3u32)));
        let _ = node_proto.insert_property(
            boa_engine::js_string!("CDATA_SECTION_NODE"),
            pd(JsValue::from(4u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("ENTITY_REFERENCE_NODE"),
            pd(JsValue::from(5u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("ENTITY_NODE"),
            pd(JsValue::from(6u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("PROCESSING_INSTRUCTION_NODE"),
            pd(JsValue::from(7u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("COMMENT_NODE"),
            pd(JsValue::from(8u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("DOCUMENT_NODE"),
            pd(JsValue::from(9u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("DOCUMENT_TYPE_NODE"),
            pd(JsValue::from(10u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("DOCUMENT_FRAGMENT_NODE"),
            pd(JsValue::from(11u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("NOTATION_NODE"),
            pd(JsValue::from(12u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("DOCUMENT_POSITION_CONTAINED_BY"),
            pd(JsValue::from(0x10u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("DOCUMENT_POSITION_CONTAINS"),
            pd(JsValue::from(0x08u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("DOCUMENT_POSITION_PRECEDING"),
            pd(JsValue::from(0x02u32)),
        );
        let _ = node_proto.insert_property(
            boa_engine::js_string!("DOCUMENT_POSITION_FOLLOWING"),
            pd(JsValue::from(0x04u32)),
        );

        // Also add constants to the Node constructor itself (Node.ELEMENT_NODE).
        if let Ok(node_ctor_val) = ctx.global_object().get(boa_engine::js_string!("Node"), ctx) {
            if let Some(node_ctor) = node_ctor_val.as_object() {
                for (name, val) in &[
                    ("ELEMENT_NODE", 1u32),
                    ("ATTRIBUTE_NODE", 2u32),
                    ("TEXT_NODE", 3u32),
                    ("CDATA_SECTION_NODE", 4u32),
                    ("ENTITY_REFERENCE_NODE", 5u32),
                    ("ENTITY_NODE", 6u32),
                    ("PROCESSING_INSTRUCTION_NODE", 7u32),
                    ("COMMENT_NODE", 8u32),
                    ("DOCUMENT_NODE", 9u32),
                    ("DOCUMENT_TYPE_NODE", 10u32),
                    ("DOCUMENT_FRAGMENT_NODE", 11u32),
                    ("NOTATION_NODE", 12u32),
                    ("DOCUMENT_POSITION_DISCONNECTED", 0x01u32),
                    ("DOCUMENT_POSITION_PRECEDING", 0x02u32),
                    ("DOCUMENT_POSITION_FOLLOWING", 0x04u32),
                    ("DOCUMENT_POSITION_CONTAINS", 0x08u32),
                    ("DOCUMENT_POSITION_CONTAINED_BY", 0x10u32),
                    ("DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC", 0x20u32),
                ] {
                    let _ = node_ctor
                        .insert_property(boa_engine::js_string!(*name), pd(JsValue::from(*val)));
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
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );

        // Add isEqualNode to Node.prototype.
        let ien_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let other = args.first().cloned().unwrap_or(JsValue::null());
            if other.is_null() || other.is_undefined() {
                return Ok(JsValue::from(false));
            }
            if JsValue::same_value(this, &other) {
                return Ok(JsValue::from(true));
            }
            let this_obj = this.as_object();
            let other_obj = other.as_object();
            if this_obj.is_none() || other_obj.is_none() {
                return Ok(JsValue::from(false));
            }
            let o1 = this_obj.unwrap();
            let o2 = other_obj.unwrap();
            // Compare nodeType.
            let nt1 = o1
                .get(boa_engine::js_string!("nodeType"), ctx)
                .ok()
                .and_then(|v| v.as_number())
                .unwrap_or(-1.0);
            let nt2 = o2
                .get(boa_engine::js_string!("nodeType"), ctx)
                .ok()
                .and_then(|v| v.as_number())
                .unwrap_or(-1.0);
            if nt1 != nt2 {
                return Ok(JsValue::from(false));
            }
            // For Element: compare tagName, attributes.
            if nt1 == 1.0 {
                let tn1 = o1
                    .get(boa_engine::js_string!("tagName"), ctx)
                    .ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                let tn2 = o2
                    .get(boa_engine::js_string!("tagName"), ctx)
                    .ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                if tn1 != tn2 {
                    return Ok(JsValue::from(false));
                }
            }
            // For CharacterData (Text/Comment/PI): compare data.
            if nt1 == 3.0 || nt1 == 4.0 || nt1 == 8.0 || nt1 == 7.0 {
                let d1 = o1.get(boa_engine::js_string!("data"), ctx).ok();
                let d2 = o2.get(boa_engine::js_string!("data"), ctx).ok();
                if d1 != d2 {
                    return Ok(JsValue::from(false));
                }
            }
            // For DocumentType: compare name, publicId, systemId.
            if nt1 == 10.0 {
                for p in &["name", "publicId", "systemId"] {
                    let v1 = o1.get(boa_engine::js_string!(*p), ctx).ok();
                    let v2 = o2.get(boa_engine::js_string!(*p), ctx).ok();
                    if v1 != v2 {
                        return Ok(JsValue::from(false));
                    }
                }
            }
            // For Attr: compare name, value, namespaceURI.
            if nt1 == 2.0 {
                for p in &["name", "value", "namespaceURI"] {
                    let v1 = o1.get(boa_engine::js_string!(*p), ctx).ok();
                    let v2 = o2.get(boa_engine::js_string!(*p), ctx).ok();
                    if v1 != v2 {
                        return Ok(JsValue::from(false));
                    }
                }
            }
            // Compare childNodes count.
            let c1 = o1.get(boa_engine::js_string!("childNodes"), ctx).ok();
            let c2 = o2.get(boa_engine::js_string!("childNodes"), ctx).ok();
            let len1 = c1
                .as_ref()
                .and_then(|v| v.as_object())
                .and_then(|a| a.get(boa_engine::js_string!("length"), ctx).ok())
                .and_then(|v| v.as_number())
                .unwrap_or(0.0);
            let len2 = c2
                .as_ref()
                .and_then(|v| v.as_object())
                .and_then(|a| a.get(boa_engine::js_string!("length"), ctx).ok())
                .and_then(|v| v.as_number())
                .unwrap_or(0.0);
            if len1 != len2 {
                return Ok(JsValue::from(false));
            }
            Ok(JsValue::from(true))
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("isEqualNode"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ien_fn).build())
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
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
                .value(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), contains_fn)
                        .build(),
                )
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
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
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );

        // Add hasChildNodes to Node.prototype.
        let hcn_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
            if let Some(o) = this.as_object() {
                if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                    if let Some(ca) = cv.as_object() {
                        let len = ca
                            .get(boa_engine::js_string!("length"), ctx)
                            .ok()
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0);
                        return Ok(JsValue::from(len > 0.0));
                    }
                }
                if let Ok(cn) = o.get(boa_engine::js_string!("childNodes"), ctx) {
                    if let Some(ca) = cn.as_object() {
                        let len = ca
                            .get(boa_engine::js_string!("length"), ctx)
                            .ok()
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0);
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
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );

        // Add parentElement getter to Node.prototype — returns parent if it's an Element, else null.
        let pe_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
            if let Some(o) = this.as_object() {
                if let Ok(pn) = o.get(boa_engine::js_string!("parentNode"), ctx) {
                    if let Some(p) = pn.as_object() {
                        if let Ok(nt) = p.get(boa_engine::js_string!("nodeType"), ctx) {
                            if nt.as_number().map(|n| n as u32 == 1).unwrap_or(false) {
                                return Ok(JsValue::from(p.clone()));
                            }
                        }
                    }
                }
            }
            Ok(JsValue::null())
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("parentElement"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pe_get).build())
                .enumerable(true)
                .configurable(true)
                .build(),
        );

        // Add normalize to Node.prototype — merges adjacent text nodes.
        let normalize_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
            if let Some(o) = this.as_object() {
                if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
                    if let Some(ca) = cv.as_object() {
                        let len = ca
                            .get(boa_engine::js_string!("length"), ctx)
                            .ok()
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0) as u32;
                        // Merge adjacent text nodes.
                        let pd = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let mut i = 0u32;
                        let mut new_children: Vec<JsValue> = Vec::new();
                        while i < len {
                            if let Ok(child) = ca.get(i, ctx) {
                                if let Some(co) = child.as_object() {
                                    let nt = co
                                        .get(boa_engine::js_string!("nodeType"), ctx)
                                        .ok()
                                        .and_then(|v| v.as_number())
                                        .unwrap_or(0.0)
                                        as u32;
                                    if nt == 3 {
                                        // Text node — merge with following text nodes.
                                        let mut combined = read_data_utf16(&child, ctx);
                                        let mut j = i + 1;
                                        while j < len {
                                            if let Ok(next) = ca.get(j, ctx) {
                                                if let Some(no) = next.as_object() {
                                                    let nnt = no
                                                        .get(
                                                            boa_engine::js_string!("nodeType"),
                                                            ctx,
                                                        )
                                                        .ok()
                                                        .and_then(|v| v.as_number())
                                                        .unwrap_or(0.0)
                                                        as u32;
                                                    if nnt == 3 {
                                                        combined
                                                            .extend(read_data_utf16(&next, ctx));
                                                        j += 1;
                                                    } else {
                                                        break;
                                                    }
                                                } else {
                                                    break;
                                                }
                                            } else {
                                                break;
                                            }
                                        }
                                        if j > i + 1 {
                                            // Create merged text node.
                                            let tn =
                                                boa_engine::object::JsObject::with_object_proto(
                                                    ctx.intrinsics(),
                                                );
                                            let _ = tn.insert_property(
                                                boa_engine::js_string!("nodeType"),
                                                pd(JsValue::from(3u32)),
                                            );
                                            let js_str = boa_engine::JsString::from(&combined[..]);
                                            let _ = tn.insert_property(
                                                boa_engine::js_string!("_data"),
                                                pd(JsValue::from(js_str.clone())),
                                            );
                                            let _ = tn.insert_property(
                                                boa_engine::js_string!("data"),
                                                pd(JsValue::from(js_str.clone())),
                                            );
                                            let _ = tn.insert_property(
                                                boa_engine::js_string!("nodeValue"),
                                                pd(JsValue::from(js_str.clone())),
                                            );
                                            let _ = tn.insert_property(
                                                boa_engine::js_string!("textContent"),
                                                pd(JsValue::from(js_str)),
                                            );
                                            let _ = tn.insert_property(
                                                boa_engine::js_string!("nodeName"),
                                                pd(JsValue::from(boa_engine::js_string!("#text"))),
                                            );
                                            let _ = tn.insert_property(
                                                boa_engine::js_string!("parentNode"),
                                                pd(JsValue::from(o.clone())),
                                            );
                                            if let Ok(tc) = ctx
                                                .global_object()
                                                .get(boa_engine::js_string!("Text"), ctx)
                                            {
                                                if let Some(tco) = tc.as_object() {
                                                    if let Ok(pv) = tco.get(
                                                        boa_engine::js_string!("prototype"),
                                                        ctx,
                                                    ) {
                                                        if let Some(p) = pv.as_object() {
                                                            let _ = tn.set_prototype(Some(p));
                                                        }
                                                    }
                                                }
                                            }
                                            new_children.push(tn.into());
                                            i = j;
                                            continue;
                                        }
                                    }
                                    new_children.push(child);
                                    i += 1;
                                } else {
                                    i += 1;
                                }
                            } else {
                                i += 1;
                            }
                        }
                        // Update _children if changes were made.
                        if new_children.len() as u32 != len {
                            let new_arr = new_array(ctx);
                            for child in new_children {
                                let _ = new_arr.push(child, ctx);
                            }
                            let _ = o.insert_property(
                                boa_engine::js_string!("_children"),
                                pd(new_arr.into()),
                            );
                        }
                    }
                }
            }
            Ok(JsValue::undefined())
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("normalize"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), normalize_fn)
                        .build(),
                )
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );

        // Add cloneNode to Node.prototype — always uses clone_node_js directly
        // (avoids infinite recursion when looking up own cloneNode property).
        let clone_node_proto_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let deep = args.first().and_then(|v| v.as_boolean()).unwrap_or(false);
            if let Some(o) = this.as_object() {
                return clone_node_js(Some(&o), deep, ctx);
            }
            Ok(JsValue::null())
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("cloneNode"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(
                    boa_engine::object::FunctionObjectBuilder::new(
                        ctx.realm(),
                        clone_node_proto_fn,
                    )
                    .build(),
                )
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );

        // Add lookupNamespaceURI, lookupPrefix, isDefaultNamespace to Node.prototype.
        let lookup_ns_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let _prefix = args
                .first()
                .map(|v| {
                    if v.is_null() || v.is_undefined() {
                        None
                    } else {
                        v.as_string().map(|s| s.to_std_string_escaped())
                    }
                })
                .flatten();
            if let Some(o) = this.as_object() {
                if let Ok(ns) = o.get(boa_engine::js_string!("namespaceURI"), ctx) {
                    if let Some(ns_s) = ns.as_string() {
                        let ns_str = ns_s.to_std_string_escaped();
                        if !ns_str.is_empty() {
                            // Walk up parentNode chain looking for namespace definition.
                            let mut current: Option<boa_engine::JsObject> = Some(o.clone());
                            for _ in 0..1000 {
                                if let Some(c) = current {
                                    if let Ok(pn) = c.get(boa_engine::js_string!("parentNode"), ctx)
                                    {
                                        if let Some(p) = pn.as_object() {
                                            current = Some(p.clone());
                                        } else if pn.is_null() {
                                            break;
                                        } else {
                                            current = None;
                                        }
                                    } else {
                                        current = None;
                                    }
                                } else {
                                    break;
                                }
                            }
                            return Ok(JsValue::null());
                        }
                    }
                }
            }
            Ok(JsValue::null())
        });
        let _ = node_proto.insert_property(
            boa_engine::js_string!("lookupNamespaceURI"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(
                    boa_engine::object::FunctionObjectBuilder::new(
                        ctx.realm(),
                        lookup_ns_fn.clone(),
                    )
                    .build(),
                )
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let lookup_prefix_fn =
            NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
        let _ = node_proto.insert_property(
            boa_engine::js_string!("lookupPrefix"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(
                    boa_engine::object::FunctionObjectBuilder::new(
                        ctx.realm(),
                        lookup_prefix_fn.clone(),
                    )
                    .build(),
                )
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
        );
        let is_def_ns_fn =
            NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::from(true)));
        let _ = node_proto.insert_property(
            boa_engine::js_string!("isDefaultNamespace"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), is_def_ns_fn)
                        .build(),
                )
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build(),
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
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    };
    let tag_pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(false)
            .enumerable(false)
            .configurable(true)
            .build()
    };
    if let Some(sym) = to_string_tag_sym {
        for (name, tag) in &[
            ("TreeWalker", "TreeWalker"),
            ("NodeIterator", "NodeIterator"),
            ("NodeList", "NodeList"),
            ("HTMLCollection", "HTMLCollection"),
            ("Range", "Range"),
            ("Document", "Document"),
            ("DocumentFragment", "DocumentFragment"),
            ("DocumentType", "DocumentType"),
            ("Element", "Element"),
            ("Text", "Text"),
            ("Comment", "Comment"),
            ("Attr", "Attr"),
            ("Event", "Event"),
            ("Node", "Node"),
        ] {
            if let Some(proto) = get_proto(name, ctx) {
                let _ = proto.insert_property(
                    sym.clone(),
                    tag_pd(JsValue::from(boa_engine::js_string!(*tag))),
                );
            }
        }
    }

    // Add caniuse event properties to event prototypes (KeyboardEvent, MouseEvent, FocusEvent).
    let ev_pd = |v: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(v)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let ev_false = ev_pd(JsValue::from(false));
    let ev_zero = ev_pd(JsValue::from(0u32));
    let ev_null = ev_pd(JsValue::null());
    let ev_empty = ev_pd(JsValue::from(boa_engine::js_string!("")));
    // KeyboardEvent
    if let Some(proto) = get_proto("KeyboardEvent", ctx) {
        for (prop, val) in &[
            ("key", ev_empty.clone()),
            ("code", ev_empty.clone()),
            ("location", ev_zero.clone()),
            ("ctrlKey", ev_false.clone()),
            ("shiftKey", ev_false.clone()),
            ("altKey", ev_false.clone()),
            ("metaKey", ev_false.clone()),
            ("repeat", ev_false.clone()),
            ("isComposing", ev_false.clone()),
            ("charCode", ev_zero.clone()),
            ("keyCode", ev_zero.clone()),
            ("which", ev_zero.clone()),
            ("detail", ev_zero.clone()),
        ] {
            let _ = proto.insert_property(boa_engine::js_string!(*prop), val.clone());
        }
    }
    // MouseEvent
    if let Some(proto) = get_proto("MouseEvent", ctx) {
        for (prop, val) in &[
            ("screenX", ev_zero.clone()),
            ("screenY", ev_zero.clone()),
            ("clientX", ev_zero.clone()),
            ("clientY", ev_zero.clone()),
            ("ctrlKey", ev_false.clone()),
            ("shiftKey", ev_false.clone()),
            ("altKey", ev_false.clone()),
            ("metaKey", ev_false.clone()),
            ("button", ev_zero.clone()),
            ("buttons", ev_zero.clone()),
            ("relatedTarget", ev_null.clone()),
            ("movementX", ev_zero.clone()),
            ("movementY", ev_zero.clone()),
            ("detail", ev_zero.clone()),
        ] {
            let _ = proto.insert_property(boa_engine::js_string!(*prop), val.clone());
        }
    }
    // FocusEvent
    if let Some(proto) = get_proto("FocusEvent", ctx) {
        let _ = proto.insert_property(boa_engine::js_string!("relatedTarget"), ev_null.clone());
    }
    // UIEvent
    if let Some(proto) = get_proto("UIEvent", ctx) {
        let _ = proto.insert_property(boa_engine::js_string!("detail"), ev_zero.clone());
        let _ = proto.insert_property(boa_engine::js_string!("view"), ev_null.clone());
    }
    // WheelEvent (non-standard, widely used)
    if let Some(proto) = get_proto("WheelEvent", ctx) {
        let _ = proto.insert_property(boa_engine::js_string!("deltaX"), ev_zero.clone());
        let _ = proto.insert_property(boa_engine::js_string!("deltaY"), ev_zero.clone());
        let _ = proto.insert_property(boa_engine::js_string!("deltaZ"), ev_zero.clone());
        let _ = proto.insert_property(boa_engine::js_string!("deltaMode"), ev_zero.clone());
    }
    // PointerEvent (caniuse 97%+)
    if let Some(proto) = get_proto("PointerEvent", ctx) {
        for (prop, val) in &[
            ("pointerId", ev_zero.clone()),
            ("width", ev_zero.clone()),
            ("height", ev_zero.clone()),
            ("pressure", ev_pd(JsValue::from(0.0f64))),
            ("tiltX", ev_zero.clone()),
            ("tiltY", ev_zero.clone()),
            ("pointerType", ev_empty.clone()),
            ("isPrimary", ev_false.clone()),
        ] {
            let _ = proto.insert_property(boa_engine::js_string!(*prop), val.clone());
        }
    }
    // ClipboardEvent (caniuse 96%+)
    if let Some(proto) = get_proto("ClipboardEvent", ctx) {
        let _ = proto.insert_property(boa_engine::js_string!("clipboardData"), ev_null.clone());
    }
    // ErrorEvent (caniuse 97%+)
    if let Some(proto) = get_proto("ErrorEvent", ctx) {
        let _ = proto.insert_property(boa_engine::js_string!("message"), ev_empty.clone());
        let _ = proto.insert_property(boa_engine::js_string!("filename"), ev_empty.clone());
        let _ = proto.insert_property(boa_engine::js_string!("lineno"), ev_zero.clone());
        let _ = proto.insert_property(boa_engine::js_string!("colno"), ev_zero.clone());
        let _ = proto.insert_property(boa_engine::js_string!("error"), ev_null.clone());
    }
    // DragEvent (caniuse 97%+)
    if let Some(proto) = get_proto("DragEvent", ctx) {
        let _ = proto.insert_property(boa_engine::js_string!("dataTransfer"), ev_null.clone());
        let _ = proto.insert_property(boa_engine::js_string!("clientX"), ev_zero.clone());
        let _ = proto.insert_property(boa_engine::js_string!("clientY"), ev_zero);
    }
    // GamepadEvent (caniuse 97%+)
    if let Some(proto) = get_proto("GamepadEvent", ctx) {
        let _ = proto.insert_property(boa_engine::js_string!("gamepad"), ev_null.clone());
    }
    // InputEvent (caniuse 97%+) — Chromium non-standard
    if let Some(proto) = get_proto("InputEvent", ctx) {
        let _ = proto.insert_property(boa_engine::js_string!("data"), ev_null.clone());
        let _ = proto.insert_property(boa_engine::js_string!("dataTransfer"), ev_null.clone());
        let _ = proto.insert_property(boa_engine::js_string!("inputType"), ev_empty.clone());
        let _ = proto.insert_property(boa_engine::js_string!("isComposing"), ev_false);
    }
    // TouchEvent (caniuse 98%+) — touch support
    if let Some(proto) = get_proto("TouchEvent", ctx) {
        let _ = proto.insert_property(
            boa_engine::js_string!("touches"),
            ev_pd(JsValue::from(new_array(ctx))),
        );
        let _ = proto.insert_property(
            boa_engine::js_string!("targetTouches"),
            ev_pd(JsValue::from(new_array(ctx))),
        );
        let _ = proto.insert_property(
            boa_engine::js_string!("changedTouches"),
            ev_pd(JsValue::from(new_array(ctx))),
        );
        let _ = proto.insert_property(
            boa_engine::js_string!("ctrlKey"),
            ev_pd(JsValue::from(false)),
        );
        let _ = proto.insert_property(
            boa_engine::js_string!("shiftKey"),
            ev_pd(JsValue::from(false)),
        );
        let _ = proto.insert_property(
            boa_engine::js_string!("altKey"),
            ev_pd(JsValue::from(false)),
        );
        let _ = proto.insert_property(
            boa_engine::js_string!("metaKey"),
            ev_pd(JsValue::from(false)),
        );
    }
    // CompositionEvent (caniuse 97%+)
    if let Some(proto) = get_proto("CompositionEvent", ctx) {
        let _ = proto.insert_property(
            boa_engine::js_string!("data"),
            ev_pd(JsValue::from(boa_engine::js_string!(""))),
        );
    }

    // Add iterable methods to NodeList.prototype (values, entries, forEach, Symbol.iterator).
    if let Some(nl_proto) = get_proto("NodeList", ctx) {
        let fn_pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        // item(index) method.
        let nl_item = NativeFunction::from_copy_closure(|this, args, ctx| {
            let idx = args.first().and_then(|v| v.as_number()).unwrap_or(-1.0) as i32;
            if idx < 0 {
                return Ok(JsValue::null());
            }
            if let Some(o) = this.as_object() {
                let len = o
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                if (idx as u32) < len {
                    return Ok(o.get(idx as u32, ctx).unwrap_or(JsValue::null()));
                }
            }
            Ok(JsValue::null())
        });
        let _ = nl_proto.insert_property(
            boa_engine::js_string!("item"),
            fn_pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nl_item).build(),
            )),
        );
        // forEach(callback) method.
        let nl_foreach = NativeFunction::from_copy_closure(|this, args, ctx| {
            let cb = args.first().cloned().unwrap_or(JsValue::undefined());
            let this_arg = args.get(1).cloned().unwrap_or(JsValue::undefined());
            if let (Some(o), Some(cb_fn)) = (this.as_object(), cb.as_object()) {
                if cb_fn.is_callable() {
                    let len = o
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    for i in 0..len {
                        let val = o.get(i, ctx).unwrap_or(JsValue::undefined());
                        let _ = cb_fn.call(&this_arg, &[val, JsValue::from(i), this.clone()], ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        });
        let _ = nl_proto.insert_property(
            boa_engine::js_string!("forEach"),
            fn_pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nl_foreach).build(),
            )),
        );
        // values() returns an array of values.
        let nl_values = NativeFunction::from_copy_closure(|this, _args, ctx| {
            let arr = new_array(ctx);
            if let Some(o) = this.as_object() {
                let len = o
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                for i in 0..len {
                    let val = o.get(i, ctx).unwrap_or(JsValue::undefined());
                    let _ = arr.push(val, ctx);
                }
            }
            // Make it an iterator.
            let iter_pd = boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(
                        ctx.realm(),
                        NativeFunction::from_copy_closure(|this, _a, _ctx| Ok(this.clone())),
                    )
                    .build(),
                ))
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build();
            let _ = arr.insert_property(
                boa_engine::symbol::JsSymbol::new(Some(boa_engine::JsString::from(
                    "Symbol(Symbol.iterator)",
                )))
                .unwrap(),
                iter_pd,
            );
            Ok(arr.into())
        });
        let _ = nl_proto.insert_property(
            boa_engine::js_string!("values"),
            fn_pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nl_values.clone())
                    .build(),
            )),
        );
        let _ = nl_proto.insert_property(
            boa_engine::js_string!("keys"),
            fn_pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nl_values).build(),
            )),
        );
        // entries() returns array of [key, value] pairs.
        let nl_entries = NativeFunction::from_copy_closure(|this, _args, ctx| {
            let arr = new_array(ctx);
            if let Some(o) = this.as_object() {
                let len = o
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                for i in 0..len {
                    let val = o.get(i, ctx).unwrap_or(JsValue::undefined());
                    let pair = new_array(ctx);
                    let _ = pair.push(JsValue::from(i), ctx);
                    let _ = pair.push(val, ctx);
                    let _ = arr.push(JsValue::from(pair), ctx);
                }
            }
            Ok(arr.into())
        });
        let _ = nl_proto.insert_property(
            boa_engine::js_string!("entries"),
            fn_pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), nl_entries).build(),
            )),
        );
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
        for name in &[
            "Document",
            "DocumentFragment",
            "Attr",
            "DocumentType",
            "ProcessingInstruction",
        ] {
            if let Some(p) = get_proto(name, ctx) {
                let _ = p.set_prototype(Some(node_proto.clone()));
            }
        }
    }

    // Add NodeFilter constants to the NodeFilter constructor.
    let pd_const = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(false)
            .enumerable(false)
            .configurable(false)
            .build()
    };
    if let Ok(nf_val) = ctx
        .global_object()
        .get(boa_engine::js_string!("NodeFilter"), ctx)
    {
        if let Some(nf) = nf_val.as_object() {
            let _ = nf.insert_property(
                boa_engine::js_string!("FILTER_ACCEPT"),
                pd_const(JsValue::from(1u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("FILTER_REJECT"),
                pd_const(JsValue::from(2u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("FILTER_SKIP"),
                pd_const(JsValue::from(3u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_ALL"),
                pd_const(JsValue::from(0xFFFFFFFFu32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_ELEMENT"),
                pd_const(JsValue::from(0x1u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_ATTRIBUTE"),
                pd_const(JsValue::from(0x2u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_TEXT"),
                pd_const(JsValue::from(0x4u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_CDATA_SECTION"),
                pd_const(JsValue::from(0x8u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_PROCESSING_INSTRUCTION"),
                pd_const(JsValue::from(0x40u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_COMMENT"),
                pd_const(JsValue::from(0x80u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_DOCUMENT"),
                pd_const(JsValue::from(0x100u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_DOCUMENT_TYPE"),
                pd_const(JsValue::from(0x200u32)),
            );
            let _ = nf.insert_property(
                boa_engine::js_string!("SHOW_DOCUMENT_FRAGMENT"),
                pd_const(JsValue::from(0x400u32)),
            );
        }
    }

    // ── AbortController (caniuse: 99%+) ──
    let ac_ctor = NativeFunction::from_copy_closure(move |this, _a, ctx| {
        let obj = this
            .as_object()
            .unwrap_or_else(|| boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()));
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        // Create the signal object.
        let signal = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = signal.insert_property(boa_engine::js_string!("aborted"), pd(JsValue::from(false)));
        let _ = signal.insert_property(boa_engine::js_string!("reason"), pd(JsValue::undefined()));
        // Set AbortSignal.prototype.
        if let Ok(ctor_val) = ctx
            .global_object()
            .get(boa_engine::js_string!("AbortSignal"), ctx)
        {
            if let Some(ctor_obj) = ctor_val.as_object() {
                if let Ok(pv) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                    if let Some(p) = pv.as_object() {
                        let _ = signal.set_prototype(Some(p));
                    }
                }
            }
        }
        // throwIfAborted() method.
        let sig_ref = signal.clone();
        let tia = NativeFunction::from_copy_closure_with_captures(
            move |_t, _a, sig_ref, ctx| {
                let aborted = sig_ref
                    .get(boa_engine::js_string!("aborted"), ctx)
                    .ok()
                    .and_then(|v| v.as_boolean())
                    .unwrap_or(false);
                if aborted {
                    let reason = sig_ref
                        .get(boa_engine::js_string!("reason"), ctx)
                        .ok()
                        .unwrap_or(JsValue::undefined());
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message(
                            reason
                                .to_string(ctx)
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default(),
                        )
                        .into());
                }
                Ok(JsValue::undefined())
            },
            sig_ref,
        );
        let _ = signal.insert_property(
            boa_engine::js_string!("throwIfAborted"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), tia).build(),
            )),
        );
        let _ = obj.insert_property(boa_engine::js_string!("signal"), pd(signal.into()));
        // Set AbortController.prototype.
        if let Ok(ctor_val) = ctx
            .global_object()
            .get(boa_engine::js_string!("AbortController"), ctx)
        {
            if let Some(ctor_obj) = ctor_val.as_object() {
                if let Ok(pv) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                    if let Some(p) = pv.as_object() {
                        let _ = obj.set_prototype(Some(p));
                    }
                }
            }
        }
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("AbortController"), 0, ac_ctor);
    // Add abort() to AbortController.prototype.
    if let Ok(ac_val) = ctx
        .global_object()
        .get(boa_engine::js_string!("AbortController"), ctx)
    {
        if let Some(ac) = ac_val.as_object() {
            if let Ok(proto_val) = ac.get(boa_engine::js_string!("prototype"), ctx) {
                if let Some(proto) = proto_val.as_object() {
                    let abort_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                        if let Some(o) = this.as_object() {
                            let pd = |v: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(v)
                                    .writable(true)
                                    .enumerable(true)
                                    .configurable(true)
                                    .build()
                            };
                            if let Ok(sig) = o.get(boa_engine::js_string!("signal"), ctx) {
                                if let Some(so) = sig.as_object() {
                                    let reason =
                                        args.first().cloned().unwrap_or(JsValue::undefined());
                                    let _ = so.insert_property(
                                        boa_engine::js_string!("aborted"),
                                        pd(JsValue::from(true)),
                                    );
                                    let _ = so.insert_property(
                                        boa_engine::js_string!("reason"),
                                        pd(reason),
                                    );
                                }
                            }
                        }
                        Ok(JsValue::undefined())
                    });
                    let _ = proto.insert_property(
                        boa_engine::js_string!("abort"),
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(
                                boa_engine::object::FunctionObjectBuilder::new(
                                    ctx.realm(),
                                    abort_fn,
                                )
                                .build(),
                            )
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build(),
                    );
                }
            }
        }
    }

    // ── MutationObserver (caniuse: 97%+) ──
    // Stub: creates observer, observe/disconnect/takeRecords are no-ops.
    let mo_ctor = NativeFunction::from_copy_closure(move |this, args, ctx| {
        let obj = this
            .as_object()
            .unwrap_or_else(|| boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()));
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let callback = args.first().cloned().unwrap_or(JsValue::undefined());
        let _ = obj.insert_property(boa_engine::js_string!("callback"), pd(callback));
        // observe(target, options) — stores target and snapshot of className.
        let observe_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
            let target = args.first().cloned().unwrap_or(JsValue::null());
            if let (Some(o), Some(t)) = (this.as_object(), target.as_object()) {
                let pd2 = |v: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(v)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                // Store observed element.
                let _ = o.insert_property(
                    boa_engine::js_string!("_observed"),
                    pd2(JsValue::from(t.clone())),
                );
                // Snapshot className (for attribute mutation detection).
                let snap = t
                    .get(boa_engine::js_string!("className"), ctx)
                    .ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                let _ = o.insert_property(
                    boa_engine::js_string!("_classSnap"),
                    pd2(JsValue::from(boa_engine::js_string!(snap))),
                );
                // Also snapshot attributes to detect any attribute change.
                let _ = o.insert_property(
                    boa_engine::js_string!("_attrSnap"),
                    pd2(JsValue::from(true)),
                );
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("observe"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), observe_fn).build(),
            )),
        );
        // disconnect() — clears stored references.
        let disconnect_fn = NativeFunction::from_copy_closure(|this, _a, ctx| {
            if let Some(o) = this.as_object() {
                let pd2 = |v: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(v)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ =
                    o.insert_property(boa_engine::js_string!("_observed"), pd2(JsValue::null()));
                let _ = o.insert_property(
                    boa_engine::js_string!("_classSnap"),
                    pd2(JsValue::from(boa_engine::js_string!(""))),
                );
            }
            Ok(JsValue::undefined())
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("disconnect"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), disconnect_fn).build(),
            )),
        );
        // takeRecords() — returns mutation records if className changed.
        let take_fn = NativeFunction::from_copy_closure(|this, _a, ctx| {
            let records = new_array(ctx);
            if let Some(o) = this.as_object() {
                // Check if className changed since observe().
                let target = o
                    .get(boa_engine::js_string!("_observed"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let old_snap = o
                    .get(boa_engine::js_string!("_classSnap"), ctx)
                    .ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                if let Some(t) = target {
                    let new_class = t
                        .get(boa_engine::js_string!("className"), ctx)
                        .ok()
                        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                        .unwrap_or_default();
                    if new_class != old_snap {
                        // Class changed — return a MutationRecord.
                        let record =
                            boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let rpd = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = record.insert_property(
                            boa_engine::js_string!("type"),
                            rpd(JsValue::from(boa_engine::js_string!("attributes"))),
                        );
                        let _ = record.insert_property(
                            boa_engine::js_string!("attributeName"),
                            rpd(JsValue::from(boa_engine::js_string!("class"))),
                        );
                        let _ = record.insert_property(
                            boa_engine::js_string!("target"),
                            rpd(JsValue::from(t.clone())),
                        );
                        let _ = record.insert_property(
                            boa_engine::js_string!("oldValue"),
                            rpd(JsValue::from(boa_engine::js_string!(old_snap.clone()))),
                        );
                        let _ = records.push(JsValue::from(record), ctx);
                        // Update snapshot to avoid duplicate reports.
                        let pd2 = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = o.insert_property(
                            boa_engine::js_string!("_classSnap"),
                            pd2(JsValue::from(boa_engine::js_string!(new_class))),
                        );
                    }
                }
            }
            Ok(JsValue::from(records))
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("takeRecords"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), take_fn).build(),
            )),
        );
        // Set MutationObserver.prototype.
        if let Ok(ctor_val) = ctx
            .global_object()
            .get(boa_engine::js_string!("MutationObserver"), ctx)
        {
            if let Some(ctor_obj) = ctor_val.as_object() {
                if let Ok(pv) = ctor_obj.get(boa_engine::js_string!("prototype"), ctx) {
                    if let Some(p) = pv.as_object() {
                        let _ = obj.set_prototype(Some(p));
                    }
                }
            }
        }
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("MutationObserver"), 1, mo_ctor);

    // ── requestAnimationFrame / cancelAnimationFrame (caniuse: 100%) ──
    // Synchronous stub: calls callback immediately with timestamp, returns 0.
    let raf_fn = NativeFunction::from_copy_closure(|_t, args, ctx| {
        if let Some(cb) = args.first() {
            if let Some(cb_fn) = cb.as_object() {
                if cb_fn.is_callable() {
                    let ts = JsValue::from(0.0f64);
                    let _ = cb_fn.call(&JsValue::undefined(), &[ts], ctx);
                }
            }
        }
        Ok(JsValue::from(0u32))
    });
    let _ =
        ctx.register_global_callable(boa_engine::js_string!("requestAnimationFrame"), 1, raf_fn);
    let caf_fn = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
    let _ = ctx.register_global_callable(boa_engine::js_string!("cancelAnimationFrame"), 1, caf_fn);

    // ── Storage stubs: localStorage / sessionStorage (caniuse: 95%+) ──
    fn make_storage(ctx: &mut Context) -> boa_engine::JsObject {
        let store = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = store.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
        // getItem(key) → null (empty storage).
        let gi = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::null()));
        let _ = store.insert_property(
            boa_engine::js_string!("getItem"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), gi.clone()).build(),
            )),
        );
        let _ = store.insert_property(
            boa_engine::js_string!("setItem"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), gi.clone()).build(),
            )),
        );
        let _ = store.insert_property(
            boa_engine::js_string!("removeItem"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), gi.clone()).build(),
            )),
        );
        let _ = store.insert_property(
            boa_engine::js_string!("clear"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), gi.clone()).build(),
            )),
        );
        let _ = store.insert_property(
            boa_engine::js_string!("key"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), gi).build(),
            )),
        );
        store
    }
    let ls = make_storage(ctx);
    let _ = ctx.global_object().insert_property(
        boa_engine::js_string!("localStorage"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(ls))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );
    let ss = make_storage(ctx);
    let _ = ctx.global_object().insert_property(
        boa_engine::js_string!("sessionStorage"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(ss))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // ── queueMicrotask (caniuse: 97%+) ── Synchronous execution.
    let qmt_fn = NativeFunction::from_copy_closure(|_t, args, ctx| {
        if let Some(cb) = args.first() {
            if let Some(cb_fn) = cb.as_object() {
                if cb_fn.is_callable() {
                    let _ = cb_fn.call(&JsValue::undefined(), &[], ctx);
                }
            }
        }
        Ok(JsValue::undefined())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("queueMicrotask"), 1, qmt_fn);

    // ── structuredClone (caniuse: 97%+) ── Simple deep clone via JSON roundtrip fallback.
    let sc_fn = NativeFunction::from_copy_closure(|_t, args, ctx| {
        // Best-effort: use clone_node_js for objects, otherwise return as-is.
        let val = args.first().cloned().unwrap_or(JsValue::undefined());
        if let Some(o) = val.as_object() {
            return clone_node_js(Some(&o), true, ctx);
        }
        Ok(val)
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("structuredClone"), 1, sc_fn);

    // ── BroadcastChannel (caniuse: 96%+) ── Stub.
    let bc_ctor = NativeFunction::from_copy_closure(move |this, _a, ctx| {
        let obj = this
            .as_object()
            .unwrap_or_else(|| boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()));
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = obj.insert_property(
            boa_engine::js_string!("name"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        let noop = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("postMessage"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop.clone()).build(),
            )),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("close"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop).build(),
            )),
        );
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("BroadcastChannel"), 1, bc_ctor);

    // ── Gamepad API (caniuse: 97%+) ── Stub.
    let gp_fn = NativeFunction::from_copy_closure(|_t, _a, ctx| Ok(JsValue::from(new_array(ctx))));
    let _ = ctx.global_object().insert_property(
        boa_engine::js_string!("getGamepads"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), gp_fn).build(),
            ))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // ── Web Crypto (caniuse: 97%+) ── Stub.
    let crypto_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let crypto_pd = |v: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(v)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let _ = crypto_obj.insert_property(
        boa_engine::js_string!("subtle"),
        crypto_pd(JsValue::from(
            boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()),
        )),
    );
    let get_random = NativeFunction::from_copy_closure(|_t, args, ctx| {
        let arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd3 = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = arr.insert_property(boa_engine::js_string!("length"), pd3(JsValue::from(0u32)));
        Ok(JsValue::from(arr))
    });
    let _ = crypto_obj.insert_property(
        boa_engine::js_string!("getRandomValues"),
        crypto_pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), get_random).build(),
        )),
    );
    let _ = crypto_obj.insert_property(
        boa_engine::js_string!("randomUUID"),
        crypto_pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(
                ctx.realm(),
                NativeFunction::from_copy_closure(|_t, _a, ctx| {
                    Ok(JsValue::from(boa_engine::js_string!(
                        "00000000-0000-0000-0000-000000000000"
                    )))
                }),
            )
            .build(),
        )),
    );
    let _ = ctx.global_object().insert_property(
        boa_engine::js_string!("crypto"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(crypto_obj))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // ── indexedDB (caniuse: 95%+) ── Stub.
    let idb_factory = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let idb_pd = |v: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(v)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let idb_open = NativeFunction::from_copy_closure(|_t, _a, ctx| {
        let req = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd4 = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = req.insert_property(boa_engine::js_string!("result"), pd4(JsValue::null()));
        let _ = req.insert_property(boa_engine::js_string!("error"), pd4(JsValue::null()));
        let _ = req.insert_property(boa_engine::js_string!("onsuccess"), pd4(JsValue::null()));
        let _ = req.insert_property(boa_engine::js_string!("onerror"), pd4(JsValue::null()));
        Ok(JsValue::from(req))
    });
    let _ = idb_factory.insert_property(
        boa_engine::js_string!("open"),
        idb_pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), idb_open).build(),
        )),
    );
    let _ = ctx.global_object().insert_property(
        boa_engine::js_string!("indexedDB"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(idb_factory))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // ── CSS.supports / CSS.escape (caniuse: 97%+) ──
    let css_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let css_pd = |v: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(v)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let css_supports = NativeFunction::from_copy_closure(|_t, args, ctx| {
        // Simple check: just look for basic property:value format support.
        let prop = arg_string(args, 0);
        let val = args
            .get(1)
            .map(|_| arg_to_string(args, 1, ctx))
            .unwrap_or_default();
        // Return true for common properties.
        let known: &[&str] = &[
            "display",
            "color",
            "width",
            "height",
            "margin",
            "padding",
            "border",
            "background",
            "font",
            "flex",
            "grid",
            "transform",
            "opacity",
            "position",
            "overflow",
            "visibility",
            "z-index",
            "cursor",
            "pointer-events",
            "user-select",
        ];
        Ok(JsValue::from(known.contains(&prop.as_str())))
    });
    let _ = css_obj.insert_property(
        boa_engine::js_string!("supports"),
        css_pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), css_supports).build(),
        )),
    );
    let css_escape = NativeFunction::from_copy_closure(|_t, args, ctx| {
        let s = arg_to_string(args, 0, ctx);
        // Basic CSS escaping: escape special characters.
        let escaped: String = s
            .chars()
            .map(|c| match c {
                '"' | '\'' | '\\' | '(' | ')' | '{' | '}' | ';' | ':' | ',' => {
                    format!("\\{:X} ", c as u32)
                }
                _ => c.to_string(),
            })
            .collect();
        Ok(JsValue::from(boa_engine::js_string!(escaped)))
    });
    let _ = css_obj.insert_property(
        boa_engine::js_string!("escape"),
        css_pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), css_escape).build(),
        )),
    );
    let _ = ctx.global_object().insert_property(
        boa_engine::js_string!("CSS"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(css_obj))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // ── ResizeObserver (caniuse: 96%+) ── Stub.
    let ro_ctor = NativeFunction::from_copy_closure(move |this, _a, ctx| {
        let obj = this
            .as_object()
            .unwrap_or_else(|| boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()));
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let noop = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("observe"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop.clone()).build(),
            )),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("unobserve"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop.clone()).build(),
            )),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("disconnect"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop).build(),
            )),
        );
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("ResizeObserver"), 1, ro_ctor);

    // ── IntersectionObserver (caniuse: 97%+) ── Stub.
    let io_ctor = NativeFunction::from_copy_closure(move |this, _a, ctx| {
        let obj = this
            .as_object()
            .unwrap_or_else(|| boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()));
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let noop = NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
        let _ = obj.insert_property(
            boa_engine::js_string!("observe"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop.clone()).build(),
            )),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("unobserve"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop.clone()).build(),
            )),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("disconnect"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), noop.clone()).build(),
            )),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("takeRecords"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(
                    ctx.realm(),
                    NativeFunction::from_copy_closure(|_t, _a, ctx| {
                        Ok(JsValue::from(new_array(ctx)))
                    }),
                )
                .build(),
            )),
        );
        let _ = obj.insert_property(boa_engine::js_string!("root"), pd(JsValue::null()));
        let _ = obj.insert_property(
            boa_engine::js_string!("rootMargin"),
            pd(JsValue::from(boa_engine::js_string!("0px"))),
        );
        let _ = obj.insert_property(
            boa_engine::js_string!("thresholds"),
            pd(JsValue::from(new_array(ctx))),
        );
        Ok(obj.into())
    });
    let _ =
        ctx.register_global_callable(boa_engine::js_string!("IntersectionObserver"), 1, io_ctor);

    // ── Performance API (caniuse: 98%+) ── Minimal stub.
    let perf_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let perf_pd = |v: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(v)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let _ = perf_obj.insert_property(
        boa_engine::js_string!("now"),
        perf_pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(
                ctx.realm(),
                NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::from(0.0f64))),
            )
            .build(),
        )),
    );
    let _ = perf_obj.insert_property(
        boa_engine::js_string!("timeOrigin"),
        perf_pd(JsValue::from(0.0f64)),
    );
    let _ = ctx.global_object().insert_property(
        boa_engine::js_string!("performance"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(perf_obj))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );
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
                .with_message("substringData requires at least 1 argument")
                .into());
        }
        if args.get(1).map(|v| v.is_undefined()).unwrap_or(true) {
            return Err(boa_engine::JsNativeError::typ()
                .with_message("substringData requires offset and count")
                .into());
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
            let parent = obj
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                remove_from_children(&parent_obj, &obj, ctx);
                let pd = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ =
                    obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
                let _ = obj
                    .insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
            }
        }
        Ok(JsValue::undefined())
    });
    let before_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
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
            let parent = obj
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
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
            let parent = obj
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
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
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ =
                    obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
                let _ = obj
                    .insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
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
        let _ = o.insert_property(
            boa_engine::js_string!("_data"),
            pd(JsValue::from(js_str.clone())),
        );
        // Also update textContent and length.
        let _ = o.insert_property(
            boa_engine::js_string!("textContent"),
            pd(JsValue::from(js_str)),
        );
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
fn update_ranges_for_data_change(
    node: &JsObject,
    offset: u32,
    count: u32,
    data_len: u32,
    ctx: &mut Context,
) {
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
    let len = ranges_arr
        .get(boa_engine::js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as u32;
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    // Per DOM spec "replace data" steps:
    // Step 1: For offset > offset && <= offset+count, set to offset.
    // Step 2: For offset > offset+count, add data_len, subtract count.
    for i in 0..len {
        if let Ok(range_val) = ranges_arr.get(i as u32, ctx) {
            if let Some(range) = range_val.as_object() {
                for (container_key, offset_key) in &[
                    (
                        boa_engine::js_string!("startContainer"),
                        boa_engine::js_string!("startOffset"),
                    ),
                    (
                        boa_engine::js_string!("endContainer"),
                        boa_engine::js_string!("endOffset"),
                    ),
                ] {
                    let cont = range.get(container_key.clone(), ctx).ok();
                    if let Some(cont_obj) = cont.and_then(|v| v.as_object()) {
                        if boa_engine::object::JsObject::equals(&cont_obj, node) {
                            let cur_offset = range
                                .get(offset_key.clone(), ctx)
                                .ok()
                                .and_then(|v| v.as_number())
                                .unwrap_or(0.0) as u32;
                            let new_offset = if cur_offset > offset && cur_offset <= offset + count
                            {
                                // Step 1: clamp to offset
                                offset
                            } else if cur_offset > offset + count {
                                // Step 2: add data_len, subtract count
                                cur_offset + data_len - count
                            } else {
                                cur_offset
                            };
                            let _ = range
                                .insert_property(offset_key.clone(), pd(JsValue::from(new_offset)));
                        }
                    }
                }
            }
        }
    }
}

/// Update Range boundary points after a node is removed from its parent.
/// Per DOM spec "insert" and "remove" steps.
fn update_ranges_for_node_removal(
    removed_node: &JsObject,
    old_parent: &JsObject,
    old_index: u32,
    ctx: &mut Context,
) {
    let global = ctx.global_object();
    let ranges_val = match global.get(boa_engine::js_string!("__active_ranges"), ctx) {
        Ok(v) => v,
        Err(_) => return,
    };
    let ranges_arr = match ranges_val.as_object() {
        Some(o) => o,
        None => return,
    };
    let len = ranges_arr
        .get(boa_engine::js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as u32;
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    for i in 0..len {
        if let Ok(range_val) = ranges_arr.get(i as u32, ctx) {
            if let Some(range) = range_val.as_object() {
                for (container_key, offset_key) in &[
                    (
                        boa_engine::js_string!("startContainer"),
                        boa_engine::js_string!("startOffset"),
                    ),
                    (
                        boa_engine::js_string!("endContainer"),
                        boa_engine::js_string!("endOffset"),
                    ),
                ] {
                    let cont = range.get(container_key.clone(), ctx).ok();
                    if let Some(cont_obj) = cont.and_then(|v| v.as_object()) {
                        // Check if container is removed_node or a descendant of it.
                        let is_removed_or_descendant =
                            boa_engine::object::JsObject::equals(&cont_obj, removed_node)
                                || is_descendant_of(&cont_obj, removed_node, ctx);
                        if is_removed_or_descendant {
                            // Set boundary to (old_parent, old_index).
                            let _ = range.insert_property(
                                container_key.clone(),
                                pd(JsValue::from(old_parent.clone())),
                            );
                            let _ = range
                                .insert_property(offset_key.clone(), pd(JsValue::from(old_index)));
                        } else if boa_engine::object::JsObject::equals(&cont_obj, old_parent) {
                            // If container is old_parent and offset > old_index, subtract 1.
                            let cur_offset = range
                                .get(offset_key.clone(), ctx)
                                .ok()
                                .and_then(|v| v.as_number())
                                .unwrap_or(0.0) as u32;
                            if cur_offset > old_index {
                                let _ = range.insert_property(
                                    offset_key.clone(),
                                    pd(JsValue::from(cur_offset - 1)),
                                );
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
    let parent = match inserted_node
        .get(boa_engine::js_string!("parentNode"), ctx)
        .ok()
        .and_then(|v| v.as_object())
    {
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
    let len = ranges_arr
        .get(boa_engine::js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as u32;
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    for i in 0..len {
        if let Ok(range_val) = ranges_arr.get(i as u32, ctx) {
            if let Some(range) = range_val.as_object() {
                for (container_key, offset_key) in &[
                    (
                        boa_engine::js_string!("startContainer"),
                        boa_engine::js_string!("startOffset"),
                    ),
                    (
                        boa_engine::js_string!("endContainer"),
                        boa_engine::js_string!("endOffset"),
                    ),
                ] {
                    let cont = range.get(container_key.clone(), ctx).ok();
                    if let Some(cont_obj) = cont.and_then(|v| v.as_object()) {
                        if boa_engine::object::JsObject::equals(&cont_obj, &parent) {
                            let cur_offset = range
                                .get(offset_key.clone(), ctx)
                                .ok()
                                .and_then(|v| v.as_number())
                                .unwrap_or(0.0) as u32;
                            if cur_offset > new_index {
                                let _ = range.insert_property(
                                    offset_key.clone(),
                                    pd(JsValue::from(cur_offset + 1)),
                                );
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
    let mut current = node
        .get(boa_engine::js_string!("parentNode"), ctx)
        .ok()
        .and_then(|v| v.as_object());
    for _ in 0..100 {
        match current {
            Some(p) => {
                if boa_engine::object::JsObject::equals(&p, ancestor) {
                    return true;
                }
                current = p
                    .get(boa_engine::js_string!("parentNode"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
            }
            None => return false,
        }
    }
    false
}

/// Get the index of a node in its parent's _children array.
fn node_index_in_parent(node: &JsObject, ctx: &mut Context) -> u32 {
    let parent = match node
        .get(boa_engine::js_string!("parentNode"), ctx)
        .ok()
        .and_then(|v| v.as_object())
    {
        Some(p) => p,
        None => return 0,
    };
    let children = match parent
        .get(boa_engine::js_string!("_children"), ctx)
        .ok()
        .and_then(|v| v.as_object())
    {
        Some(c) => c,
        None => return 0,
    };
    let len = children
        .get(boa_engine::js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as u32;
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
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let children = parent
        .get(boa_engine::js_string!("_children"), ctx)
        .ok()
        .and_then(|v| v.as_object());
    match children {
        Some(obj) => JsArray::from_object(obj).unwrap_or_else(|_| new_array(ctx)),
        None => {
            let arr = new_array(ctx);
            let _ =
                parent.insert_property(boa_engine::js_string!("_children"), pd(arr.clone().into()));
            arr
        }
    }
}

fn get_next_sibling(parent: &JsObject, child: &JsObject, ctx: &mut Context) -> Option<JsObject> {
    let children = parent
        .get(boa_engine::js_string!("_children"), ctx)
        .ok()
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
        let splice_fn = arr
            .get(boa_engine::js_string!("splice"), ctx)
            .ok()
            .and_then(|v| v.as_object());
        if let Some(sf) = splice_fn {
            if sf.is_callable() {
                let arr_obj: &JsObject = &*arr;
                let _ = sf.call(
                    &JsValue::from(arr_obj.clone()),
                    &[JsValue::from(idx as i32), JsValue::from(1i32)],
                    ctx,
                );
            }
        }
    }
}

fn insert_into_children(
    parent: &JsObject,
    child: &JsObject,
    before: Option<JsObject>,
    ctx: &mut Context,
) {
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
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
        let splice_fn = arr
            .get(boa_engine::js_string!("splice"), ctx)
            .ok()
            .and_then(|v| v.as_object());
        if let Some(sf) = splice_fn {
            if sf.is_callable() {
                let arr_obj2: &JsObject = &*arr;
                let _ = sf.call(
                    &JsValue::from(arr_obj2.clone()),
                    &[
                        JsValue::from(ref_idx as i32),
                        JsValue::from(0i32),
                        JsValue::from(child.clone()),
                    ],
                    ctx,
                );
            } else {
                let _ = arr.push(JsValue::from(child.clone()), ctx);
            }
        } else {
            let _ = arr.push(JsValue::from(child.clone()), ctx);
        }
    }
    let _ = child.insert_property(
        boa_engine::js_string!("parentNode"),
        pd(JsValue::from(parent.clone())),
    );
    let _ = child.insert_property(
        boa_engine::js_string!("parentElement"),
        pd(JsValue::from(parent.clone())),
    );
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
    let _ = obj.insert_property(
        boa_engine::js_string!("appendChild"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), append).build(),
            ))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // insertBefore
    let insert_before = NativeFunction::from_copy_closure(|this, args, ctx| {
        let child = args.first().cloned().unwrap_or(JsValue::null());
        let ref_node = args.get(1).cloned().unwrap_or(JsValue::null());
        if let (Some(parent_obj), Some(child_obj)) = (this.as_object(), child.as_object()) {
            insert_into_children(&parent_obj, &child_obj, ref_node.as_object(), ctx);
        }
        Ok(child)
    });
    let _ = obj.insert_property(
        boa_engine::js_string!("insertBefore"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), insert_before).build(),
            ))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // removeChild
    let remove_child = NativeFunction::from_copy_closure(|this, args, ctx| {
        let child = args.first().cloned().unwrap_or(JsValue::null());
        if child.is_null() || child.is_undefined() || child.as_object().is_none() {
            return Err(boa_engine::JsNativeError::typ()
                .with_message("Argument is not a Node")
                .into());
        }
        let child_obj = child.as_object().unwrap();
        if let Some(parent_obj) = this.as_object() {
            remove_from_children(&parent_obj, &child_obj, ctx);
            let pd2 = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = child_obj
                .insert_property(boa_engine::js_string!("parentNode"), pd2(JsValue::null()));
            let _ = child_obj.insert_property(
                boa_engine::js_string!("parentElement"),
                pd2(JsValue::null()),
            );
        }
        Ok(child)
    });
    let _ = obj.insert_property(
        boa_engine::js_string!("removeChild"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), remove_child).build(),
            ))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // setAttribute
    let set_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0).to_ascii_lowercase();
        let value = arg_string(args, 1);
        let value2 = value.clone();
        if let Some(o) = this.as_object() {
            let _ = o.insert_property(
                boa_engine::js_string!(name.clone()),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::from(boa_engine::js_string!(value)))
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    let attr_obj =
                        boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let pd2 = |val: JsValue| {
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(val)
                            .writable(true)
                            .enumerable(true)
                            .configurable(true)
                            .build()
                    };
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("name"),
                        pd2(JsValue::from(boa_engine::js_string!(name))),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("value"),
                        pd2(JsValue::from(boa_engine::js_string!(value2))),
                    );
                    let _ = attrs.insert_property(len, pd2(attr_obj.into()));
                    let _ = attrs.insert_property(
                        boa_engine::js_string!("length"),
                        pd2(JsValue::from(len + 1)),
                    );
                }
            }
        }
        Ok(JsValue::undefined())
    });
    let _ = obj.insert_property(
        boa_engine::js_string!("setAttribute"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), set_attr).build(),
            ))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // getAttribute
    let get_attr = NativeFunction::from_copy_closure(|this, args, ctx| {
        let name = arg_string(args, 0).to_ascii_lowercase();
        if let Some(o) = this.as_object() {
            if let Ok(v) = o.get(boa_engine::js_string!(name), ctx) {
                if !v.is_undefined() && !v.is_null() {
                    return Ok(v);
                }
            }
        }
        Ok(JsValue::null())
    });
    let _ = obj.insert_property(
        boa_engine::js_string!("getAttribute"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), get_attr).build(),
            ))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

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
    let _ = obj.insert_property(
        boa_engine::js_string!("hasAttribute"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), has_attr).build(),
            ))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // remove
    let remove_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                remove_from_children(&parent_obj, &obj, ctx);
                let pd2 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ =
                    obj.insert_property(boa_engine::js_string!("parentNode"), pd2(JsValue::null()));
            }
        }
        Ok(JsValue::undefined())
    });
    let _ = obj.insert_property(
        boa_engine::js_string!("remove"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), remove_fn).build(),
            ))
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // Empty attributes NamedNodeMap.
    let attrs_map = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let pd2 = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let _ = attrs_map.insert_property(boa_engine::js_string!("length"), pd2(JsValue::from(0u32)));
    let _ = obj.insert_property(boa_engine::js_string!("attributes"), pd2(attrs_map.into()));
}

/// Convert a JsValue to a DOM node for insertion (strings become text nodes).
fn value_to_node(val: &JsValue, ctx: &mut Context) -> JsValue {
    if val.is_object() {
        return val.clone();
    }
    // Convert to string and create a text node.
    let s = val
        .to_string(ctx)
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let pd = |v: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(v)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let _ = obj.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32)));
    let _ = obj.insert_property(
        boa_engine::js_string!("_data"),
        pd(JsValue::from(boa_engine::js_string!(s.clone()))),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("data"),
        pd(JsValue::from(boa_engine::js_string!(s.clone()))),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("nodeValue"),
        pd(JsValue::from(boa_engine::js_string!(s.clone()))),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("textContent"),
        pd(JsValue::from(boa_engine::js_string!(s))),
    );
    let _ = obj.insert_property(
        boa_engine::js_string!("nodeName"),
        pd(JsValue::from(boa_engine::js_string!("#text"))),
    );
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
    if depth > 50 {
        return String::new();
    }
    let nt = o
        .get(boa_engine::js_string!("nodeType"), ctx)
        .ok()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as u32;
    // Text nodes: return their data.
    if nt == 3 {
        if let Ok(v) = o.get(boa_engine::js_string!("_data"), ctx) {
            if let Some(s) = v.as_string() {
                return s.to_std_string_escaped();
            }
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
                let len = ca_ref
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
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
                let clen = ca_ref
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
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
    if depth > 100 {
        return String::new();
    } // Prevent stack overflow from cycles.
    if let Some(o) = val.as_object() {
        let nt = o
            .get(boa_engine::js_string!("nodeType"), ctx)
            .ok()
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as u32;
        let nt = o
            .get(boa_engine::js_string!("nodeType"), ctx)
            .ok()
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as u32;
        match nt {
            3 => {
                // Text node
                o.get(boa_engine::js_string!("data"), ctx)
                    .ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default()
            }
            8 => {
                // Comment node
                let data = o
                    .get(boa_engine::js_string!("data"), ctx)
                    .ok()
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                format!("<!--{}-->", data)
            }
            1 | 9 | 11 => {
                // Element / Document / DocumentFragment
                let tag = o
                    .get(boa_engine::js_string!("tagName"), ctx)
                    .ok()
                    .or_else(|| o.get(boa_engine::js_string!("nodeName"), ctx).ok())
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                // Serialize attributes.
                let mut attrs_str = String::new();
                if let Ok(av) = o.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(ao) = av.as_object() {
                        let alen = ao
                            .get(boa_engine::js_string!("length"), ctx)
                            .ok()
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0) as u32;
                        for i in 0..alen {
                            if let Ok(a) = ao.get(i as u32, ctx) {
                                if let Some(a_obj) = a.as_object() {
                                    let name = a_obj
                                        .get(boa_engine::js_string!("name"), ctx)
                                        .ok()
                                        .and_then(|v| {
                                            v.as_string().map(|s| s.to_std_string_escaped())
                                        })
                                        .unwrap_or_default();
                                    let value = a_obj
                                        .get(boa_engine::js_string!("value"), ctx)
                                        .ok()
                                        .and_then(|v| {
                                            v.as_string().map(|s| s.to_std_string_escaped())
                                        })
                                        .unwrap_or_default();
                                    if !name.is_empty() {
                                        attrs_str.push_str(&format!(
                                            " {}=\"{}\"",
                                            name.to_ascii_lowercase(),
                                            value
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                // Serialize children from _children array (use serialize_children for JsArray support).
                let children_html = serialize_children(&o, ctx);
                // Void elements don't have closing tags.
                let void = matches!(
                    tag.to_ascii_lowercase().as_str(),
                    "area"
                        | "base"
                        | "br"
                        | "col"
                        | "embed"
                        | "hr"
                        | "img"
                        | "input"
                        | "link"
                        | "meta"
                        | "param"
                        | "source"
                        | "track"
                        | "wbr"
                );
                if void {
                    format!("<{}{}>", tag.to_ascii_lowercase(), attrs_str)
                } else {
                    format!(
                        "<{}{}>{}</{}>",
                        tag.to_ascii_lowercase(),
                        attrs_str,
                        children_html,
                        tag.to_ascii_lowercase()
                    )
                }
            }
            _ => String::new(),
        }
    } else {
        val.display().to_string()
    }
}

fn element_matches_selector(
    obj: &boa_engine::object::JsObject,
    selector: &str,
    ctx: &mut Context,
) -> bool {
    let selector = selector.trim();
    if selector.is_empty() {
        return false;
    }
    // Handle comma-separated selector lists: match if ANY sub-selector matches.
    for sub in selector.split(',') {
        let sub = sub.trim();
        if element_matches_single_selector(obj, sub, ctx) {
            return true;
        }
    }
    false
}

fn element_matches_single_selector(
    obj: &boa_engine::object::JsObject,
    selector: &str,
    ctx: &mut Context,
) -> bool {
    let selector = selector.trim();
    if selector.is_empty() {
        return false;
    }

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
                if state == 't' && !current.is_empty() {
                    tag = current.clone();
                } else if state == '.' {
                    classes.push(current.clone());
                } else if state == '#' {
                    id = Some(current.clone());
                } else if state == ']' {
                    attrs.push((attr_name.clone(), attr_val.clone()));
                }
                current.clear();
                state = '.';
            }
            '#' => {
                if state == 't' && !current.is_empty() {
                    tag = current.clone();
                } else if state == '.' {
                    classes.push(current.clone());
                } else if state == '#' {
                    id = Some(current.clone());
                } else if state == ']' {
                    attrs.push((attr_name.clone(), attr_val.clone()));
                }
                current.clear();
                state = '#';
            }
            '[' => {
                if state == 't' && !current.is_empty() {
                    tag = current.clone();
                } else if state == '.' {
                    classes.push(current.clone());
                } else if state == '#' {
                    id = Some(current.clone());
                }
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
            _ => {
                current.push(c);
            }
        }
    }
    // Handle trailing state.
    match state {
        't' => {
            if !current.is_empty() {
                tag = current;
            }
        }
        '.' => {
            classes.push(current);
        }
        '#' => {
            id = Some(current);
        }
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
        let tn = obj
            .get(boa_engine::js_string!("tagName"), ctx)
            .ok()
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        if !tn.eq_ignore_ascii_case(&tag) {
            return false;
        }
    }
    // Check classes.
    for cls in &classes {
        let cn = obj
            .get(boa_engine::js_string!("className"), ctx)
            .ok()
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        if !cn.split_whitespace().any(|c| c.eq_ignore_ascii_case(cls)) {
            return false;
        }
    }
    // Check id.
    if let Some(ref id_val) = id {
        let eid = obj
            .get(boa_engine::js_string!("id"), ctx)
            .ok()
            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
            .unwrap_or_default();
        if &eid != id_val {
            return false;
        }
    }
    // Check attributes.
    for (aname, aval) in &attrs {
        let has = obj.get(boa_engine::js_string!(aname.clone()), ctx).ok();
        match has {
            Some(v) => {
                if let Some(expected) = aval {
                    let actual = v
                        .as_string()
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    if &actual != expected {
                        return false;
                    }
                }
            }
            None => {
                // Check if it's in the NamedNodeMap.
                let found = if let Ok(av2) = obj.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(ao) = av2.as_object() {
                        let len = ao
                            .get(boa_engine::js_string!("length"), ctx)
                            .ok()
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0) as u32;
                        let mut f = false;
                        for i in 0..len {
                            if let Ok(a) = ao.get(i as u32, ctx) {
                                if let Some(a_obj) = a.as_object() {
                                    if let Ok(an) = a_obj.get(boa_engine::js_string!("name"), ctx) {
                                        if an
                                            .as_string()
                                            .map(|s| s.to_std_string_escaped().to_ascii_lowercase())
                                            .as_deref()
                                            == Some(&aname.to_ascii_lowercase())
                                        {
                                            if let Some(expected) = aval {
                                                if let Ok(av3) =
                                                    a_obj.get(boa_engine::js_string!("value"), ctx)
                                                {
                                                    if av3
                                                        .as_string()
                                                        .map(|s| s.to_std_string_escaped())
                                                        .as_deref()
                                                        == Some(expected.as_str())
                                                    {
                                                        f = true;
                                                        break;
                                                    }
                                                }
                                            } else {
                                                f = true;
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        f
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !found {
                    return false;
                }
            }
        }
    }
    // Check :not() pseudo-class.
    if let Some(not_sel) = pseudo_not {
        if element_matches_single_selector(obj, &not_sel, ctx) {
            return false;
        }
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
            let upper: String = tag
                .chars()
                .map(|c| {
                    if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect();
            let lower: String = tag
                .chars()
                .map(|c| {
                    if c.is_ascii_uppercase() {
                        c.to_ascii_lowercase()
                    } else {
                        c
                    }
                })
                .collect();
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
            let _ = handle.insert_property(boa_engine::js_string!("prefix"), pd(JsValue::null()));
            let _ =
                handle.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(1u32)));
            // nodeValue is set as an accessor (always null) by make_element_handle.
            // textContent is set as an accessor by make_element_handle.
            let _ = handle.insert_property(
                boa_engine::js_string!("ownerDocument"),
                pd(ctx
                    .global_object()
                    .get(boa_engine::js_string!("document"), ctx)
                    .unwrap_or(JsValue::null())),
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
            let _ = attrs_map
                .insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
            let _ = handle.insert_property(
                boa_engine::js_string!("attributes"),
                pd(JsValue::from(attrs_map.clone())),
            );
            // classList — DOMTokenList backed by element's className attribute.
            let class_list = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            // Store reference to parent element.
            let _ = class_list.insert_property(
                boa_engine::js_string!("_element"),
                pd(JsValue::from(handle.clone())),
            );
            // Store reference to attributes NamedNodeMap (avoids Boa prototype bug on lookup).
            let _ = class_list.insert_property(
                boa_engine::js_string!("_attrs"),
                pd(JsValue::from(attrs_map)),
            );
            // contains(token)
            let cl_contains = NativeFunction::from_copy_closure(|this, args, ctx| {
                let token_str = arg_to_string(args, 0, ctx);
                if let Some(o) = this.as_object() {
                    let el = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object());
                    if let Some(el_obj) = el {
                        let cn = el_obj
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        return Ok(JsValue::from(cn.split_whitespace().any(|c| c == token_str)));
                    }
                }
                Ok(JsValue::from(false))
            });
            let _ = class_list.insert_property(
                boa_engine::js_string!("contains"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_contains)
                        .build(),
                )),
            );
            // add(...tokens)
            let cl_add = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    let el = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object());
                    if let Some(el_obj) = el {
                        let cn = el_obj
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        // Normalize: split whitespace, remove empty & duplicate tokens.
                        let mut classes: Vec<String> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !classes.contains(&t.to_string()) {
                                classes.push(t.to_string());
                            }
                        }
                        for arg in args.iter() {
                            let token = arg
                                .to_string(ctx)
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default();
                            if !classes.contains(&token) {
                                classes.push(token);
                            }
                        }
                        let new_cn = classes.join(" ");
                        let pd2 = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = el_obj.insert_property(
                            boa_engine::js_string!("className"),
                            pd2(JsValue::from(boa_engine::js_string!(new_cn.clone()))),
                        );
                        // Also update via setAttribute if available.
                        if let Ok(sa) = el_obj.get(boa_engine::js_string!("setAttribute"), ctx) {
                            if let Some(sa_fn) = sa.as_object() {
                                if sa_fn.is_callable() {
                                    let _ = sa_fn.call(
                                        &JsValue::from(el_obj.clone()),
                                        &[
                                            JsValue::from(boa_engine::js_string!("class")),
                                            JsValue::from(boa_engine::js_string!(new_cn.clone())),
                                        ],
                                        ctx,
                                    );
                                }
                            }
                        }
                        // Also manually sync attributes for non-HTML elements (Boa bug workaround).
                        // Use _attrs stored on classList to bypass prototype chain issues.
                        if let Ok(av) = o.get(boa_engine::js_string!("_attrs"), ctx) {
                            if let Some(attrs) = av.as_object() {
                                let alen = attrs
                                    .get(boa_engine::js_string!("length"), ctx)
                                    .ok()
                                    .and_then(|v| v.as_number())
                                    .unwrap_or(0.0)
                                    as u32;
                                let mut fidx: Option<u32> = None;
                                for i in 0..alen {
                                    if let Ok(a) = attrs.get(i as u32, ctx) {
                                        if let Some(ao) = a.as_object() {
                                            if let Ok(an) =
                                                ao.get(boa_engine::js_string!("name"), ctx)
                                            {
                                                if an
                                                    .as_string()
                                                    .map(|s| {
                                                        s.to_std_string_escaped()
                                                            .to_ascii_lowercase()
                                                    })
                                                    .as_deref()
                                                    == Some("class")
                                                {
                                                    fidx = Some(i);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                let atr = boa_engine::object::JsObject::with_object_proto(
                                    ctx.intrinsics(),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("name"),
                                    pd2(JsValue::from(boa_engine::js_string!("class"))),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("value"),
                                    pd2(JsValue::from(boa_engine::js_string!(new_cn))),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("namespaceURI"),
                                    pd2(JsValue::null()),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("localName"),
                                    pd2(JsValue::from(boa_engine::js_string!("class"))),
                                );
                                if let Some(idx) = fidx {
                                    let _ = attrs.insert_property(idx, pd2(atr.into()));
                                } else {
                                    let _ = attrs.insert_property(alen, pd2(atr.into()));
                                    let _ = attrs.insert_property(
                                        boa_engine::js_string!("length"),
                                        pd2(JsValue::from(alen + 1)),
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(JsValue::undefined())
            });
            let _ = class_list.insert_property(
                boa_engine::js_string!("add"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_add).build(),
                )),
            );
            // remove(...tokens)
            let cl_remove = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    let el = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object());
                    if let Some(el_obj) = el {
                        let cn = el_obj
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        // Normalize: split whitespace, remove empty & duplicate tokens.
                        let mut classes: Vec<String> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !classes.contains(&t.to_string()) {
                                classes.push(t.to_string());
                            }
                        }
                        for arg in args.iter() {
                            let token = arg
                                .to_string(ctx)
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default();
                            classes.retain(|c| c != &token);
                        }
                        let new_cn = classes.join(" ");
                        let pd2 = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = el_obj.insert_property(
                            boa_engine::js_string!("className"),
                            pd2(JsValue::from(boa_engine::js_string!(new_cn.clone()))),
                        );
                        if let Ok(sa) = el_obj.get(boa_engine::js_string!("setAttribute"), ctx) {
                            if let Some(sa_fn) = sa.as_object() {
                                if sa_fn.is_callable() {
                                    let _ = sa_fn.call(
                                        &JsValue::from(el_obj.clone()),
                                        &[
                                            JsValue::from(boa_engine::js_string!("class")),
                                            JsValue::from(boa_engine::js_string!(new_cn.clone())),
                                        ],
                                        ctx,
                                    );
                                }
                            }
                        }
                        // Also manually sync attributes for non-HTML elements (Boa bug workaround).
                        // Use _attrs stored on classList to bypass prototype chain issues.
                        if let Ok(av) = o.get(boa_engine::js_string!("_attrs"), ctx) {
                            if let Some(attrs) = av.as_object() {
                                let alen = attrs
                                    .get(boa_engine::js_string!("length"), ctx)
                                    .ok()
                                    .and_then(|v| v.as_number())
                                    .unwrap_or(0.0)
                                    as u32;
                                let mut fidx: Option<u32> = None;
                                for i in 0..alen {
                                    if let Ok(a) = attrs.get(i as u32, ctx) {
                                        if let Some(ao) = a.as_object() {
                                            if let Ok(an) =
                                                ao.get(boa_engine::js_string!("name"), ctx)
                                            {
                                                if an
                                                    .as_string()
                                                    .map(|s| {
                                                        s.to_std_string_escaped()
                                                            .to_ascii_lowercase()
                                                    })
                                                    .as_deref()
                                                    == Some("class")
                                                {
                                                    fidx = Some(i);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                let atr = boa_engine::object::JsObject::with_object_proto(
                                    ctx.intrinsics(),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("name"),
                                    pd2(JsValue::from(boa_engine::js_string!("class"))),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("value"),
                                    pd2(JsValue::from(boa_engine::js_string!(new_cn))),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("namespaceURI"),
                                    pd2(JsValue::null()),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("localName"),
                                    pd2(JsValue::from(boa_engine::js_string!("class"))),
                                );
                                if let Some(idx) = fidx {
                                    let _ = attrs.insert_property(idx, pd2(atr.into()));
                                } else {
                                    let _ = attrs.insert_property(alen, pd2(atr.into()));
                                    let _ = attrs.insert_property(
                                        boa_engine::js_string!("length"),
                                        pd2(JsValue::from(alen + 1)),
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(JsValue::undefined())
            });
            let _ = class_list.insert_property(
                boa_engine::js_string!("remove"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_remove).build(),
                )),
            );
            // toggle(token, force?)
            let cl_toggle = NativeFunction::from_copy_closure(|this, args, ctx| {
                let token = arg_to_string(args, 0, ctx);
                let force = args.get(1).and_then(|v| v.as_boolean());
                if let Some(o) = this.as_object() {
                    let el = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object());
                    if let Some(el_obj) = el {
                        let cn = el_obj
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        let mut classes: Vec<String> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !classes.contains(&t.to_string()) {
                                classes.push(t.to_string());
                            }
                        }
                        let present = classes.contains(&token);
                        let should_add = force.unwrap_or(!present);
                        if should_add && !present {
                            classes.push(token.clone());
                        } else if !should_add && present {
                            classes.retain(|c| c != &token);
                        }
                        let new_cn = classes.join(" ");
                        let pd2 = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = el_obj.insert_property(
                            boa_engine::js_string!("className"),
                            pd2(JsValue::from(boa_engine::js_string!(new_cn.clone()))),
                        );
                        if let Ok(sa) = el_obj.get(boa_engine::js_string!("setAttribute"), ctx) {
                            if let Some(sa_fn) = sa.as_object() {
                                if sa_fn.is_callable() {
                                    let _ = sa_fn.call(
                                        &JsValue::from(el_obj.clone()),
                                        &[
                                            JsValue::from(boa_engine::js_string!("class")),
                                            JsValue::from(boa_engine::js_string!(new_cn.clone())),
                                        ],
                                        ctx,
                                    );
                                }
                            }
                        }
                        // Also manually sync attributes for non-HTML elements (Boa bug workaround).
                        // Use _attrs stored on classList to bypass prototype chain issues.
                        if let Ok(av) = o.get(boa_engine::js_string!("_attrs"), ctx) {
                            if let Some(attrs) = av.as_object() {
                                let alen = attrs
                                    .get(boa_engine::js_string!("length"), ctx)
                                    .ok()
                                    .and_then(|v| v.as_number())
                                    .unwrap_or(0.0)
                                    as u32;
                                let mut fidx: Option<u32> = None;
                                for i in 0..alen {
                                    if let Ok(a) = attrs.get(i as u32, ctx) {
                                        if let Some(ao) = a.as_object() {
                                            if let Ok(an) =
                                                ao.get(boa_engine::js_string!("name"), ctx)
                                            {
                                                if an
                                                    .as_string()
                                                    .map(|s| {
                                                        s.to_std_string_escaped()
                                                            .to_ascii_lowercase()
                                                    })
                                                    .as_deref()
                                                    == Some("class")
                                                {
                                                    fidx = Some(i);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                let atr = boa_engine::object::JsObject::with_object_proto(
                                    ctx.intrinsics(),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("name"),
                                    pd2(JsValue::from(boa_engine::js_string!("class"))),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("value"),
                                    pd2(JsValue::from(boa_engine::js_string!(new_cn))),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("namespaceURI"),
                                    pd2(JsValue::null()),
                                );
                                let _ = atr.insert_property(
                                    boa_engine::js_string!("localName"),
                                    pd2(JsValue::from(boa_engine::js_string!("class"))),
                                );
                                if let Some(idx) = fidx {
                                    let _ = attrs.insert_property(idx, pd2(atr.into()));
                                } else {
                                    let _ = attrs.insert_property(alen, pd2(atr.into()));
                                    let _ = attrs.insert_property(
                                        boa_engine::js_string!("length"),
                                        pd2(JsValue::from(alen + 1)),
                                    );
                                }
                            }
                        }
                        return Ok(JsValue::from(should_add));
                    }
                }
                Ok(JsValue::from(false))
            });
            let _ = class_list.insert_property(
                boa_engine::js_string!("toggle"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_toggle).build(),
                )),
            );
            // replace(old, new)
            let cl_replace = NativeFunction::from_copy_closure(|this, args, ctx| {
                let old = arg_to_string(args, 0, ctx);
                let new = arg_to_string(args, 1, ctx);
                // Validate tokens per DOM spec: empty → SyntaxError, whitespace → InvalidCharacterError.
                for token in &[&old, &new] {
                    if token.is_empty() {
                        return Err(boa_engine::JsNativeError::syntax()
                            .with_message("SyntaxError: The token must not be empty.")
                            .into());
                    }
                    if token.contains(|c: char| c.is_ascii_whitespace()) {
                        return Err(boa_engine::JsNativeError::typ().with_message("InvalidCharacterError: The token must not contain ASCII whitespace.").into());
                    }
                }
                if let Some(o) = this.as_object() {
                    let el = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object());
                    if let Some(el_obj) = el {
                        let cn = el_obj
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        let mut classes: Vec<String> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !classes.contains(&t.to_string()) {
                                classes.push(t.to_string());
                            }
                        }
                        if let Some(idx) = classes.iter().position(|c| c == &old) {
                            classes[idx] = new;
                            let new_cn = classes.join(" ");
                            let pd2 = |v: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(v)
                                    .writable(true)
                                    .enumerable(true)
                                    .configurable(true)
                                    .build()
                            };
                            let _ = el_obj.insert_property(
                                boa_engine::js_string!("className"),
                                pd2(JsValue::from(boa_engine::js_string!(new_cn))),
                            );
                            return Ok(JsValue::from(true));
                        }
                    }
                }
                Ok(JsValue::from(false))
            });
            let _ = class_list.insert_property(
                boa_engine::js_string!("replace"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_replace).build(),
                )),
            );
            // length as dynamic getter — counts tokens from element's className.
            let cl_len_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    let el = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object());
                    if let Some(el_obj) = el {
                        let cn = el_obj
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        // Normalize: count only unique tokens.
                        let mut seen: Vec<&str> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !seen.contains(&t) {
                                seen.push(t);
                            }
                        }
                        return Ok(JsValue::from(seen.len() as u32));
                    }
                }
                Ok(JsValue::from(0u32))
            });
            let cl_len_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_len_get).build();
            let _ = class_list.insert_property(
                boa_engine::js_string!("length"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cl_len_fn)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // toString() — returns the element's className string.
            let cl_ts = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    let el = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object());
                    if let Some(el_obj) = el {
                        let cn = el_obj
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        return Ok(JsValue::from(boa_engine::js_string!(cn)));
                    }
                }
                Ok(JsValue::from(boa_engine::js_string!("")))
            });
            let _ = class_list.insert_property(
                boa_engine::js_string!("toString"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_ts).build(),
                )),
            );
            // item(index) — returns the class at index.
            let cl_item = NativeFunction::from_copy_closure(|this, args, ctx| {
                let idx_raw = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
                if idx_raw < 0.0 {
                    return Ok(JsValue::null());
                }
                let idx = idx_raw as u32;
                if let Some(o) = this.as_object() {
                    let el = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object());
                    if let Some(el_obj) = el {
                        let cn = el_obj
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        let classes: Vec<&str> = cn.split_whitespace().collect();
                        if idx < classes.len() as u32 {
                            return Ok(JsValue::from(boa_engine::js_string!(
                                classes[idx as usize]
                            )));
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let _ = class_list.insert_property(
                boa_engine::js_string!("item"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_item).build(),
                )),
            );
            // classList is inserted AFTER set_element_prototype to avoid Boa bug
            // where changing prototype makes own properties inaccessible.
            // Element navigation properties (all null/empty for new elements).
            let _ = handle.insert_property(
                boa_engine::js_string!("children"),
                pd(JsValue::from(
                    boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()),
                )),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("childElementCount"),
                pd(JsValue::from(0u32)),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("firstElementChild"),
                pd(JsValue::null()),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("lastElementChild"),
                pd(JsValue::null()),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("nextElementSibling"),
                pd(JsValue::null()),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("previousElementSibling"),
                pd(JsValue::null()),
            );
            // firstChild/lastChild/childNodes are set as accessors by make_element_handle.
            let _ =
                handle.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
            let _ = handle
                .insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
            let doc_val = ctx
                .global_object()
                .get(boa_engine::js_string!("document"), ctx)
                .unwrap_or(JsValue::null());
            let _ = handle
                .insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val.clone()));
            // baseURI = document.URL (about:blank by default)
            let base_uri = doc_val
                .as_object()
                .and_then(|d| d.get(boa_engine::js_string!("URL"), ctx).ok())
                .unwrap_or(JsValue::from(boa_engine::js_string!("about:blank")));
            let _ = handle.insert_property(boa_engine::js_string!("baseURI"), pd(base_uri));
            let _ = handle.insert_property(
                boa_engine::js_string!("isConnected"),
                pd(JsValue::from(false)),
            );
            // Set the prototype chain so instanceof works.
            set_element_prototype(&handle, &tag, ctx);
            // classList MUST be after set_element_prototype — Boa bug workaround.
            // Store in _classListCache so the getter can access it (read-only per spec).
            let _ = handle.insert_property(
                boa_engine::js_string!("_classListCache"),
                pd(class_list.into()),
            );
            // classList as read-only getter — setter is no-op per spec.
            let cl_getter = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cl) = o.get(boa_engine::js_string!("_classListCache"), ctx) {
                        if !cl.is_null() && !cl.is_undefined() {
                            return Ok(cl);
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let cl_noop =
                NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
            let cl_get_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_getter).build();
            let cl_set_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_noop).build();
            let _ = handle.insert_property(
                boa_engine::js_string!("classList"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cl_get_fn)
                    .set(cl_set_fn)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
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
            let ns = if ns_is_null {
                String::new()
            } else {
                arg_string(args, 0)
            };
            let qname_raw = args.get(1).cloned().unwrap_or(JsValue::null());
            let qname = if qname_raw.is_null() || qname_raw.is_undefined() {
                "null".to_string()
            } else {
                arg_to_string(args, 1, ctx)
            };
            // Validate qualifiedName per spec.
            if qname.is_empty() {
                return Err(boa_engine::JsNativeError::typ()
                    .with_message("INVALID_CHARACTER_ERR")
                    .into());
            }
            // Check first character is valid NameStartChar.
            if !qname
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
                .unwrap_or(false)
            {
                // Check for INVALID_CHARACTER_ERR (invalid first char).
                if qname
                    .chars()
                    .next()
                    .map(|c| {
                        c == '<'
                            || c == '>'
                            || c == '}'
                            || c == '/'
                            || c == '\\'
                            || c.is_ascii_digit()
                            || c == '-'
                            || c == '.'
                            || c == ' '
                            || c == '^'
                            || c == '|'
                            || c == '&'
                            || c == '?'
                            || c == '*'
                            || c == '('
                            || c == ')'
                            || c == '+'
                            || c == '='
                            || c == '@'
                            || c == '$'
                            || c == '%'
                            || c == '#'
                            || c == '~'
                            || c == '!'
                            || c == '"'
                            || c == '\''
                            || c == '`'
                    })
                    .unwrap_or(false)
                {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("INVALID_CHARACTER_ERR")
                        .into());
                }
            }
            // Check for invalid characters anywhere.
            for c in qname.chars() {
                if c == '<' || c == '>' || c == ' ' || c == '"' || c == '\'' {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("INVALID_CHARACTER_ERR")
                        .into());
                }
            }
            // NAMESPACE_ERR: starts with ':' in non-null namespace → INVALID_CHARACTER_ERR.
            if qname.starts_with(':') {
                if ns_is_null || ns.is_empty() {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("INVALID_CHARACTER_ERR")
                        .into());
                }
                // null/empty namespace + ':' prefix → INVALID_CHARACTER_ERR
            }
            // Note: "::" in qname is VALID per spec (e.g., "prefix::local" → prefix="prefix", localName=":local").
            // NAMESPACE_ERR checks per spec.
            if let Some(idx) = qname.find(':') {
                let prefix_str = &qname[..idx];
                let local_name = &qname[idx + 1..];
                // Namespace is null/empty but qualifiedName has prefix → NAMESPACE_ERR.
                if ns_is_null || ns.is_empty() {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR")
                        .into());
                }
                // Prefix 'xml' but namespace is not XML namespace.
                if prefix_str == "xml" && ns != "http://www.w3.org/XML/1998/namespace" {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR")
                        .into());
                }
                // Prefix 'xmlns' but namespace is not XMLNS namespace.
                if prefix_str == "xmlns" && ns != "http://www.w3.org/2000/xmlns/" {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR")
                        .into());
                }
                // localName 'xmlns' but namespace is not XMLNS namespace.
                if local_name == "xmlns" && ns != "http://www.w3.org/2000/xmlns/" {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR")
                        .into());
                }
            } else {
                // No prefix. localName 'xmlns' without XMLNS namespace → NAMESPACE_ERR.
                if qname == "xmlns" && ns != "http://www.w3.org/2000/xmlns/" {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message("NAMESPACE_ERR")
                        .into());
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
                qname
                    .chars()
                    .map(|c| {
                        if c.is_ascii_lowercase() {
                            c.to_ascii_uppercase()
                        } else {
                            c
                        }
                    })
                    .collect::<String>()
            } else {
                qname.clone()
            };
            let tag_name_clone = tag_name.clone();
            let pid = {
                let mut bb = b.borrow_mut();
                let pid = bb.next_pending;
                bb.next_pending += 1;
                bb.pending
                    .insert(pid, (local.to_string(), String::new(), Vec::new()));
                pid
            };
            let handle = make_element_handle(ctx, Gc::clone(b), 0, Some(pid))?;
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = handle.insert_property(
                boa_engine::js_string!("tagName"),
                pd(JsValue::from(boa_engine::js_string!(tag_name))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("nodeName"),
                pd(JsValue::from(boa_engine::js_string!(tag_name_clone))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("localName"),
                pd(JsValue::from(boa_engine::js_string!(local))),
            );
            // namespaceURI: null if namespace was null/undefined/empty, else the namespace.
            let ns_val = if ns_is_null || ns.is_empty() {
                JsValue::null()
            } else {
                JsValue::from(boa_engine::js_string!(ns.clone()))
            };
            let _ = handle.insert_property(boa_engine::js_string!("namespaceURI"), pd(ns_val));
            // prefix: null if no colon, else the prefix part.
            let _ = handle.insert_property(
                boa_engine::js_string!("prefix"),
                pd(if prefix.is_empty() {
                    JsValue::null()
                } else {
                    JsValue::from(boa_engine::js_string!(prefix.to_string()))
                }),
            );
            let _ =
                handle.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(1u32)));
            // nodeValue is set as an accessor (always null) by make_element_handle.
            // textContent is set as an accessor by make_element_handle.
            let _ = handle.insert_property(
                boa_engine::js_string!("id"),
                pd(JsValue::from(boa_engine::js_string!(""))),
            );
            let _ = handle.insert_property(
                boa_engine::js_string!("className"),
                pd(JsValue::from(boa_engine::js_string!(""))),
            );
            // ownerDocument = the global document.
            let doc_val = ctx
                .global_object()
                .get(boa_engine::js_string!("document"), ctx)
                .unwrap_or(JsValue::null());
            let _ = handle.insert_property(boa_engine::js_string!("ownerDocument"), pd(doc_val));
            let attrs_map = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = attrs_map
                .insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
            let attrs_clone = attrs_map.clone();
            let _ =
                handle.insert_property(boa_engine::js_string!("attributes"), pd(attrs_map.into()));
            // classList — minimal DOMTokenList for createElementNS elements.
            let cl_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("_element"),
                pd(JsValue::from(handle.clone())),
            );
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("_attrs"),
                pd(JsValue::from(attrs_clone)),
            );
            // contains
            let cl_c = NativeFunction::from_copy_closure(|this, args, ctx| {
                let token_str = arg_to_string(args, 0, ctx);
                if let Some(o) = this.as_object() {
                    if let Some(el) = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        let cn = el
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        return Ok(JsValue::from(cn.split_whitespace().any(|c| c == token_str)));
                    }
                }
                Ok(JsValue::from(false))
            });
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("contains"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_c).build(),
                )),
            );
            // add
            let cl_a = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Some(el) = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        let cn = el
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        let mut classes: Vec<String> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !classes.contains(&t.to_string()) {
                                classes.push(t.to_string());
                            }
                        }
                        for arg in args.iter() {
                            let t = arg
                                .to_string(ctx)
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default();
                            if !classes.contains(&t) {
                                classes.push(t);
                            }
                        }
                        let nc = classes.join(" ");
                        let pd2 = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = el.insert_property(
                            boa_engine::js_string!("className"),
                            pd2(JsValue::from(boa_engine::js_string!(nc))),
                        );
                    }
                }
                Ok(JsValue::undefined())
            });
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("add"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_a).build(),
                )),
            );
            // remove
            let cl_r = NativeFunction::from_copy_closure(|this, args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Some(el) = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        let cn = el
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        let mut classes: Vec<String> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !classes.contains(&t.to_string()) {
                                classes.push(t.to_string());
                            }
                        }
                        for arg in args.iter() {
                            let t = arg
                                .to_string(ctx)
                                .map(|s| s.to_std_string_escaped())
                                .unwrap_or_default();
                            classes.retain(|c| c != &t);
                        }
                        let nc = classes.join(" ");
                        let pd2 = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = el.insert_property(
                            boa_engine::js_string!("className"),
                            pd2(JsValue::from(boa_engine::js_string!(nc))),
                        );
                    }
                }
                Ok(JsValue::undefined())
            });
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("remove"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_r).build(),
                )),
            );
            // toggle
            let cl_t = NativeFunction::from_copy_closure(|this, args, ctx| {
                let token = arg_to_string(args, 0, ctx);
                let force = args.get(1).and_then(|v| v.as_boolean());
                if let Some(o) = this.as_object() {
                    if let Some(el) = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        let cn = el
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        let mut classes: Vec<String> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !classes.contains(&t.to_string()) {
                                classes.push(t.to_string());
                            }
                        }
                        let present = classes.contains(&token);
                        let should_add = force.unwrap_or(!present);
                        if should_add && !present {
                            classes.push(token);
                        } else if !should_add && present {
                            classes.retain(|c| c != &token);
                        }
                        let nc = classes.join(" ");
                        let pd2 = |v: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(v)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = el.insert_property(
                            boa_engine::js_string!("className"),
                            pd2(JsValue::from(boa_engine::js_string!(nc))),
                        );
                        return Ok(JsValue::from(should_add));
                    }
                }
                Ok(JsValue::from(false))
            });
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("toggle"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_t).build(),
                )),
            );
            // replace(old, new)
            let cl_rp = NativeFunction::from_copy_closure(|this, args, ctx| {
                let old = arg_to_string(args, 0, ctx);
                let new = arg_to_string(args, 1, ctx);
                // Validate tokens per DOM spec.
                for token in &[&old, &new] {
                    if token.is_empty() {
                        return Err(boa_engine::JsNativeError::syntax()
                            .with_message("SyntaxError: The token must not be empty.")
                            .into());
                    }
                    if token.contains(|c: char| c.is_ascii_whitespace()) {
                        return Err(boa_engine::JsNativeError::typ().with_message("InvalidCharacterError: The token must not contain ASCII whitespace.").into());
                    }
                }
                if let Some(o) = this.as_object() {
                    if let Some(el) = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        let cn = el
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        let mut classes: Vec<String> = Vec::new();
                        for t in cn.split_whitespace() {
                            if !classes.contains(&t.to_string()) {
                                classes.push(t.to_string());
                            }
                        }
                        if let Some(idx) = classes.iter().position(|c| c == &old) {
                            classes[idx] = new;
                            let nc = classes.join(" ");
                            let pd2 = |v: JsValue| {
                                boa_engine::property::PropertyDescriptor::builder()
                                    .value(v)
                                    .writable(true)
                                    .enumerable(true)
                                    .configurable(true)
                                    .build()
                            };
                            let _ = el.insert_property(
                                boa_engine::js_string!("className"),
                                pd2(JsValue::from(boa_engine::js_string!(nc))),
                            );
                            return Ok(JsValue::from(true));
                        }
                    }
                }
                Ok(JsValue::from(false))
            });
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("replace"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_rp).build(),
                )),
            );
            // length as getter — dynamically counts tokens from className.
            let cl_len = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Some(el) = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        let cn = el
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        return Ok(JsValue::from(cn.split_whitespace().count() as u32));
                    }
                }
                Ok(JsValue::from(0u32))
            });
            let cl_len_fn =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_len).build();
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("length"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cl_len_fn)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
            // toString
            let cl_ts = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Some(el) = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        let cn = el
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        return Ok(JsValue::from(boa_engine::js_string!(cn)));
                    }
                }
                Ok(JsValue::from(boa_engine::js_string!("")))
            });
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("toString"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_ts).build(),
                )),
            );
            // item(index) — returns the class at index.
            let cl_it = NativeFunction::from_copy_closure(|this, args, ctx| {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
                if let Some(o) = this.as_object() {
                    if let Some(el) = o
                        .get(boa_engine::js_string!("_element"), ctx)
                        .ok()
                        .and_then(|v| v.as_object())
                    {
                        let cn = el
                            .get(boa_engine::js_string!("className"), ctx)
                            .ok()
                            .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                            .unwrap_or_default();
                        let classes: Vec<&str> = cn.split_whitespace().collect();
                        if idx < classes.len() as u32 {
                            return Ok(JsValue::from(boa_engine::js_string!(
                                classes[idx as usize]
                            )));
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let _ = cl_obj.insert_property(
                boa_engine::js_string!("item"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_it).build(),
                )),
            );
            set_element_prototype(&handle, local, ctx);
            // For non-HTML namespace, override prototype to Element.prototype (not HTMLxxxElement).
            if !is_html {
                if let Ok(elem_ctor) = ctx
                    .global_object()
                    .get(boa_engine::js_string!("Element"), ctx)
                {
                    if let Some(ec) = elem_ctor.as_object() {
                        if let Ok(proto_val) = ec.get(boa_engine::js_string!("prototype"), ctx) {
                            if let Some(proto) = proto_val.as_object() {
                                let _ = handle.set_prototype(Some(proto));
                            }
                        }
                    }
                }
            }
            // classList MUST be added after prototype changes — Boa loses own
            // properties when the prototype is modified.
            // Store in internal cache for read-only getter.
            let _ = handle
                .insert_property(boa_engine::js_string!("_classListCache"), pd(cl_obj.into()));
            let cl_getter2 = NativeFunction::from_copy_closure(|this, _args, ctx| {
                if let Some(o) = this.as_object() {
                    if let Ok(cl) = o.get(boa_engine::js_string!("_classListCache"), ctx) {
                        if !cl.is_null() && !cl.is_undefined() {
                            return Ok(cl);
                        }
                    }
                }
                Ok(JsValue::null())
            });
            let cl_noop2 =
                NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined()));
            let cl_get_fn2 =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_getter2).build();
            let cl_set_fn2 =
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), cl_noop2).build();
            let _ = handle.insert_property(
                boa_engine::js_string!("classList"),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(cl_get_fn2)
                    .set(cl_set_fn2)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
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

    // getElementsByTagName on document — needs bridge captures.
    let doc_by_tag = NativeFunction::from_copy_closure_with_captures(
        move |_this, args, b, ctx| {
            let tag = arg_string(args, 0).to_lowercase();
            let bb = b.borrow();
            let matching: Vec<u64> = bb
                .node_props
                .iter()
                .filter(|(_, s)| {
                    s.node_type == 1 && (tag == "*" || s.tag_name.to_lowercase() == tag)
                })
                .map(|(&id, _)| id)
                .collect();
            drop(bb);
            let arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
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
            let _ = arr.insert_property(
                boa_engine::js_string!("length"),
                pd(JsValue::from(matching.len() as u32)),
            );
            let item_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(-1.0) as i32;
                if idx < 0 {
                    return Ok(JsValue::null());
                }
                if let Some(o) = this.as_object() {
                    let len = o
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    if (idx as u32) < len {
                        return Ok(o.get(idx as u32, ctx).unwrap_or(JsValue::null()));
                    }
                }
                Ok(JsValue::null())
            });
            let _ = arr.insert_property(
                boa_engine::js_string!("item"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), item_fn).build(),
                )),
            );
            Ok(arr.into())
        },
        Gc::clone(&bridge),
    );
    // getElementsByClassName on document
    let doc_by_class = NativeFunction::from_copy_closure_with_captures(
        move |_this, args, b, ctx| {
            let cls = arg_string(args, 0);
            let bb = b.borrow();
            let matching: Vec<u64> = bb
                .node_props
                .iter()
                .filter(|(_, s)| {
                    s.node_type == 1 && s.class_name.split_whitespace().any(|c| c == cls)
                })
                .map(|(&id, _)| id)
                .collect();
            drop(bb);
            let arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
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
            let _ = arr.insert_property(
                boa_engine::js_string!("length"),
                pd(JsValue::from(matching.len() as u32)),
            );
            Ok(arr.into())
        },
        Gc::clone(&bridge),
    );

    // querySelectorAll on document — returns empty NodeList with NodeList.prototype.
    let doc_qsa = NativeFunction::from_copy_closure(|_t, _a, ctx| {
        let nl = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = nl.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
        // Set NodeList.prototype.
        if let Ok(nl_ctor) = ctx
            .global_object()
            .get(boa_engine::js_string!("NodeList"), ctx)
        {
            if let Some(nlc) = nl_ctor.as_object() {
                if let Ok(pv) = nlc.get(boa_engine::js_string!("prototype"), ctx) {
                    if let Some(p) = pv.as_object() {
                        let _ = nl.set_prototype(Some(p));
                    }
                }
            }
        }
        Ok(JsValue::from(nl))
    });

    let document = ObjectInitializer::new(ctx)
        .function(get_by_id, boa_engine::js_string!("getElementById"), 1)
        .function(create_el, boa_engine::js_string!("createElement"), 1)
        .function(create_el_ns, boa_engine::js_string!("createElementNS"), 2)
        .function(query_sel, boa_engine::js_string!("querySelector"), 1)
        .function(doc_qsa, boa_engine::js_string!("querySelectorAll"), 1)
        .function(
            doc_by_tag,
            boa_engine::js_string!("getElementsByTagName"),
            1,
        )
        .function(
            doc_by_class,
            boa_engine::js_string!("getElementsByClassName"),
            1,
        )
        .build();

    // Add addEventListener/dispatchEvent to document (EventTarget interface).
    {
        let doc_ref = document.clone();
        let pd = |val: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let doc_for_add = doc_ref.clone();
        let add_ev = NativeFunction::from_copy_closure_with_captures(
            move |_this, args, doc_for_add, ctx| {
                let event_type = arg_string(args, 0);
                let handler = args.get(1).cloned().unwrap_or(JsValue::undefined());
                let lmap = doc_for_add
                    .get(boa_engine::js_string!("_js_listeners"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let lm = match lmap {
                    Some(m) => m,
                    None => {
                        let m = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let pd2 = |val: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(val)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = doc_for_add.insert_property(
                            boa_engine::js_string!("_js_listeners"),
                            pd2(m.clone().into()),
                        );
                        m
                    }
                };
                let type_arr = lm
                    .get(boa_engine::js_string!(event_type.clone()), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let arr = match type_arr {
                    Some(a) => a,
                    None => {
                        let a = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let pd2 = |val: JsValue| {
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(val)
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build()
                        };
                        let _ = a.insert_property(
                            boa_engine::js_string!("length"),
                            pd2(JsValue::from(0u32)),
                        );
                        let _ = lm.insert_property(
                            boa_engine::js_string!(event_type.clone()),
                            pd2(a.clone().into()),
                        );
                        a
                    }
                };
                let len = arr
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                let pd2 = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                // Determine passive: touch/wheel events are passive by default on document.
                let explicit_passive = args
                    .get(2)
                    .and_then(|o| o.as_object())
                    .and_then(|o| o.get(boa_engine::js_string!("passive"), ctx).ok())
                    .and_then(|v| v.as_boolean());
                let is_passive_event = matches!(
                    event_type.as_str(),
                    "touchstart" | "touchmove" | "wheel" | "mousewheel"
                );
                let passive = explicit_passive.unwrap_or(is_passive_event);
                let listener_obj =
                    boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let _ =
                    listener_obj.insert_property(boa_engine::js_string!("callback"), pd2(handler));
                let _ = listener_obj.insert_property(
                    boa_engine::js_string!("passive"),
                    pd2(JsValue::from(passive)),
                );
                let _ = arr.insert_property(len, pd2(listener_obj.into()));
                let _ = arr.insert_property(
                    boa_engine::js_string!("length"),
                    pd2(JsValue::from(len + 1)),
                );
                Ok(JsValue::undefined())
            },
            doc_for_add,
        );
        let doc_for_disp = doc_ref.clone();
        let disp_ev = NativeFunction::from_copy_closure_with_captures(
            move |_this, args, doc_for_disp, ctx| {
                let event = args.first().cloned().unwrap_or(JsValue::null());
                let event_type = event
                    .as_object()
                    .and_then(|o| o.get(boa_engine::js_string!("type"), ctx).ok())
                    .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                    .unwrap_or_default();
                let mut not_canceled = true;
                if let Ok(lmap) = doc_for_disp.get(boa_engine::js_string!("_js_listeners"), ctx) {
                    if let Some(lm) = lmap.as_object() {
                        if let Ok(type_arr) = lm.get(boa_engine::js_string!(event_type), ctx) {
                            if let Some(arr) = type_arr.as_object() {
                                let len =
                                    arr.get(boa_engine::js_string!("length"), ctx)
                                        .ok()
                                        .and_then(|v| v.as_number())
                                        .unwrap_or(0.0) as u32;
                                for i in 0..len {
                                    if let Ok(listener) = arr.get(i as u32, ctx) {
                                        // Listener is {callback, passive} or raw function.
                                        let (callback, is_passive) =
                                            if let Some(lo) = listener.as_object() {
                                                let cb = lo
                                                    .get(boa_engine::js_string!("callback"), ctx)
                                                    .ok()
                                                    .unwrap_or(listener.clone());
                                                let p = lo
                                                    .get(boa_engine::js_string!("passive"), ctx)
                                                    .ok()
                                                    .and_then(|v| v.as_boolean())
                                                    .unwrap_or(false);
                                                (cb, p)
                                            } else {
                                                (listener, false)
                                            };
                                        // Set _passive flag on event.
                                        if let Some(ev_obj) = event.as_object() {
                                            let pd2 = |val: JsValue| {
                                                boa_engine::property::PropertyDescriptor::builder()
                                                    .value(val)
                                                    .writable(true)
                                                    .enumerable(false)
                                                    .configurable(true)
                                                    .build()
                                            };
                                            let _ = ev_obj.insert_property(
                                                boa_engine::js_string!("_passive"),
                                                pd2(JsValue::from(is_passive)),
                                            );
                                        }
                                        if let Some(fn_obj) = callback.as_object() {
                                            if fn_obj.is_callable() {
                                                let _ = fn_obj.call(
                                                    &JsValue::null(),
                                                    &[event.clone()],
                                                    ctx,
                                                );
                                            }
                                        }
                                    }
                                    if let Some(ev_obj) = event.as_object() {
                                        let dp = ev_obj
                                            .get(boa_engine::js_string!("defaultPrevented"), ctx)
                                            .ok()
                                            .and_then(|v| v.as_boolean())
                                            .unwrap_or(false);
                                        if dp {
                                            not_canceled = false;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(JsValue::from(not_canceled))
            },
            doc_for_disp,
        );
        // removeEventListener on the document: drop the matching entry from
        // the document's _js_listeners array. Previously a no-op stub, so a
        // document listener could never be removed (L940 follow-up).
        let rm_ev = NativeFunction::from_copy_closure_with_captures(
            move |_this, args, doc_for_rm, ctx| {
                let event_type = arg_string(args, 0);
                let handler_obj = args.get(1).and_then(|v| v.as_object());
                let lmap = doc_for_rm
                    .get(boa_engine::js_string!("_js_listeners"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let Some(lm) = lmap else {
                    return Ok(JsValue::undefined());
                };
                let Ok(type_val) = lm.get(boa_engine::js_string!(event_type.clone()), ctx) else {
                    return Ok(JsValue::undefined());
                };
                let Some(arr) = type_val.as_object() else {
                    return Ok(JsValue::undefined());
                };
                let len = arr
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                let mut kept: Vec<JsValue> = Vec::new();
                for i in 0..len {
                    let Ok(entry) = arr.get(i, ctx) else { continue };
                    let matches = entry
                        .as_object()
                        .and_then(|o| o.get(boa_engine::js_string!("callback"), ctx).ok())
                        .and_then(|cb| cb.as_object())
                        .zip(handler_obj.as_ref())
                        .map(|(cb, wanted)| boa_engine::object::JsObject::equals(&cb, wanted))
                        .unwrap_or(false);
                    if !matches {
                        kept.push(entry);
                    }
                }
                let pd_rm = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                for (i, v) in kept.iter().enumerate() {
                    let _ = arr.insert_property(i as u32, pd_rm(v.clone()));
                }
                // Drop any tail indices left over from the longer array —
                // insert_property bypasses array [[Set]], so shrinking
                // `length` alone leaves the old own properties behind (they
                // are page-visible through Object.keys).
                for stale in kept.len() as u32..len {
                    let _ = arr
                        .delete_property_or_throw(boa_engine::js_string!(stale.to_string()), ctx);
                }
                let _ = arr.insert_property(
                    boa_engine::js_string!("length"),
                    pd_rm(JsValue::from(kept.len() as u32)),
                );
                Ok(JsValue::undefined())
            },
            doc_ref.clone(),
        );
        let _ = document.insert_property(
            boa_engine::js_string!("addEventListener"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), add_ev).build(),
            )),
        );
        let _ = document.insert_property(
            boa_engine::js_string!("dispatchEvent"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), disp_ev).build(),
            )),
        );
        let _ = document.insert_property(
            boa_engine::js_string!("removeEventListener"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), rm_ev).build(),
            )),
        );
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
fn make_canvas_handle(ctx: &mut Context, canvas_id: u64) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;

    // `_canvasAlloc` routes appendChild: the handle carries the allocation
    // id, and the DOM node id only exists once the op replay creates the
    // real element (AttachCanvas homes the two together).

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
            boa_engine::js_string!("_canvasAlloc"),
            JsValue::from(canvas_id),
            Attribute::all(),
        )
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
fn make_context_2d(ctx: &mut Context, canvas_id: u64) -> JsResult<JsObject> {
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

    // DOMParser (caniuse 96%+)
    let dp_ctor = NativeFunction::from_copy_closure(move |this, _a, ctx| {
        let obj = this
            .as_object()
            .unwrap_or_else(|| boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()));
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        // parseFromString(str, type) — returns a minimal document.
        let pfs = NativeFunction::from_copy_closure(|_t, args, ctx| {
            let _str = arg_string(args, 0);
            let _typ = arg_string(args, 1).to_ascii_lowercase();
            let doc = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd2 = |v: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(v)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ =
                doc.insert_property(boa_engine::js_string!("nodeType"), pd2(JsValue::from(9u32)));
            let _ = doc.insert_property(
                boa_engine::js_string!("nodeName"),
                pd2(JsValue::from(boa_engine::js_string!("#document"))),
            );
            let _ = doc.insert_property(
                boa_engine::js_string!("contentType"),
                pd2(JsValue::from(boa_engine::js_string!(_typ.clone()))),
            );
            let _ = doc.insert_property(
                boa_engine::js_string!("documentElement"),
                pd2(JsValue::null()),
            );
            if let Ok(dc) = ctx
                .global_object()
                .get(boa_engine::js_string!("Document"), ctx)
            {
                if let Some(dco) = dc.as_object() {
                    if let Ok(pv) = dco.get(boa_engine::js_string!("prototype"), ctx) {
                        if let Some(p) = pv.as_object() {
                            let _ = doc.set_prototype(Some(p));
                        }
                    }
                }
            }
            Ok(JsValue::from(doc))
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("parseFromString"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), pfs).build(),
            )),
        );
        Ok(obj.into())
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("DOMParser"), 0, dp_ctor);

    // ── fetch (caniuse: 98%+) ── Returns a Promise-like object.
    let fetch_fn = NativeFunction::from_copy_closure(|_t, _args, ctx| {
        let resp = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = resp.insert_property(boa_engine::js_string!("ok"), pd(JsValue::from(false)));
        let _ = resp.insert_property(boa_engine::js_string!("status"), pd(JsValue::from(0u32)));
        let _ = resp.insert_property(
            boa_engine::js_string!("statusText"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        let _ = resp.insert_property(
            boa_engine::js_string!("url"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        // text() method returns empty string.
        let text_fn = NativeFunction::from_copy_closure(|_t, _a, _ctx| {
            Ok(JsValue::from(boa_engine::js_string!("")))
        });
        let _ = resp.insert_property(
            boa_engine::js_string!("text"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), text_fn.clone())
                    .build(),
            )),
        );
        // json() method returns empty object.
        let json_fn = NativeFunction::from_copy_closure(|_t, _a, ctx| {
            Ok(JsValue::from(
                boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()),
            ))
        });
        let _ = resp.insert_property(
            boa_engine::js_string!("json"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), json_fn).build(),
            )),
        );
        // Return a thenable (Promise-like).
        let thenable = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let resp_ref = resp.clone();
        let then_fn = NativeFunction::from_copy_closure_with_captures(
            move |_t, args: &[boa_engine::JsValue], resp_ref, ctx| {
                let cb = args.first().cloned().unwrap_or(JsValue::undefined());
                if let Some(cb_fn) = cb.as_object() {
                    if cb_fn.is_callable() {
                        let _ = cb_fn.call(
                            &JsValue::undefined(),
                            &[JsValue::from(resp_ref.clone())],
                            ctx,
                        );
                    }
                }
                Ok(JsValue::undefined())
            },
            resp_ref,
        );
        let _ = thenable.insert_property(
            boa_engine::js_string!("then"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), then_fn).build(),
            )),
        );
        let _ = thenable.insert_property(
            boa_engine::js_string!("catch"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(
                    ctx.realm(),
                    NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined())),
                )
                .build(),
            )),
        );
        Ok(JsValue::from(thenable))
    });
    let _ = ctx.register_global_callable(boa_engine::js_string!("fetch"), 1, fetch_fn);

    // ── CSSStyleDeclaration (caniuse: 98%+) ── Minimal stub.
    let csssd_ctor = NativeFunction::from_copy_closure(move |this, _a, ctx| {
        let obj = this
            .as_object()
            .unwrap_or_else(|| boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()));
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = obj.insert_property(
            boa_engine::js_string!("cssText"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        let _ = obj.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
        let _ = obj.insert_property(
            boa_engine::js_string!("cssFloat"),
            pd(JsValue::from(boa_engine::js_string!(""))),
        );
        let get_pv = NativeFunction::from_copy_closure(|_t, _a, ctx| {
            Ok(JsValue::from(boa_engine::js_string!("")))
        });
        let _ = obj.insert_property(
            boa_engine::js_string!("getPropertyValue"),
            pd(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), get_pv).build(),
            )),
        );
        Ok(obj.into())
    });
    let _ =
        ctx.register_global_callable(boa_engine::js_string!("CSSStyleDeclaration"), 0, csssd_ctor);

    // ── ServiceWorker / navigator.serviceWorker (caniuse: 96%+) ── Stub.
    let sw_container = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let sw_pd = |v: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(v)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    let _ =
        sw_container.insert_property(boa_engine::js_string!("controller"), sw_pd(JsValue::null()));
    // register() returns a thenable.
    let sw_reg = NativeFunction::from_copy_closure(|_t, _a, ctx| {
        let thenable = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd3 = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = thenable.insert_property(
            boa_engine::js_string!("then"),
            pd3(JsValue::from(
                boa_engine::object::FunctionObjectBuilder::new(
                    ctx.realm(),
                    NativeFunction::from_copy_closure(|_t, _a, _ctx| Ok(JsValue::undefined())),
                )
                .build(),
            )),
        );
        Ok(JsValue::from(thenable))
    });
    let _ = sw_container.insert_property(
        boa_engine::js_string!("register"),
        sw_pd(JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), sw_reg).build(),
        )),
    );
    let _ = sw_container.insert_property(
        boa_engine::js_string!("ready"),
        sw_pd(JsValue::from(
            boa_engine::object::JsObject::with_object_proto(ctx.intrinsics()),
        )),
    );
    if let Ok(nav_val) = ctx
        .global_object()
        .get(boa_engine::js_string!("navigator"), ctx)
    {
        if let Some(nav) = nav_val.as_object() {
            let _ = nav.insert_property(
                boa_engine::js_string!("serviceWorker"),
                sw_pd(JsValue::from(sw_container)),
            );
        }
    }
}

/// Replace `o`'s `_children` mirror with a single text node holding `s`
/// (or clear it for an empty `s`). Shared by every element text setter
/// (`textContent` accessor, `setText`, `setHTML`) so the JS-side mirror and
/// the op-applied tree never disagree about the last write — before this,
/// `setText`/`setHTML` updated the DOM but left the mirror stale, and the
/// `textContent` getter (which prefers `_children`) read back the old value
/// (L940 getter-precedence, three shapes).
fn rebuild_children_mirror(o: &JsObject, s: &str, ctx: &mut Context) {
    let pd = |val: JsValue| {
        boa_engine::property::PropertyDescriptor::builder()
            .value(val)
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build()
    };
    // Clear parentNode on all existing children.
    if let Ok(cv) = o.get(boa_engine::js_string!("_children"), ctx) {
        if let Some(ca) = cv.as_object() {
            let ca = ca.clone();
            let len = ca
                .get(boa_engine::js_string!("length"), ctx)
                .ok()
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as u32;
            for i in 0..len {
                if let Ok(child) = ca.get(i as u32, ctx) {
                    if let Some(co) = child.as_object() {
                        let co = co.clone();
                        let _ = co.insert_property(
                            boa_engine::js_string!("parentNode"),
                            pd(JsValue::null()),
                        );
                    }
                }
            }
        }
    }
    if s.is_empty() {
        // Empty string → no children.
        let empty_arr = new_array(ctx);
        let _ = o.insert_property(boa_engine::js_string!("_children"), pd(empty_arr.into()));
    } else {
        // Replace all children with a single text node.
        let text_node = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ =
            text_node.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32)));
        let _ = text_node.insert_property(
            boa_engine::js_string!("_data"),
            pd(JsValue::from(boa_engine::js_string!(s.to_string()))),
        );
        let _ = text_node.insert_property(
            boa_engine::js_string!("data"),
            pd(JsValue::from(boa_engine::js_string!(s.to_string()))),
        );
        let _ = text_node.insert_property(
            boa_engine::js_string!("nodeValue"),
            pd(JsValue::from(boa_engine::js_string!(s.to_string()))),
        );
        let _ = text_node.insert_property(
            boa_engine::js_string!("textContent"),
            pd(JsValue::from(boa_engine::js_string!(s.to_string()))),
        );
        let _ = text_node.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("#text"))),
        );
        let _ = text_node.insert_property(
            boa_engine::js_string!("parentNode"),
            pd(JsValue::from(o.clone())),
        );
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
        let new_arr = new_array(ctx);
        let _ = new_arr.push(JsValue::from(text_node), ctx);
        let _ = o.insert_property(boa_engine::js_string!("_children"), pd(new_arr.into()));
    }
}

fn make_element_handle(
    ctx: &mut Context,
    bridge: Gc<GcRefCell<Bridge>>,
    nid: u64,
    pending: Option<u32>,
) -> JsResult<JsObject> {
    use boa_engine::object::ObjectInitializer;

    // Pre-create objects that need ctx.intrinsics() before borrowing ctx mutably.
    let empty_arr1 = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let empty_arr2 = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    // `el.style` — a style object whose `cssText` writes the element's
    // `style` attribute. Without it `el.style.cssText = v` threw a TypeError
    // ("cannot convert 'null' to object"), and the retired `js_interactive`
    // bridge hid that by rewriting the source to `setAttribute('style', v)`.
    // Only `cssText` is implemented; per-property access (`el.style.color`)
    // is a separate, larger surface. Built before `init` because `init` takes
    // a mutable borrow of `ctx`.
    // Capture the pending pid for the style object's writes: `this` inside
    // the closure is the style object itself, which carries no `_pending`.
    let style_pending = pending;
    let style_css_text_set = NativeFunction::from_copy_closure_with_captures(
        move |this, args, b, _ctx| {
            let value = arg_string(args, 0);
            // `nid == 0` is a PENDING handle (a `createElement` node with no
            // tree id yet). Pushing an op against it panics in blitz
            // (`SlotMap` invalid key), which is strictly worse than the caught
            // JS TypeError this path used to raise. For a pending handle the
            // value rides the pending tuple instead (branch below), so it
            // survives `appendChild`.
            if nid != 0 {
                let mut bb = b.borrow_mut();
                bb.ops.push(Op::SetAttr {
                    node_id: nid,
                    name: "style".to_string(),
                    value: value.clone(),
                });
                // Keep the snapshot attrs in step so a FRESHLY-FETCHED handle
                // (getElementById) seeds its style object's cssText from the
                // snapshot and sees this write (round-2 S2).
                if let Some(snap) = bb.node_props.get_mut(&nid) {
                    match snap.attrs.iter().position(|(n, _)| n == "style") {
                        Some(i) => snap.attrs[i] = ("style".to_string(), value.clone()),
                        None => snap.attrs.push(("style".to_string(), value.clone())),
                    }
                }
            } else if let Some(pid) = style_pending {
                // PENDING handle (`nid == 0`): carrying the write in the
                // pending tuple lets a detached node's style survive its
                // append; pushing an op against id 0 would panic in blitz.
                // Replace-by-name, same last-wins reasoning as setAttribute.
                let mut bb = b.borrow_mut();
                if let Some(entry) = bb.pending.get_mut(&pid) {
                    match entry.2.iter().position(|(n, _)| n == "style") {
                        Some(i) => entry.2[i] = ("style".to_string(), value.clone()),
                        None => entry.2.push(("style".to_string(), value.clone())),
                    }
                }
            }
            if let Some(o) = this.as_object() {
                let _ = o.insert_property(
                    boa_engine::js_string!("_arisCssTextCache"),
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(JsValue::from(boa_engine::js_string!(value)))
                        .writable(true)
                        .enumerable(false)
                        .configurable(true)
                        .build(),
                );
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    let style_css_text_set_fn =
        boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), style_css_text_set).build();
    // `cssText` must be a proper ACCESSOR. The old definition carried both
    // `.value("")` and `.set(...)` (a contradictory descriptor) AND the setter
    // replaced `cssText` with a data property on first write — so the second
    // `el.style.cssText = …` assignment hit a plain data property and pushed
    // no op. The getter reads the cache the setter maintains; there is no
    // parse-time seed, so `cssText` on a parsed style attribute reads "".
    let style_css_text_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(o) = this.as_object()
            && let Ok(v) = o.get(boa_engine::js_string!("_arisCssTextCache"), ctx)
            && !v.is_undefined()
        {
            return Ok(v);
        }
        Ok(JsValue::from(boa_engine::js_string!("")))
    });
    let style_css_text_get_fn =
        boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), style_css_text_get).build();
    let style_obj = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
    let _ = style_obj.insert_property(
        boa_engine::js_string!("cssText"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(style_css_text_get_fn)
            .set(style_css_text_set_fn)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

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
        // Id-less handles (document.write-materialized elements, which the
        // bridge never saw at parse time) share the node-0 scratch slot —
        // master parity. Blitz node ids are versioned and never 0, so slot 0
        // is never read back by the home lookup (fresh-document ids are all
        // > u32::MAX; pinned by
        // node_props_tests::snapshot_ids_are_live_tree_ids). Known limitation
        // (L940): these buffers are never composited — the replay-created
        // node carries no layout, master included.
        let node_id = read_handle_id(_this).unwrap_or(0);
        let alloc = CANVAS_HOME.with(|home| {
            let mut hm = home.borrow_mut();
            *hm.entry(node_id).or_insert_with(|| alloc_canvas(300, 150))
        });
        Ok(make_context_2d(ctx, alloc)?.into())
    });
    init.function(get_ctx, boa_engine::js_string!("getContext"), 1);

    // setText / textContent setter
    let set_text = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            let v = arg_string(args, 0);
            // Keep the `_children` mirror in step (same as the textContent
            // accessor setter): otherwise the getter — which prefers
            // `_children` — reads the stale pre-write text.
            if let Some(o) = this.as_object() {
                rebuild_children_mirror(&o, &v, ctx);
            }
            if let Some(nid) = read_handle_id(this) {
                let mut bb = b.borrow_mut();
                // Keep the node snapshot in step so a freshly-fetched handle
                // reads the new text (round-4 S2: without this, `setText` left
                // the snapshot stale while the DOM moved on).
                if let Some(snap) = bb.node_props.get_mut(&nid) {
                    snap.text_content = v.clone();
                }
                bb.ops.push(Op::SetText {
                    node_id: nid,
                    value: v.clone(),
                });
            } else if let Some(pid) = read_pending(this) {
                // PENDING handle (a detached `createElement` node): the write
                // must ride the pending tuple so `Op::AppendChild` carries it
                // into the tree when the node is attached. Previously dropped
                // at the source — the most common dynamic-DOM pattern
                // (create → set text → append) silently produced empty nodes.
                if let Some(entry) = b.borrow_mut().pending.get_mut(&pid) {
                    entry.1 = v.clone();
                }
            }
            // Do NOT write a `textContent` data property here: that would
            // replace the accessor installed by `make_element_handle` and
            // silently break later reads through this handle (round-4 S2).
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(set_text, boa_engine::js_string!("setText"), 1);

    // setHTML / innerHTML setter (strips tags)
    let set_html = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            let text = strip_tags(&arg_string(args, 0));
            // Mirror in step (see setText).
            if let Some(o) = this.as_object() {
                rebuild_children_mirror(&o, &text, ctx);
            }
            if let Some(nid) = read_handle_id(this) {
                let mut bb = b.borrow_mut();
                // Keep the node snapshot in step so a freshly-fetched handle
                // reads the new text (same reason as `setText`, round-4 S2).
                if let Some(snap) = bb.node_props.get_mut(&nid) {
                    snap.text_content = text.clone();
                }
                bb.ops.push(Op::SetText {
                    node_id: nid,
                    value: text.clone(),
                });
            } else if let Some(pid) = read_pending(this) {
                // PENDING handle (a detached `createElement` node): the write
                // rides the pending tuple so it lands when the node is
                // attached — same source-drop fix as the textContent / setText
                // branches.
                if let Some(entry) = b.borrow_mut().pending.get_mut(&pid) {
                    entry.1 = text.clone();
                }
            }
            // Do NOT write a `textContent` data property here: it would
            // replace the accessor installed by `make_element_handle` and
            // silently break later reads through this handle (the same
            // clobber `setText` had, fixed in its round-4 S2).
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
            } else if let Some(pid) = read_pending(this) {
                // PENDING handle: carry the attribute in the pending tuple so
                // it lands when the node is attached (same source-drop fix as
                // the textContent branch above). Replace an existing entry of
                // the same name — pushing would stack duplicates and the DOM
                // resolves first-wins while getAttribute is last-wins (a
                // silent JS-vs-DOM split, round-2 S1).
                let mut bb = b.borrow_mut();
                if let Some(entry) = bb.pending.get_mut(&pid) {
                    match entry.2.iter().position(|(n, _)| *n == name) {
                        Some(i) => entry.2[i] = (name.clone(), value.clone()),
                        None => entry.2.push((name.clone(), value.clone())),
                    }
                }
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
                // `style` is special: the handle carries a style OBJECT
                // (installed by `make_element_handle`) whose `cssText` the
                // page reads back. Inserting a string data property here
                // overwrote that object — `el.style` became a primitive and
                // every later `el.style.cssText = …` silently no-op'ed
                // (L940). Update the object instead.
                if name == "style" {
                    if let Ok(style_val) = o.get(boa_engine::js_string!("style"), ctx)
                        && let Some(style_obj) = style_val.as_object()
                    {
                        // Route through the cssText ACCESSOR (`JsObject::set`
                        // fires its setter): a data-property insert here would
                        // replace the setter and silently no-op every later
                        // `el.style.cssText = …`. The setter re-pushes an
                        // identical SetAttr op — idempotent under drain.
                        let _ = style_obj.set(
                            boa_engine::js_string!("cssText"),
                            JsValue::from(boa_engine::js_string!(value.clone())),
                            false,
                            ctx,
                        );
                    } else {
                        // No style object (defensive): fall back to the
                        // generic data property.
                        let _ = o.insert_property(
                            boa_engine::js_string!(name.clone()),
                            pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                        );
                    }
                } else {
                    // Also set the attribute as a direct property.
                    let _ = o.insert_property(
                        boa_engine::js_string!(name.clone()),
                        pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                    );
                }
                // Update the attributes NamedNodeMap: find or create the Attr.
                if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(attrs) = attrs_val.as_object() {
                        // Build the Attr object.
                        let attr_obj =
                            boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("name"),
                            pd(JsValue::from(boa_engine::js_string!(name.clone()))),
                        );
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("nodeName"),
                            pd(JsValue::from(boa_engine::js_string!(name.clone()))),
                        );
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("value"),
                            pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                        );
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("nodeValue"),
                            pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                        );
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("textContent"),
                            pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                        );
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("localName"),
                            pd(JsValue::from(boa_engine::js_string!(name.clone()))),
                        );
                        let _ = attr_obj
                            .insert_property(boa_engine::js_string!("prefix"), pd(JsValue::null()));
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("namespaceURI"),
                            pd(JsValue::null()),
                        );
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("specified"),
                            pd(JsValue::from(true)),
                        );
                        let _ = attr_obj.insert_property(
                            boa_engine::js_string!("ownerElement"),
                            pd(JsValue::from(o.clone())),
                        );
                        // Find if this attr already exists, else append.
                        let len = attrs
                            .get(boa_engine::js_string!("length"), ctx)
                            .ok()
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0) as u32;
                        let mut found_idx: Option<u32> = None;
                        for i in 0..len {
                            if let Ok(av) = attrs.get(i as u32, ctx) {
                                if let Some(ao) = av.as_object() {
                                    if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                        if an
                                            .as_string()
                                            .map(|s| s.to_std_string_escaped())
                                            .as_deref()
                                            == Some(&name)
                                        {
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
    init.property(
        boa_engine::js_string!("style"),
        style_obj,
        boa_engine::property::Attribute::all(),
    );

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
            // For "class", empty string means attribute was removed — fall through.
            if name != "class" || v.as_string().map_or(true, |s| !s.is_empty()) {
                return Ok(v);
            }
        }
        // Fallback: check id/class special cases. Only return non-null if
        // the attribute exists in the NamedNodeMap (not just className).
        if name == "id" {
            if let Some(o) = &obj {
                return Ok(o
                    .get(boa_engine::js_string!("id"), ctx)
                    .unwrap_or(JsValue::null()));
            }
        } else if name == "class" {
            if let Some(o) = &obj {
                // Always check className first — the NamedNodeMap may be stale
                // for non-HTML elements (Boa prototype change bug).
                let cn = o
                    .get(boa_engine::js_string!("className"), ctx)
                    .unwrap_or(JsValue::null());
                if let Some(cn_s) = cn.as_string() {
                    let s = cn_s.to_std_string_escaped();
                    if !s.is_empty() {
                        return Ok(JsValue::from(boa_engine::js_string!(s)));
                    }
                }
                // className is empty — check NamedNodeMap to distinguish
                // "class was removed" (return null) from edge cases.
                if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(attrs) = attrs_val.as_object() {
                        let len = attrs
                            .get(boa_engine::js_string!("length"), ctx)
                            .ok()
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0) as u32;
                        let mut found = false;
                        for i in 0..len {
                            if let Ok(av) = attrs.get(i as u32, ctx) {
                                if let Some(ao) = av.as_object() {
                                    if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                        if an
                                            .as_string()
                                            .map(|s| s.to_std_string_escaped().to_ascii_lowercase())
                                            .as_deref()
                                            == Some("class")
                                        {
                                            found = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        if found {
                            return Ok(o
                                .get(boa_engine::js_string!("className"), ctx)
                                .unwrap_or(JsValue::null()));
                        }
                    }
                }
            }
            return Ok(JsValue::null());
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
                    let len = ao
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(attr) = ao.get(i as u32, ctx) {
                            if let Some(ao2) = attr.as_object() {
                                if let Ok(an) = ao2.get(boa_engine::js_string!("name"), ctx) {
                                    if let Some(an_s) = an.as_string() {
                                        if an_s.to_std_string_escaped().to_ascii_lowercase()
                                            == name_lower.as_str()
                                        {
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
            // Remove from attributes NamedNodeMap.
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    // Find and remove the matching attribute entry.
                    let mut found_idx: Option<u32> = None;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                    if an
                                        .as_string()
                                        .map(|s| s.to_std_string_escaped().to_ascii_lowercase())
                                        .as_deref()
                                        == Some(&name)
                                    {
                                        found_idx = Some(i);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if let Some(idx) = found_idx {
                        // Shift remaining entries down.
                        for i in idx..len - 1 {
                            if let Ok(next) = attrs.get((i + 1) as u32, ctx) {
                                let pd2 = |v: JsValue| {
                                    boa_engine::property::PropertyDescriptor::builder()
                                        .value(v)
                                        .writable(true)
                                        .enumerable(true)
                                        .configurable(true)
                                        .build()
                                };
                                let _ = attrs.insert_property(i as u32, pd2(next));
                            }
                        }
                        let _ = attrs.insert_property(
                            boa_engine::js_string!("length"),
                            boa_engine::property::PropertyDescriptor::builder()
                                .value(JsValue::from(len - 1))
                                .writable(true)
                                .enumerable(true)
                                .configurable(true)
                                .build(),
                        );
                    }
                }
            }
            // Clear direct property and special properties.
            let pd_clear = |v: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(v)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            if name == "class" {
                let _ = o.insert_property(
                    boa_engine::js_string!("className"),
                    pd_clear(JsValue::from(boa_engine::js_string!(""))),
                );
                // Clear the direct class property set by setAttribute.
                let _ =
                    o.insert_property(boa_engine::js_string!("class"), pd_clear(JsValue::null()));
            } else if name == "id" {
                let _ = o.insert_property(
                    boa_engine::js_string!("id"),
                    pd_clear(JsValue::from(boa_engine::js_string!(""))),
                );
                let _ = o.insert_property(boa_engine::js_string!("id"), pd_clear(JsValue::null()));
            } else if name == "style" {
                // Clear the style OBJECT's cache — writing `style: null` on
                // the handle would clobber the style object and make the
                // next `el.style.cssText` read THROW (round-2 S2). The style
                // object itself stays; DOM-wise removing the style
                // attribute leaves an empty CSSStyleDeclaration.
                if let Ok(style_val) = o.get(boa_engine::js_string!("style"), ctx)
                    && let Some(style_obj) = style_val.as_object()
                {
                    let _ = style_obj.insert_property(
                        boa_engine::js_string!("_arisCssTextCache"),
                        boa_engine::property::PropertyDescriptor::builder()
                            .value(JsValue::from(boa_engine::js_string!("")))
                            .writable(true)
                            .enumerable(false)
                            .configurable(true)
                            .build(),
                    );
                }
            } else {
                // Clear the direct property.
                let _ = o.insert_property(
                    boa_engine::js_string!(name.clone()),
                    pd_clear(JsValue::null()),
                );
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
                    let len = attrs
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    let mut found = false;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                    if an
                                        .as_string()
                                        .map(|s| s.to_std_string_escaped().to_ascii_lowercase())
                                        .as_deref()
                                        == Some(&lower)
                                    {
                                        found = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    found
                } else {
                    false
                }
            } else {
                false
            };
            match force {
                Some(true) => {
                    // Set to empty string.
                    let sfn = o.get(boa_engine::js_string!("setAttribute"), ctx).ok();
                    if let Some(sv) = sfn {
                        if let Some(sf) = sv.as_object() {
                            let _ = sf.call(
                                &JsValue::from(o.clone()),
                                &[
                                    JsValue::from(boa_engine::js_string!(lower)),
                                    JsValue::from(boa_engine::js_string!("")),
                                ],
                                ctx,
                            );
                        }
                    }
                    return Ok(JsValue::from(true));
                }
                Some(false) => {
                    if exists {
                        let rfn = o.get(boa_engine::js_string!("removeAttribute"), ctx).ok();
                        if let Some(rv) = rfn {
                            if let Some(rf) = rv.as_object() {
                                let _ = rf.call(
                                    &JsValue::from(o.clone()),
                                    &[JsValue::from(boa_engine::js_string!(name))],
                                    ctx,
                                );
                            }
                        }
                    }
                    return Ok(JsValue::from(false));
                }
                None => {
                    if exists {
                        let rfn = o.get(boa_engine::js_string!("removeAttribute"), ctx).ok();
                        if let Some(rv) = rfn {
                            if let Some(rf) = rv.as_object() {
                                let _ = rf.call(
                                    &JsValue::from(o.clone()),
                                    &[JsValue::from(boa_engine::js_string!(name))],
                                    ctx,
                                );
                            }
                        }
                        return Ok(JsValue::from(false));
                    } else {
                        let sfn = o.get(boa_engine::js_string!("setAttribute"), ctx).ok();
                        if let Some(sv) = sfn {
                            if let Some(sf) = sv.as_object() {
                                let _ = sf.call(
                                    &JsValue::from(o.clone()),
                                    &[
                                        JsValue::from(boa_engine::js_string!(lower)),
                                        JsValue::from(boa_engine::js_string!("")),
                                    ],
                                    ctx,
                                );
                            }
                        }
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
                    let len = attrs
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                    if an
                                        .as_string()
                                        .map(|s| s.to_std_string_escaped().to_ascii_lowercase())
                                        .as_deref()
                                        == Some(&name)
                                    {
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
                    let len = attrs
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao
                                    .get(boa_engine::js_string!("localName"), ctx)
                                    .or_else(|_| ao.get(boa_engine::js_string!("name"), ctx))
                                {
                                    if an
                                        .as_string()
                                        .map(|s| s.to_std_string_escaped().to_ascii_lowercase())
                                        .as_deref()
                                        == Some(&name)
                                    {
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
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            // Also set as direct property (case-sensitive for NS attributes).
            let _ = o.insert_property(
                boa_engine::js_string!(local.to_string()),
                pd(JsValue::from(boa_engine::js_string!(value.clone()))),
            );
            // Update the attributes NamedNodeMap.
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let attr_obj =
                        boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("name"),
                        pd(JsValue::from(boa_engine::js_string!(qname.clone()))),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("nodeName"),
                        pd(JsValue::from(boa_engine::js_string!(qname.clone()))),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("value"),
                        pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("nodeValue"),
                        pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("textContent"),
                        pd(JsValue::from(boa_engine::js_string!(value.clone()))),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("localName"),
                        pd(JsValue::from(boa_engine::js_string!(local.to_string()))),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("prefix"),
                        pd(if prefix.is_empty() {
                            JsValue::null()
                        } else {
                            JsValue::from(boa_engine::js_string!(prefix.to_string()))
                        }),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("namespaceURI"),
                        pd(if ns.is_empty() {
                            JsValue::null()
                        } else {
                            JsValue::from(boa_engine::js_string!(ns.clone()))
                        }),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("specified"),
                        pd(JsValue::from(true)),
                    );
                    let _ = attr_obj.insert_property(
                        boa_engine::js_string!("ownerElement"),
                        pd(JsValue::from(o.clone())),
                    );
                    // Find if attr with same localName already exists.
                    let len = attrs
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    let mut found_idx: Option<u32> = None;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("localName"), ctx) {
                                    if an.as_string().map(|s| s.to_std_string_escaped()).as_deref()
                                        == Some(local)
                                    {
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
    });
    init.function(set_attr_ns, boa_engine::js_string!("setAttributeNS"), 3);

    // getAttributeNS(ns, name) — returns value or null. Case-sensitive (no lowercasing).
    let get_attr_ns = NativeFunction::from_copy_closure(|this, args, ctx| {
        let _ns = arg_string(args, 0);
        let name = arg_string(args, 1);
        if let Some(o) = this.as_object() {
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao
                                    .get(boa_engine::js_string!("localName"), ctx)
                                    .or_else(|_| ao.get(boa_engine::js_string!("name"), ctx))
                                {
                                    if an.as_string().map(|s| s.to_std_string_escaped()).as_deref()
                                        == Some(&name)
                                    {
                                        if let Ok(val) =
                                            ao.get(boa_engine::js_string!("value"), ctx)
                                        {
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
    let remove_attr_ns =
        NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
    init.function(
        remove_attr_ns,
        boa_engine::js_string!("removeAttributeNS"),
        2,
    );

    // hasAttributes() — true if attributes.length > 0.
    let has_attributes = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(o) = this.as_object() {
            if let Ok(av) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(ao) = av.as_object() {
                    let len = ao
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0);
                    return Ok(JsValue::from(len > 0.0));
                }
            }
        }
        Ok(JsValue::from(false))
    });
    init.function(has_attributes, boa_engine::js_string!("hasAttributes"), 0);

    // addEventListener
    let add_listener = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            let event_type = arg_to_string(args, 0, ctx);
            let handler = args.get(1).cloned().unwrap_or(JsValue::undefined());
            // Check if options specify passive.
            let explicit_passive = args
                .get(2)
                .and_then(|o| o.as_object())
                .and_then(|o| o.get(boa_engine::js_string!("passive"), ctx).ok())
                .and_then(|v| v.as_boolean());
            // Determine default passive: touchstart, touchmove, wheel, mousewheel
            // are passive by default on window/document/documentElement/body.
            let is_passive_event = matches!(
                event_type.as_str(),
                "touchstart" | "touchmove" | "wheel" | "mousewheel"
            );
            // Check if this target is a passive-by-default target.
            // Window: this === global object. Document: nodeName=#document.
            // documentElement: nodeName=HTML. body: nodeName=BODY.
            let is_window = this
                .as_object()
                .map(|o| {
                    // Check if this is the global object by comparing with ctx.global_object().
                    boa_engine::object::JsObject::equals(&o, &ctx.global_object())
                })
                .unwrap_or(false);
            let target_name = this
                .as_object()
                .and_then(|o| o.get(boa_engine::js_string!("nodeName"), ctx).ok())
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            let is_passive_target = is_window
                || target_name == "#document"
                || target_name == "HTML"
                || target_name == "BODY";
            let passive = explicit_passive.unwrap_or(is_passive_event && is_passive_target);
            if let Some(o) = this.as_object() {
                let pd = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                // Get or create _js_listeners map on this object.
                let listeners = o
                    .get(boa_engine::js_string!("_js_listeners"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let lmap = match listeners {
                    Some(m) => m,
                    None => {
                        let m = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = o.insert_property(
                            boa_engine::js_string!("_js_listeners"),
                            pd(m.clone().into()),
                        );
                        m
                    }
                };
                // Get or create the array for this event type.
                let type_arr = lmap
                    .get(boa_engine::js_string!(event_type.clone()), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let arr = match type_arr {
                    Some(a) => a,
                    None => {
                        let a = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                        let _ = a.insert_property(
                            boa_engine::js_string!("length"),
                            pd(JsValue::from(0u32)),
                        );
                        let _ = lmap.insert_property(
                            boa_engine::js_string!(event_type.clone()),
                            pd(a.clone().into()),
                        );
                        a
                    }
                };
                let len = arr
                    .get(boa_engine::js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                // Store handler as {callback, passive} object.
                let listener_obj =
                    boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
                let _ = listener_obj
                    .insert_property(boa_engine::js_string!("callback"), pd(handler.clone()));
                let _ = listener_obj.insert_property(
                    boa_engine::js_string!("passive"),
                    pd(JsValue::from(passive)),
                );
                let _ = arr.insert_property(len, pd(listener_obj.into()));
                let _ = arr
                    .insert_property(boa_engine::js_string!("length"), pd(JsValue::from(len + 1)));
            }
            // Fire_click registration: stash the callback under a
            // per-registration global name — GC-rooted via the global object,
            // so the handler outlives the handle object it was registered on —
            // and record (node, type, name, handler) for harvest. Several
            // handlers may listen to one (node, type); each gets its own slot.
            // A page script re-run (resize / zoom / reload) re-registers into
            // the SAME slot numbers, and the bind reconciles afterwards, so a
            // replay collapses instead of stacking (see `reconcile_slots`).
            if let Some(nid) = read_handle_id(this) {
                // Unique per registration so several handlers on one
                // (node, type) each get a distinct GC root.
                // Slot numbering is PER (node, type), not global: a replay
                // then lands on exactly the same names for a fixed-order
                // page, which is what makes the harvest collapse duplicates.
                // A changed registration pattern re-uses the same slot range
                // and is reconciled after the run (see `reconcile_slots`).
                let seq = {
                    let mut bb = b.borrow_mut();
                    if bb.slots_active {
                        let key = (nid, event_type.clone());
                        let slot = bb.listener_slots.entry(key).or_insert(0);
                        *slot += 1;
                        *slot - 1
                    } else {
                        bb.next_listener_reg = bb.next_listener_reg.wrapping_add(1);
                        // Offset so an ad-hoc registration cannot alias a
                        // page slot (which starts at 0).
                        1_000_000 + bb.next_listener_reg
                    }
                };
                let name = format!("__aris_listener_{nid}_{event_type}_{seq}");
                b.borrow_mut().listener_actions.push(ListenerAction::Add {
                    node_id: nid,
                    name: name.clone(),
                    event_type: event_type.clone(),
                    handler: handler.clone(),
                });
                let _ = ctx.global_object().insert_property(
                    boa_engine::js_string!(name),
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(handler)
                        .writable(true)
                        .enumerable(false)
                        .configurable(true)
                        .build(),
                );
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(add_listener, boa_engine::js_string!("addEventListener"), 2);

    // dispatchEvent(event) — calls listeners registered via addEventListener
    // + on<type> property handlers.
    let dispatch = NativeFunction::from_copy_closure(|this, args, ctx| {
        let event = args.first().cloned().unwrap_or(JsValue::null());
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
            let _ = ev_obj.insert_property(
                boa_engine::js_string!("currentTarget"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(this.clone())
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
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
                                let dp = ev_obj
                                    .get(boa_engine::js_string!("defaultPrevented"), ctx)
                                    .ok()
                                    .and_then(|v| v.as_boolean())
                                    .unwrap_or(false);
                                if dp {
                                    not_canceled = false;
                                }
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
                            let len = arr
                                .get(boa_engine::js_string!("length"), ctx)
                                .ok()
                                .and_then(|v| v.as_number())
                                .unwrap_or(0.0) as u32;
                            for i in 0..len {
                                if let Ok(listener) = arr.get(i as u32, ctx) {
                                    // Listener is {callback, passive} or raw function.
                                    let (callback, is_passive) =
                                        if let Some(lo) = listener.as_object() {
                                            let cb = lo
                                                .get(boa_engine::js_string!("callback"), ctx)
                                                .ok()
                                                .unwrap_or(JsValue::undefined());
                                            let p = lo
                                                .get(boa_engine::js_string!("passive"), ctx)
                                                .ok()
                                                .and_then(|v| v.as_boolean())
                                                .unwrap_or(false);
                                            (cb, p)
                                        } else {
                                            (listener, false)
                                        };
                                    // Set _passive flag on event so preventDefault is a no-op.
                                    if let Some(ev_obj) = event.as_object() {
                                        let pd2 = |val: JsValue| {
                                            boa_engine::property::PropertyDescriptor::builder()
                                                .value(val)
                                                .writable(true)
                                                .enumerable(false)
                                                .configurable(true)
                                                .build()
                                        };
                                        let _ = ev_obj.insert_property(
                                            boa_engine::js_string!("_passive"),
                                            pd2(JsValue::from(is_passive)),
                                        );
                                    }
                                    if let Some(fn_obj) = callback.as_object() {
                                        if fn_obj.is_callable() {
                                            let _ = fn_obj.call(&this, &[event.clone()], ctx);
                                        } else if let Ok(he) =
                                            fn_obj.get(boa_engine::js_string!("handleEvent"), ctx)
                                        {
                                            if let Some(he_fn) = he.as_object() {
                                                if he_fn.is_callable() {
                                                    let _ = he_fn.call(
                                                        &callback,
                                                        &[event.clone()],
                                                        ctx,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                // Check defaultPrevented after each handler.
                                if let Some(ev_obj) = event.as_object() {
                                    let dp = ev_obj
                                        .get(boa_engine::js_string!("defaultPrevented"), ctx)
                                        .ok()
                                        .and_then(|v| v.as_boolean())
                                        .unwrap_or(false);
                                    if dp {
                                        not_canceled = false;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // WHATWG: currentTarget is null once dispatch completes (after every
        // listener ran; during the walk it stays the dispatch target).
        if let Some(ev_obj) = event.as_object() {
            let _ = ev_obj.insert_property(
                boa_engine::js_string!("currentTarget"),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(JsValue::null())
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build(),
            );
        }
        Ok(JsValue::from(not_canceled))
    });
    init.function(dispatch, boa_engine::js_string!("dispatchEvent"), 1);

    // removeEventListener — removes from _js_listeners.
    let remove_listener = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            let event_type = arg_string(args, 0);
            let handler_obj = args.get(1).and_then(|v| v.as_object());
            // Untrack from the fire_click dispatch map (L940 leftover ②-cycle):
            // without this, a removed click handler kept firing (ghost-firing).
            // Gated on handler IDENTITY: the registered callback is the stashed
            // global, so removing an unrelated/undefined handler must not kill
            // a live listener (R3 S0).
            if let Some(nid) = read_handle_id(this) {
                // Match by handler identity against the registration TABLE
                // (not the stashed global): several (node, type) entries
                // coexist now, and an Add re-stash in the same window would
                // otherwise hide the row we need to drop.
                let target = {
                    let bb = b.borrow();
                    // The row may still be pending in this window's action log
                    // (add and remove in one script), so consult both: the
                    // harvested table first, then the queued Adds.
                    bb.listener_registry
                        .iter()
                        .find(|(node, ty, _, h)| {
                            *node == nid
                                && *ty == event_type
                                && match (&handler_obj, h.as_object()) {
                                    (Some(wanted), Some(actual)) => {
                                        boa_engine::object::JsObject::equals(wanted, &actual)
                                    }
                                    _ => false,
                                }
                        })
                        .map(|(_, _, name, _)| name.clone())
                        .or_else(|| {
                            bb.listener_actions.iter().rev().find_map(|a| match a {
                                ListenerAction::Add {
                                    node_id,
                                    name,
                                    event_type: ty,
                                    handler,
                                } if *node_id == nid
                                    && ty == &event_type
                                    && match (&handler_obj, handler.as_object()) {
                                        (Some(wanted), Some(actual)) => {
                                            boa_engine::object::JsObject::equals(wanted, &actual)
                                        }
                                        _ => false,
                                    } =>
                                {
                                    Some(name.clone())
                                }
                                _ => None,
                            })
                        })
                };
                if let Some(name) = target {
                    b.borrow_mut()
                        .listener_actions
                        .push(ListenerAction::Remove { node_id: nid, name });
                }
            }
            if let Some(o) = this.as_object() {
                if let Ok(lmap) = o.get(boa_engine::js_string!("_js_listeners"), ctx) {
                    if let Some(lm) = lmap.as_object() {
                        if let Ok(type_arr) =
                            lm.get(boa_engine::js_string!(event_type.clone()), ctx)
                        {
                            if let Some(arr) = type_arr.as_object() {
                                let len =
                                    arr.get(boa_engine::js_string!("length"), ctx)
                                        .ok()
                                        .and_then(|v| v.as_number())
                                        .unwrap_or(0.0) as u32;
                                let new_arr = boa_engine::object::JsObject::with_object_proto(
                                    ctx.intrinsics(),
                                );
                                let pd = |val: JsValue| {
                                    boa_engine::property::PropertyDescriptor::builder()
                                        .value(val)
                                        .writable(true)
                                        .enumerable(true)
                                        .configurable(true)
                                        .build()
                                };
                                let mut new_len = 0u32;
                                for i in 0..len {
                                    if let Ok(h) = arr.get(i as u32, ctx) {
                                        let matches = match (&handler_obj, h.as_object()) {
                                            (Some(target), Some(ho)) => {
                                                boa_engine::object::JsObject::equals(target, &ho)
                                            }
                                            _ => false,
                                        };
                                        if !matches {
                                            let _ = new_arr.insert_property(new_len, pd(h));
                                            new_len += 1;
                                        }
                                    }
                                }
                                let _ = new_arr.insert_property(
                                    boa_engine::js_string!("length"),
                                    pd(JsValue::from(new_len)),
                                );
                                let _ = lm.insert_property(
                                    boa_engine::js_string!(event_type),
                                    pd(JsValue::from(new_arr)),
                                );
                            }
                        }
                    }
                }
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(
        remove_listener,
        boa_engine::js_string!("removeEventListener"),
        2,
    );

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
            } else if let (Some(parent_id), Some(alloc)) = (parent_id, read_canvas_alloc(&child)) {
                // Created canvas: the buffer is already allocated under
                // `alloc`; the op replay creates the DOM node and homes it.
                b.borrow_mut().ops.push(Op::AttachCanvas {
                    parent_id,
                    alloc_id: alloc,
                });
            } else if let (Some(parent_id), Some(child_id)) = (parent_id, read_handle_id(&child)) {
                // A REAL tree node: DOM appendChild MOVES it under the new
                // parent. Without this branch the call was mirror-only and
                // the tree kept the old shape (L940 — `appendChild` of a
                // parsed node was a total no-op). Anchor 0 = append at end;
                // the apply arm relocates via append_children.
                b.borrow_mut().ops.push(Op::InsertBefore {
                    parent_id,
                    child_id,
                    anchor_id: 0,
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
    let remove_child = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
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
            let in_children = parent_obj
                .get(boa_engine::js_string!("_children"), ctx)
                .ok()
                .and_then(|v| v.as_object())
                .map(|arr| {
                    let len = arr
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    (0..len).any(|i| {
                        arr.get(i as u32, ctx)
                            .ok()
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
                // Compare the child's REAL parent instead of assuming
                // membership: the old "assume it's in the tree" fallback let
                // `removeChild(cousin)` silently destroy an unrelated live
                // node (round-2 S1), and its unreachable else is what made a
                // double removal panic reachable.
                let in_tree = match (parent_id, child_id) {
                    (Some(pid), Some(cid)) => b
                        .borrow()
                        .node_props
                        .get(&cid)
                        .map(|snap| snap.parent_id == pid)
                        .unwrap_or(false),
                    _ => false,
                };
                if !in_tree {
                    return Err(boa_engine::JsNativeError::typ()
                        .with_message(
                            "NotFoundError: The node to be removed is not a child of this node",
                        )
                        .into());
                }
            }
            // Push the DOM removal: the JS mirror alone left the parsed tree
            // unchanged, so even a rebind could not observe the removal (L940).
            if let (Some(parent_id), Some(child_id)) = (
                read_handle_id(&JsValue::from(parent_obj.clone())),
                read_handle_id(&child),
            ) {
                b.borrow_mut().ops.push(Op::RemoveChild {
                    parent_id,
                    child_id,
                });
            }
            remove_from_children(&parent_obj, &child_obj, ctx);
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
            };
            let _ = child_obj
                .insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
            let _ = child_obj
                .insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
            Ok(child)
        },
        Gc::clone(&bridge),
    );
    init.function(remove_child, boa_engine::js_string!("removeChild"), 1);

    // insertBefore — inserts child before reference node in _children.
    let insert_before = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            let child = args.first().cloned().unwrap_or(JsValue::null());
            let ref_node = args.get(1).cloned().unwrap_or(JsValue::null());
            if let (Some(parent_obj), Some(child_obj)) = (this.as_object(), child.as_object()) {
                insert_into_children(&parent_obj, &child_obj, ref_node.as_object(), ctx);
                // Push the tree-level insert: mirror-only left the parsed tree
                // unchanged, so a remove→insert "move" destroyed the node instead
                // of relocating it (L940, PR #86 R2). Anchor 0 = append at end
                // (the apply arm then behaves like AppendChild).
                if let Some(parent_id) = read_handle_id(&JsValue::from(parent_obj.clone())) {
                    let child_id = read_handle_id(&child).unwrap_or(0);
                    let anchor_id = read_handle_id(&ref_node).unwrap_or(0);
                    b.borrow_mut().ops.push(Op::InsertBefore {
                        parent_id,
                        child_id,
                        anchor_id,
                    });
                }
            }
            Ok(child)
        },
        Gc::clone(&bridge),
    );
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
        if new_child.is_null()
            || new_child.is_undefined()
            || old_child.is_null()
            || old_child.is_undefined()
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
                    let len = cn_obj
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0);
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
        let nt1 = this_obj
            .get(boa_engine::js_string!("nodeType"), ctx)
            .unwrap_or_default();
        let nt2 = other_obj
            .get(boa_engine::js_string!("nodeType"), ctx)
            .unwrap_or_default();
        if nt1 != nt2 {
            return Ok(JsValue::from(false));
        }
        // Compare nodeName.
        let tn1 = this_obj
            .get(boa_engine::js_string!("nodeName"), ctx)
            .unwrap_or_default();
        let tn2 = other_obj
            .get(boa_engine::js_string!("nodeName"), ctx)
            .unwrap_or_default();
        if tn1 != tn2 {
            return Ok(JsValue::from(false));
        }

        let nt_val = nt1.as_number().unwrap_or(0.0) as u32;
        match nt_val {
            // DocumentType: compare name, publicId, systemId
            10 => {
                for prop in &["name", "publicId", "systemId"] {
                    let v1 = this_obj
                        .get(boa_engine::js_string!(*prop), ctx)
                        .unwrap_or_default();
                    let v2 = other_obj
                        .get(boa_engine::js_string!(*prop), ctx)
                        .unwrap_or_default();
                    if v1 != v2 {
                        return Ok(JsValue::from(false));
                    }
                }
                Ok(JsValue::from(true))
            }
            // Element: compare namespaceURI, prefix, localName, attributes
            1 => {
                for prop in &["namespaceURI", "prefix", "localName"] {
                    let v1 = this_obj
                        .get(boa_engine::js_string!(*prop), ctx)
                        .unwrap_or_default();
                    let v2 = other_obj
                        .get(boa_engine::js_string!(*prop), ctx)
                        .unwrap_or_default();
                    if v1 != v2 {
                        return Ok(JsValue::from(false));
                    }
                }
                // Compare attributes count (simplified).
                let a1 = this_obj
                    .get(boa_engine::js_string!("attributes"), ctx)
                    .ok()
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(boa_engine::js_string!("length"), ctx).ok())
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                let a2 = other_obj
                    .get(boa_engine::js_string!("attributes"), ctx)
                    .ok()
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(boa_engine::js_string!("length"), ctx).ok())
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
                if a1 != a2 {
                    return Ok(JsValue::from(false));
                }
                Ok(JsValue::from(true))
            }
            // Text/Comment/PI: compare data/textContent
            3 | 8 | 7 => {
                let tc1 = this_obj
                    .get(boa_engine::js_string!("textContent"), ctx)
                    .unwrap_or_default();
                let tc2 = other_obj
                    .get(boa_engine::js_string!("textContent"), ctx)
                    .unwrap_or_default();
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
    init.function(
        compare_pos,
        boa_engine::js_string!("compareDocumentPosition"),
        1,
    );

    // getRootNode() — returns this (simplified; no composed tree walking).
    let get_root_node = NativeFunction::from_copy_closure(|this, _args, ctx| {
        let mut current = this.as_object();
        for _ in 0..100 {
            let parent = current.as_ref().and_then(|o| {
                o.get(boa_engine::js_string!("parentNode"), ctx)
                    .ok()
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
            if v.is_null() {
                None
            } else {
                v.as_string().map(|s| s.to_std_string_escaped())
            }
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
            let nt = node
                .get(boa_engine::js_string!("nodeType"), ctx)
                .ok()
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as u32;

            // For element nodes (type 1), check namespace declarations.
            if nt == 1 {
                // Check if this element's own prefix matches.
                if !prefix_str.is_empty() {
                    let elem_prefix = node
                        .get(boa_engine::js_string!("prefix"), ctx)
                        .ok()
                        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()));
                    if elem_prefix.as_deref() == Some(prefix_str) {
                        let ns = node.get(boa_engine::js_string!("namespaceURI"), ctx).ok();
                        return Ok(ns.filter(|v| !v.is_null()).unwrap_or(JsValue::null()));
                    }
                }
                // Check xmlns attributes.
                if let Ok(attrs) = node.get(boa_engine::js_string!("attributes"), ctx) {
                    if let Some(ao) = attrs.as_object() {
                        let alen = ao
                            .get(boa_engine::js_string!("length"), ctx)
                            .ok()
                            .and_then(|v| v.as_number())
                            .unwrap_or(0.0) as u32;
                        for i in 0..alen {
                            if let Ok(a) = ao.get(i as u32, ctx) {
                                if let Some(a_obj) = a.as_object() {
                                    let aname = a_obj
                                        .get(boa_engine::js_string!("name"), ctx)
                                        .ok()
                                        .and_then(|v| {
                                            v.as_string().map(|s| s.to_std_string_escaped())
                                        })
                                        .unwrap_or_default();
                                    let aval = a_obj
                                        .get(boa_engine::js_string!("value"), ctx)
                                        .ok()
                                        .and_then(|v| {
                                            v.as_string().map(|s| s.to_std_string_escaped())
                                        })
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
            current = node
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
                .and_then(|v| v.as_object());
        }
        Ok(JsValue::null())
    });
    init.function(
        lookup_ns_uri,
        boa_engine::js_string!("lookupNamespaceURI"),
        1,
    );

    // lookupPrefix(namespace) — returns prefix for given namespace (simplified: null).
    let lookup_prefix = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::null()));
    init.function(lookup_prefix, boa_engine::js_string!("lookupPrefix"), 1);

    // isDefaultNamespace(namespace) — checks if lookupNamespaceURI(null) === namespace.
    let is_default_ns = NativeFunction::from_copy_closure(|this, args, ctx| {
        let ns_val = args.first().cloned().unwrap_or(JsValue::null());
        let ns_str = if ns_val.is_null() {
            String::new()
        } else {
            ns_val
                .as_string()
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default()
        };
        // Get the default namespace by calling lookupNamespaceURI(null).
        let lookup_fn = this
            .as_object()
            .and_then(|o| {
                o.get(boa_engine::js_string!("lookupNamespaceURI"), ctx)
                    .ok()
            })
            .and_then(|v| v.as_object());
        if let Some(lf) = lookup_fn {
            if let Ok(result) = lf.call(this, &[JsValue::null()], ctx) {
                let result_str = result
                    .as_string()
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                return Ok(JsValue::from(result_str == ns_str));
            }
        }
        // Fallback: default namespace is null/empty.
        Ok(JsValue::from(ns_str.is_empty()))
    });
    init.function(
        is_default_ns,
        boa_engine::js_string!("isDefaultNamespace"),
        1,
    );

    // normalize() — no-op.
    let normalize_fn =
        NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
    init.function(normalize_fn, boa_engine::js_string!("normalize"), 0);

    // remove() — removes this node from its parent's _children.
    let remove_fn = NativeFunction::from_copy_closure(|this, _args, ctx| {
        if let Some(obj) = this.as_object() {
            let parent = obj
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
                .and_then(|v| v.as_object());
            if let Some(parent_obj) = parent {
                remove_from_children(&parent_obj, &obj, ctx);
                let pd = |val: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(val)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                let _ =
                    obj.insert_property(boa_engine::js_string!("parentNode"), pd(JsValue::null()));
                let _ = obj
                    .insert_property(boa_engine::js_string!("parentElement"), pd(JsValue::null()));
            }
        }
        Ok(JsValue::undefined())
    });
    init.function(remove_fn, boa_engine::js_string!("remove"), 0);

    // append(...nodes) — appends all arguments as children.
    let append_children = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            if let Some(parent_obj) = this.as_object() {
                let parent_id = read_handle_id(&JsValue::from(parent_obj.clone()));
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        // Real tree node: DOM append() MOVES it to the end, same
                        // as appendChild (#88). Mirror-only left the tree stale.
                        if let (Some(pid), Some(cid)) = (parent_id, read_handle_id(&node)) {
                            b.borrow_mut().ops.push(Op::InsertBefore {
                                parent_id: pid,
                                child_id: cid,
                                anchor_id: 0,
                            });
                        }
                        insert_into_children(&parent_obj, &child_obj, None, ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(append_children, boa_engine::js_string!("append"), 0);

    // prepend(...nodes) — inserts all arguments before the first child.
    let prepend_children = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            if let Some(parent_obj) = this.as_object() {
                let parent_id = read_handle_id(&JsValue::from(parent_obj.clone()));
                // Anchor = the parent's FIRST child (bind-time snapshot; empty
                // parent → 0 → the apply arm appends). Captured once so a
                // multi-arg prepend keeps its order: each node lands right
                // before the original first child, so [a, b] prepends as
                // [a, b, ...old...].
                let anchor_id = parent_id
                    .and_then(|pid| {
                        let props = b.borrow();
                        props
                            .node_props
                            .get(&pid)
                            .and_then(|snap| snap.child_ids.first().copied())
                    })
                    .unwrap_or(0);
                // Mirror anchor: the first child STUB, so a prepended node
                // splices to the HEAD of the JS mirror (master's behavior —
                // appending at the tail left the mirror reading [a,b,c]
                // while the tree held [m,a,b,c], round-5 S2).
                let first_child = parent_obj
                    .get(boa_engine::js_string!("_children"), ctx)
                    .ok()
                    .and_then(|v| v.as_object())
                    .and_then(|a| a.get(0u32, ctx).ok())
                    .and_then(|v| v.as_object());
                for arg in args.iter() {
                    let node = value_to_node(arg, ctx);
                    if let Some(child_obj) = node.as_object() {
                        if let (Some(pid), Some(cid)) = (parent_id, read_handle_id(&node)) {
                            if cid == anchor_id {
                                // Prepending the current first child is a
                                // DOM no-op: neither the tree nor the mirror
                                // may change (pushing the op made blitz
                                // detach the child and panic on the severed
                                // anchor — round-2 S0).
                                continue;
                            }
                            b.borrow_mut().ops.push(Op::InsertBefore {
                                parent_id: pid,
                                child_id: cid,
                                anchor_id,
                            });
                        }
                        insert_into_children(&parent_obj, &child_obj, first_child.clone(), ctx);
                    }
                }
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(prepend_children, boa_engine::js_string!("prepend"), 0);

    // after(...nodes) — inserts siblings after this node in parent._children.
    let after_fn = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            if let Some(obj) = this.as_object() {
                let parent = obj
                    .get(boa_engine::js_string!("parentNode"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                if let Some(parent_obj) = parent {
                    // Find next sibling as reference for insertion.
                    let next_sibling = get_next_sibling(&parent_obj, &obj, ctx);
                    // Mirror first (every arg splices before the SAME
                    // captured sibling -> DOM order), then push ONE batch op
                    // carrying every real-tree arg in DOM order. A single op
                    // cannot drift its anchor between arguments
                    // (round-6 S0: mixed argument lists landed on the wrong
                    // side of the ref with per-arg ops).
                    let mut batch: Vec<u64> = Vec::new();
                    for arg in args.iter() {
                        let node = value_to_node(arg, ctx);
                        if let Some(child_obj) = node.as_object() {
                            if let Some(cid) = read_handle_id(&node) {
                                batch.push(cid);
                            }
                            insert_into_children(
                                &parent_obj,
                                &child_obj,
                                next_sibling.clone(),
                                ctx,
                            );
                        }
                    }
                    if !batch.is_empty()
                        && let Some(ref_id) = read_handle_id(&JsValue::from(obj.clone()))
                    {
                        b.borrow_mut().ops.push(Op::InsertAdjacent {
                            ref_id,
                            child_ids: batch,
                            after: true,
                        });
                    }
                }
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(after_fn, boa_engine::js_string!("after"), 0);

    // before(...nodes) — inserts siblings before this node in parent._children.
    let before_fn = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            if let Some(obj) = this.as_object() {
                let parent = obj
                    .get(boa_engine::js_string!("parentNode"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                if let Some(parent_obj) = parent {
                    // Same batching as after(): one op carries the whole node
                    // list in DOM order.
                    let mut batch: Vec<u64> = Vec::new();
                    for arg in args.iter() {
                        let node = value_to_node(arg, ctx);
                        if let Some(child_obj) = node.as_object() {
                            if let Some(cid) = read_handle_id(&node) {
                                batch.push(cid);
                            }
                            insert_into_children(&parent_obj, &child_obj, Some(obj.clone()), ctx);
                        }
                    }
                    if !batch.is_empty()
                        && let Some(ref_id) = read_handle_id(&JsValue::from(obj.clone()))
                    {
                        b.borrow_mut().ops.push(Op::InsertAdjacent {
                            ref_id,
                            child_ids: batch,
                            after: false,
                        });
                    }
                }
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(before_fn, boa_engine::js_string!("before"), 0);

    // replaceWith(...nodes) — replaces this node with the arguments.
    let replace_with_fn = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            if let Some(obj) = this.as_object() {
                let parent = obj
                    .get(boa_engine::js_string!("parentNode"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                if let Some(parent_obj) = parent {
                    // Real tree node batch: one op replaces the ref in the tree
                    // (mirror-only left the tree unchanged — same family as
                    // before/after, PR #90 R5).
                    let mut batch: Vec<u64> = Vec::new();
                    for arg in args.iter() {
                        let node = value_to_node(arg, ctx);
                        if let Some(child_obj) = node.as_object() {
                            if let Some(cid) = read_handle_id(&node) {
                                batch.push(cid);
                            }
                            insert_into_children(&parent_obj, &child_obj, Some(obj.clone()), ctx);
                        }
                    }
                    // Remove this node from the mirror too.
                    remove_from_children(&parent_obj, &obj, ctx);
                    if let Some(ref_id) = read_handle_id(&JsValue::from(obj.clone())) {
                        b.borrow_mut().ops.push(Op::ReplaceWith {
                            ref_id,
                            child_ids: batch,
                        });
                    }
                }
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
    init.function(replace_with_fn, boa_engine::js_string!("replaceWith"), 0);

    // closest(selector) — walks up parentNode chain matching selector.
    let closest_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let selector = arg_string(args, 0);
        if selector.is_empty() {
            return Ok(JsValue::null());
        }
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
            current = node
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
                .and_then(|v| v.as_object());
        }
        Ok(JsValue::null())
    });
    init.function(closest_fn, boa_engine::js_string!("closest"), 1);

    // matches(selector) — check if this element matches a selector.
    let matches_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let selector = arg_string(args, 0);
        if selector.is_empty() {
            return Ok(JsValue::from(false));
        }
        let node = match this.as_object() {
            Some(o) => o,
            None => return Ok(JsValue::from(false)),
        };
        Ok(JsValue::from(element_matches_selector(
            &node, &selector, ctx,
        )))
    });
    init.function(matches_fn, boa_engine::js_string!("matches"), 1);

    // insertAdjacentElement(position, element) — inserts element relative to this.
    let iae_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
        let position = arg_string(args, 0).to_ascii_lowercase();
        let element = args.get(1).cloned().unwrap_or(JsValue::null());
        if let (Some(obj), Some(el_obj)) = (this.as_object(), element.as_object()) {
            let parent = obj
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
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
                    let first_child =
                        if let Ok(cv) = obj.get(boa_engine::js_string!("_children"), ctx) {
                            if let Some(ca) = cv.as_object() {
                                let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                                if v.is_undefined() {
                                    None
                                } else {
                                    v.as_object()
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };
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
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        // Create a text node.
        let tn = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let _ = tn.insert_property(boa_engine::js_string!("nodeType"), pd(JsValue::from(3u32)));
        let _ = tn.insert_property(
            boa_engine::js_string!("_data"),
            pd(JsValue::from(boa_engine::js_string!(text.clone()))),
        );
        let _ = tn.insert_property(
            boa_engine::js_string!("data"),
            pd(JsValue::from(boa_engine::js_string!(text.clone()))),
        );
        let _ = tn.insert_property(
            boa_engine::js_string!("nodeValue"),
            pd(JsValue::from(boa_engine::js_string!(text.clone()))),
        );
        let _ = tn.insert_property(
            boa_engine::js_string!("textContent"),
            pd(JsValue::from(boa_engine::js_string!(text))),
        );
        let _ = tn.insert_property(
            boa_engine::js_string!("nodeName"),
            pd(JsValue::from(boa_engine::js_string!("#text"))),
        );
        if let Ok(text_ctor) = ctx.global_object().get(boa_engine::js_string!("Text"), ctx) {
            if let Some(tc) = text_ctor.as_object() {
                if let Ok(pv) = tc.get(boa_engine::js_string!("prototype"), ctx) {
                    if let Some(p) = pv.as_object() {
                        let _ = tn.set_prototype(Some(p));
                    }
                }
            }
        }
        if let Some(obj) = this.as_object() {
            let parent = obj
                .get(boa_engine::js_string!("parentNode"), ctx)
                .ok()
                .and_then(|v| v.as_object());
            match position.as_str() {
                "beforebegin" => {
                    if let Some(p) = parent {
                        insert_into_children(&p, &tn, Some(obj.clone()), ctx);
                    }
                }
                "afterend" => {
                    if let Some(p) = parent {
                        let next = get_next_sibling(&p, &obj, ctx);
                        insert_into_children(&p, &tn, next, ctx);
                    }
                }
                "afterbegin" => {
                    let first_child =
                        if let Ok(cv) = obj.get(boa_engine::js_string!("_children"), ctx) {
                            if let Some(ca) = cv.as_object() {
                                let v = ca.get(0u32, ctx).unwrap_or(JsValue::null());
                                if v.is_undefined() {
                                    None
                                } else {
                                    v.as_object()
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };
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
            let matching: Vec<u64> = bb
                .node_props
                .iter()
                .filter(|(_, s)| {
                    s.node_type == 1 && (tag == "*" || s.tag_name.to_lowercase() == tag)
                })
                .map(|(&id, _)| id)
                .collect();
            drop(bb);
            // Use a plain object (NOT JsArray — JsArray doesn't support custom methods).
            // Do NOT set HTMLCollection.prototype as it breaks integer-index property lookup.
            let arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
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
            let _ = arr.insert_property(
                boa_engine::js_string!("length"),
                pd(JsValue::from(matching.len() as u32)),
            );
            // item(index) method.
            let item_fn = NativeFunction::from_copy_closure(|this, args, ctx| {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(-1.0) as i32;
                if idx < 0 {
                    return Ok(JsValue::null());
                }
                if let Some(o) = this.as_object() {
                    let len = o
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    if (idx as u32) < len {
                        return Ok(o.get(idx as u32, ctx).unwrap_or(JsValue::null()));
                    }
                }
                Ok(JsValue::null())
            });
            let _ = arr.insert_property(
                boa_engine::js_string!("item"),
                pd(JsValue::from(
                    boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), item_fn).build(),
                )),
            );
            // Note: HTMLCollection.prototype is NOT set on the array because it
            // breaks Boa's integer-index property lookup. Keep Array.prototype.
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
                .value(val)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = arr.insert_property(boa_engine::js_string!("length"), pd(JsValue::from(0u32)));
        Ok(arr.into())
    });
    init.function(
        by_tag_ns,
        boa_engine::js_string!("getElementsByTagNameNS"),
        2,
    );

    // getElementsByClassName — returns array of matching element handles.
    let by_class = NativeFunction::from_copy_closure_with_captures(
        |_this, args, b, ctx| {
            let cls = arg_string(args, 0);
            let wanted: Vec<&str> = cls.split_whitespace().collect();
            let bb = b.borrow();
            let matching: Vec<u64> = bb
                .node_props
                .iter()
                .filter(|(_, s)| {
                    s.node_type == 1
                        && wanted
                            .iter()
                            .all(|w| s.class_name.split_whitespace().any(|c| c == *w))
                })
                .map(|(&id, _)| id)
                .collect();
            drop(bb);
            // Use a plain object (NOT JsArray — JsArray doesn't support custom methods).
            // Do NOT set HTMLCollection.prototype as it breaks integer-index property lookup.
            let arr = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
            let pd = |val: JsValue| {
                boa_engine::property::PropertyDescriptor::builder()
                    .value(val)
                    .writable(true)
                    .enumerable(true)
                    .configurable(true)
                    .build()
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
            let _ = arr.insert_property(
                boa_engine::js_string!("length"),
                pd(JsValue::from(matching.len() as u32)),
            );
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

    // dataset — DOMStringMap of data-* attributes. Built lazily from attributes NamedNodeMap.
    let ds_get = NativeFunction::from_copy_closure(|this, _args, ctx| {
        let ds = boa_engine::object::JsObject::with_object_proto(ctx.intrinsics());
        let pd = |v: JsValue| {
            boa_engine::property::PropertyDescriptor::builder()
                .value(v)
                .writable(true)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        if let Some(o) = this.as_object() {
            if let Ok(attrs_val) = o.get(boa_engine::js_string!("attributes"), ctx) {
                if let Some(attrs) = attrs_val.as_object() {
                    let len = attrs
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
                    for i in 0..len {
                        if let Ok(av) = attrs.get(i as u32, ctx) {
                            if let Some(ao) = av.as_object() {
                                if let Ok(an) = ao.get(boa_engine::js_string!("name"), ctx) {
                                    if let Some(an_s) = an.as_string() {
                                        let name = an_s.to_std_string_escaped();
                                        if name.starts_with("data-") {
                                            let key: String = name[5..]
                                                .split('-')
                                                .enumerate()
                                                .map(|(i, part)| {
                                                    if i == 0 {
                                                        part.to_string()
                                                    } else {
                                                        let mut c = part.chars();
                                                        c.next()
                                                            .map(|f| {
                                                                f.to_uppercase().collect::<String>()
                                                            })
                                                            .unwrap_or_default()
                                                            + c.as_str()
                                                    }
                                                })
                                                .collect();
                                            if let Ok(val) =
                                                ao.get(boa_engine::js_string!("value"), ctx)
                                            {
                                                let _ = ds.insert_property(
                                                    boa_engine::js_string!(key),
                                                    pd(val),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(JsValue::from(ds))
    });
    let ds_get_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), ds_get).build();
    let _ = obj.insert_property(
        boa_engine::js_string!("dataset"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(ds_get_fn)
            .enumerable(true)
            .configurable(true)
            .build(),
    );

    // textContent as accessor: getter concatenates descendant text, setter replaces children.
    let tc_get = NativeFunction::from_copy_closure_with_captures(
        |this, _args, b, ctx| {
            if let Some(o) = this.as_object() {
                // Has this handle's text been written from script yet? A parsed
                // element has no `_children` text, so the snapshot is the only
                // source — but once script writes (INCLUDING writing ""),
                // `_children` is authoritative once it holds text; when it is
                // empty the node snapshot is the source. Both setters keep that
                // snapshot in step, so "written empty" and "never written" stay
                // distinguishable, and after a `textContent` / `setText` write
                // every handle — fresh or held — reads back what the DOM holds
                // (rounds 2-3). NOTE the precondition: child-list mutations
                // that never carried a text write (e.g. removeChild to empty)
                // still read back from the snapshot; the create→set→append
                // shape IS reconciled, asserted by
                // `append_after_a_text_write_is_consistent`.
                let local = collect_text_content(&o, ctx, 0);
                if !local.is_empty() {
                    return Ok(JsValue::from(boa_engine::js_string!(local)));
                }
                if let Some(nid) = read_handle_id(this)
                    && let Some(snap) = b.borrow().node_props.get(&nid).cloned()
                {
                    return Ok(JsValue::from(boa_engine::js_string!(
                        snap.text_content.clone()
                    )));
                }
                return Ok(JsValue::from(boa_engine::js_string!("")));
            }
            Ok(JsValue::from(boa_engine::js_string!("")))
        },
        Gc::clone(&bridge),
    );
    let tc_set = NativeFunction::from_copy_closure_with_captures(
        |this, args, b, ctx| {
            if let Some(o) = this.as_object() {
                let val = args.first().map(|v| v.clone()).unwrap_or(JsValue::null());
                let s = if val.is_null() {
                    String::new()
                } else {
                    val.to_string(ctx)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default()
                };
                let s_for_op = s.clone();
                let pd = |v: JsValue| {
                    boa_engine::property::PropertyDescriptor::builder()
                        .value(v)
                        .writable(true)
                        .enumerable(true)
                        .configurable(true)
                        .build()
                };
                rebuild_children_mirror(&o, &s, ctx);
                // Push the DOM write. Without this the setter only rebuilt the
                // JS-side `_children` mirror, so `el.textContent = x` looked
                // applied from script but the rendered document never changed
                // (the retired `js_interactive` bridge masked this by rewriting
                // the source to `setText`).
                if read_handle_id(this).is_none()
                    && let Some(pid) = read_pending(this)
                {
                    // PENDING handle: the write must ride the pending tuple so
                    // `Op::AppendChild` carries it into the tree (create → set
                    // text → append previously produced an empty node).
                    if let Some(entry) = b.borrow_mut().pending.get_mut(&pid) {
                        entry.1 = s_for_op.clone();
                    }
                }
                if let Some(nid) = read_handle_id(this) {
                    {
                        let mut bb = b.borrow_mut();
                        // Keep the NODE snapshot in step with the write: the
                        // snapshot is what a freshly-fetched handle reads when
                        // its own `_children` is still empty (round-3 S1 —
                        // `getElementById` mints a new handle per call, so a
                        // per-handle notion of "written" cannot work).
                        if let Some(snap) = bb.node_props.get_mut(&nid) {
                            snap.text_content = s_for_op.clone();
                        }
                        bb.ops.push(Op::SetText {
                            node_id: nid,
                            value: s_for_op,
                        });
                    }
                }
            }
            Ok(JsValue::undefined())
        },
        Gc::clone(&bridge),
    );
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
                    if v.is_undefined() {
                        return Ok(JsValue::null());
                    }
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
                    let len = ca
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
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
            .get(nv_ng)
            .set(nv_ns)
            .enumerable(true)
            .configurable(true)
            .build(),
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
        Ok(JsValue::from(new_array(ctx)))
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
        if let (Some(obj), Some(parent)) = (
            this.as_object(),
            this.as_object()
                .and_then(|o| o.get(boa_engine::js_string!("parentNode"), ctx).ok())
                .and_then(|v| v.as_object()),
        ) {
            if let Ok(cv) = parent.get(boa_engine::js_string!("_children"), ctx) {
                if let Some(ca) = cv.as_object() {
                    let len = ca
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
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
        if let (Some(obj), Some(parent)) = (
            this.as_object(),
            this.as_object()
                .and_then(|o| o.get(boa_engine::js_string!("parentNode"), ctx).ok())
                .and_then(|v| v.as_object()),
        ) {
            if let Ok(cv) = parent.get(boa_engine::js_string!("_children"), ctx) {
                if let Some(ca) = cv.as_object() {
                    let len = ca
                        .get(boa_engine::js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as u32;
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
    for (name, getter) in &[
        ("previousSibling", ps_fn.clone()),
        ("nextSibling", ns_fn.clone()),
    ] {
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

#[cfg(test)]
mod node_props_tests {
    //! Pin the u64 id domain and the root-parent contract in the node
    //! snapshot (L940 leftover ⑥ — R3 mutations M3/M5 survived uncovered:
    //! truncating version bits and fabricating a root parent_id both went
    //! unnoticed by every shipped behavior bin).

    use super::{collect_ids, collect_node_props};
    use blitz_dom::{BaseDocument, DocumentConfig};
    use blitz_html::HtmlDocument;

    fn test_doc() -> HtmlDocument {
        HtmlDocument::from_html(
            r#"<html><head></head><body><p id="p">x</p></body></html>"#,
            DocumentConfig::default(),
        )
    }

    #[test]
    fn snapshot_ids_are_live_tree_ids() {
        let doc = test_doc();
        let snapshot = collect_node_props(&doc);
        let tree_ids: std::collections::HashSet<u64> =
            doc.tree().iter().map(|(id, _)| id.as_u64()).collect();

        assert!(
            !tree_ids.is_empty() && tree_ids.iter().all(|v| *v > u32::MAX as u64),
            "a fresh document must carry versioned ids (index + version in high bits), got {tree_ids:?}"
        );
        for (id, snap) in &snapshot {
            assert!(tree_ids.contains(id), "snapshot key {id:#x} is alien");
            for c in &snap.child_ids {
                assert!(tree_ids.contains(c), "child id {c:#x} is alien");
            }
            if snap.parent_id != 0 {
                assert!(
                    tree_ids.contains(&snap.parent_id),
                    "parent id {:#x} is alien",
                    snap.parent_id
                );
            }
        }
    }

    #[test]
    fn root_parent_id_is_zero_not_fabricated() {
        let doc = test_doc();
        let snapshot = collect_node_props(&doc);
        let mut covered_roots = 0;
        for (id, node) in doc.tree().iter() {
            if node.parent.is_none()
                && let Some(snap) = snapshot.get(&id.as_u64())
            {
                covered_roots += 1;
                assert_eq!(
                    snap.parent_id, 0,
                    "root {id:?} must keep parent_id 0 (the documented no-parent sentinel)"
                );
            }
        }
        assert!(
            covered_roots >= 1,
            "fixture must contain at least one parentless node, else this test is vacuous"
        );
    }

    #[test]
    fn collect_ids_agrees_with_tree() {
        let doc = test_doc();
        let ids = collect_ids(&doc);
        let tree_ids: std::collections::HashSet<u64> =
            doc.tree().iter().map(|(id, _)| id.as_u64()).collect();
        for (name, id) in &ids {
            assert!(tree_ids.contains(id), "ids[{name}] = {id:#x} is alien");
        }
    }
}

#[cfg(test)]
mod bridge_flow_tests {
    //! Behavioral pins for the two L940 leftover defects (① listener
    //! registration dead path, ② canvas key-domain mismatch), exercised
    //! through the real bridge: script → ops → replay → DOM/compositor.

    use super::JsRuntime;
    use blitz_dom::{BaseDocument, DocumentConfig};
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn find_id(doc: &HtmlDocument, want: &str) -> Option<u64> {
        doc.tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(want))
            .map(|(id, _)| id.as_u64())
    }

    #[test]
    fn addEventListener_registration_reaches_fire_click() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn">go</button><div id="out">empty</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('btn').addEventListener('click', function () {
                document.getElementById('out').setText('clicked via listener');
            });
        "#,
        );
        let Some(btn) = find_id(&doc, "btn") else {
            panic!("fixture missing #btn");
        };
        rt.fire_click(&mut doc, btn);
        let Some(out) = find_id(&doc, "out") else {
            panic!("fixture missing #out");
        };
        let text = doc
            .get_node(NodeId::from_u64(out))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            text, "clicked via listener",
            "a registered click listener must fire and mutate the DOM"
        );
    }

    #[test]
    fn onclick_attr_and_add_listener_coexist_exactly_once() {
        // L940 leftover ① guard: fire_click re-enters bind_and_run for the
        // onclick attribute, which must not unregister (or double-fire) the
        // script-registered listeners — on this node or any other.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body>
            <button id="a" onclick="document.getElementById('oa').setText('oa-' + fire_seq++)">inline</button>
            <button id="b">scripted</button>
            <div id="oa">0</div>
            <div id="sa">0</div>
            <div id="sb">0</div>
            </body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var fire_seq = 1;
            document.getElementById('b').addEventListener('click', function () {
                var el = document.getElementById('sb');
                el.setText(el.textContent + 'x');
            });
        "#,
        );
        let click = |rt: &mut JsRuntime, doc: &mut HtmlDocument, id: &str| {
            let Some(nid) = doc
                .tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                .map(|(id, _)| id.as_u64())
            else {
                panic!("missing #{id}");
            };
            rt.fire_click(doc, nid);
        };

        // First click on the onclick node: inline handler fires once, the
        // scripted listener on #b must SURVIVE the re-bind.
        click(&mut rt, &mut doc, "a");
        let text = |doc: &HtmlDocument, id: &str| {
            doc.get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing output node"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default()
        };
        let oa = text(&doc, "oa");
        assert_eq!(
            oa, "oa-1",
            "onclick handler must fire exactly once (got {oa:?})"
        );
        assert_eq!(
            text(&doc, "sb"),
            "0",
            "unrelated listener must survive the onclick re-bind"
        );

        // Second click: inline handler again (exactly once), then the
        // scripted listener on #b still fires exactly once.
        click(&mut rt, &mut doc, "b");
        assert_eq!(
            text(&doc, "sb"),
            "0x",
            "scripted listener must fire exactly once"
        );
        click(&mut rt, &mut doc, "a");
        let oa2 = text(&doc, "oa");
        // setText replaces, and fire_seq increments per fire: a single firing
        // reads "oa-2"; a stacked double firing would read "oa-3" (skipped).
        assert_eq!(
            oa2, "oa-2",
            "second inline firing must stay single (got {oa2:?})"
        );
    }

    #[test]
    fn created_canvas_homes_to_its_dom_node() {
        let mut doc =
            HtmlDocument::from_html(r#"<html><body></body></html>"#, DocumentConfig::default());
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var c = document.createElement('canvas');
            c.getContext('2d').fillRect(0, 0, 5, 5);
            document.body.appendChild(c);
        "#,
        );

        // The created canvas must exist as a real DOM node under body...
        let Some(body) = doc
            .tree()
            .iter()
            .find(|(_, n)| {
                n.element_data()
                    .map(|e| format!("{:?}", e.name.local).contains("'body'"))
                    .unwrap_or(false)
            })
            .map(|(id, _)| id.as_u64())
        else {
            panic!("fixture missing body");
        };
        let children = doc
            .get_node(NodeId::from_u64(body))
            .map(|n| n.children.clone())
            .unwrap_or_default();
        assert!(
            !children.is_empty(),
            "created canvas must enter the DOM once its AttachCanvas op replays"
        );

        // ...and its buffer must be homed (node id → allocation id) so the
        // compositor finds it, with the recorded fillRect intact.
        let canvas_node = children[children.len() - 1].as_u64();
        let alloc = super::canvas_home_for(canvas_node)
            .unwrap_or_else(|| panic!("canvas node {canvas_node} must be homed"));
        let has_content = super::CANVASES.with(|cs| {
            cs.borrow()
                .get(&alloc)
                .map(|c| c.has_content())
                .unwrap_or(false)
        });
        assert!(
            has_content,
            "buffer under alloc {alloc} must carry the fillRect content"
        );
        // Materialization (the fix for L940 leftover ①-cycle): the
        // mutator-based replay gives the created node stylo style data +
        // IS_IN_DOCUMENT, so a layout pass gives it the canvas intrinsic
        // 300×150 box instead of the old 0×0 uncomposited stub. If this
        // starts failing with 0×0 again, the materialization regressed.
        doc.resolve(0.0);
        let node = doc
            .get_node(NodeId::from_u64(canvas_node))
            .expect("canvas node vanished");
        assert!(
            node.flags.is_in_document(),
            "created canvas must be in the document"
        );
        let size = node.final_layout().size;
        assert_eq!(
            (size.width, size.height),
            (300.0, 150.0),
            "created canvas must lay out at the canvas intrinsic 300×150, got {size:?}"
        );
    }
}

#[cfg(test)]
mod listener_registry_tests {
    //! L940 leftover ②-cycle: removeEventListener must untrack the handler
    //! from the fire_click dispatch map (ghost-firing fix), and the
    //! dispatchEvent surface must reset currentTarget after dispatch.

    use super::JsRuntime;
    use blitz_dom::{BaseDocument, DocumentConfig};
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn doc_with_buttons() -> (HtmlDocument, JsRuntime) {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body>
            <button id="a">a</button>
            <button id="b">b</button>
            <div id="out">0</div>
            </body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('a').addEventListener('click', function () {
                var o = document.getElementById('out');
                o.setText(o.textContent + 'a');
            });
            document.getElementById('b').addEventListener('click', function () {
                var o = document.getElementById('out');
                o.setText(o.textContent + 'b');
            });
        "#,
        );
        (doc, rt)
    }

    fn find_id(doc: &HtmlDocument, want: &str) -> u64 {
        doc.tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(want))
            .map(|(id, _)| id.as_u64())
            .expect("fixture id missing")
    }

    fn out_text(doc: &HtmlDocument) -> String {
        doc.get_node(blitz_traits::NodeId::from_u64(find_id(doc, "out")))
            .map(|n| n.text_content())
            .unwrap_or_default()
    }

    #[test]
    fn removeEventListener_stops_firing_only_for_the_matching_handler() {
        // Register #b's handler under a named global so the script can pass
        // the SAME function object to removeEventListener (handler identity
        // is what the untrack is gated on).
        let mut doc = HtmlDocument::from_html(
            r#"<html><body>
            <button id="a">a</button>
            <button id="b">b</button>
            <div id="out">0</div>
            </body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            function onB() {
                var o = document.getElementById('out');
                o.setText(o.textContent + 'b');
            }
            function onA() {
                var o = document.getElementById('out');
                o.setText(o.textContent + 'a');
            }
            document.getElementById('b').addEventListener('click', onB);
            document.getElementById('a').addEventListener('click', onA);

            // Negative control: removing an UNRELATED handler must not kill
            // the live listener (R3 S0 — an undefined/foreign handler used to
            // untrack the whole (node, type) pair). Same ticket as the
            // registration so the removal record is actually harvested.
            document.getElementById('b').removeEventListener('click', onA);
            "#,
        );
        let b_id = find_id(&doc, "b");
        rt.fire_click(&mut doc, b_id);
        assert_eq!(
            out_text(&doc),
            "0b",
            "removing an unrelated handler must NOT stop the live listener"
        );

        // Now remove the real registered handler (same ticket as a fresh
        // registration, so identity matching drives the untrack): it must
        // stop firing.
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('b').addEventListener('click', onB);
            document.getElementById('b').removeEventListener('click', onB);
            "#,
        );
        // Count firings in a JS counter (not appended text): an appended
        // marker reads identically whether the handler fired once or twice.
        rt.bind_and_run(
            &mut doc,
            r#"
            clickCount = 0;
            document.getElementById('b').addEventListener('click', function () {
                clickCount++;
            });
            "#,
        );
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('out').setText('count=' + clickCount);
            "#,
        );
        assert_eq!(
            out_text(&doc),
            "count=1",
            "the live handler must fire exactly once"
        );
    }

    #[test]
    fn remove_then_readd_in_one_ticket_stays_live() {
        // R3 S1's exact shape: within ONE script, remove then re-register.
        // The removal record must not outlive the re-registration that
        // follows it in the same harvest window (drain-before-add plus the
        // named re-push is what keeps this live).
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="b">b</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            function onB() {
                var o = document.getElementById('out');
                o.setText(o.textContent + 'b');
            }
            var b = document.getElementById('b');
            b.addEventListener('click', onB);
            b.removeEventListener('click', onB);
            b.addEventListener('click', onB);
            "#,
        );
        let b_id = find_id(&doc, "b");
        rt.fire_click(&mut doc, b_id);
        assert_eq!(
            out_text(&doc),
            "0b",
            "remove-then-readd in one ticket must leave the listener live"
        );
    }

    #[test]
    fn dispatch_resets_current_target_after_the_walk() {
        // WHATWG: currentTarget === this during dispatch, null after it.
        // The probe records both readings from inside the page script.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="t"></div><div id="out"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var inside = 'unset';
            var after = 'unset';
            var t = document.getElementById('t');
            t.addEventListener('ping', function (e) {
                inside = (e.currentTarget === t) ? 'target' : String(e.currentTarget);
            });
            var ev = new Event('ping');
            t.dispatchEvent(ev);
            after = String(ev.currentTarget);
            var o = document.getElementById('out');
            o.setText(inside + '|' + after);
        "#,
        );
        let out = doc
            .get_node(NodeId::from_u64(find_id(&doc, "out")))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "target|null",
            "currentTarget must be the dispatch target during the walk and null after it"
        );
    }
}

#[cfg(test)]
mod idless_canvas_tests {
    //! Pin the id-less getContext scratch-slot path (L940 leftover ③).
    //!
    //! A pending element handle (`createElement('div')`, not yet appended)
    //! carries `_arisId = 0` but still gets `getContext` from
    //! `make_element_handle`, so `getContext('2d')` is reachable on it. The
    //! contract (master parity): hand out a working 2D context instead of
    //! null — R6's null-return regression would fail this test.
    //!
    //! Key domain: the id-less route homes under `CANVAS_HOME[0]`, where 0 is
    //! the node-id sentinel — NOT an allocation id. All id-less handles on a
    //! thread therefore share one buffer (production code documents this at
    //! the getContext site). The assertion below checks the *routing*
    //! (CANVAS_HOME[0] names an allocation whose buffer holds the fillRect),
    //! not merely "allocation 0 has content".

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    /// The allocation id the id-less scratch route homed under node 0.
    fn scratch_alloc() -> Option<u64> {
        super::CANVAS_HOME.with(|home| home.borrow().get(&0).copied())
    }

    fn alloc_has_content(alloc: u64) -> bool {
        super::CANVASES.with(|cs| {
            cs.borrow()
                .get(&alloc)
                .map(|c| c.has_content())
                .unwrap_or(false)
        })
    }

    #[test]
    fn idless_handle_get_context_uses_scratch_slot() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var pending = document.createElement('div');
            var ctx = pending.getContext('2d');
            ctx.fillRect(0, 0, 4, 4);
            document.getElementById('out').setText(ctx === null ? 'null' : 'ctx');
            "#,
        );
        let out = doc
            .get_node(blitz_traits::NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "ctx",
            "an id-less pending handle must get a working 2D context (master parity)"
        );
        // Identity, not coincidence: the id-less handle must have homed an
        // allocation under CANVAS_HOME[0] and the fillRect must have landed
        // in THAT allocation. (Asserting "allocation 0 has content" instead
        // would also pass for a refactor that gave each handle a fresh
        // private buffer — that mutation survived round 2.)
        let alloc = scratch_alloc()
            .expect("the id-less handle must home an allocation under CANVAS_HOME[0]");
        assert!(
            alloc_has_content(alloc),
            "the fillRect must land in the scratch allocation {alloc} that CANVAS_HOME[0] names"
        );
    }
}

#[cfg(test)]
mod multi_handler_tests {
    //! L940 registry redesign: several handlers may listen to the same
    //! (node, type), each removable independently by handler identity.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    fn click_and_read(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="b">b</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("b"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #b");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(log));"#,
        );
        doc.get_node(blitz_traits::NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(id, _)| id.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn two_handlers_same_type_both_fire() {
        let out = click_and_read(
            r#"
            var log = '';
            function h1() { log += '1'; }
            function h2() { log += '2'; }
            var b = document.getElementById('b');
            b.addEventListener('click', h1);
            b.addEventListener('click', h2);
        "#,
        );
        assert_eq!(
            out, "12",
            "both handlers on one (node, type) must fire, in order"
        );
    }

    #[test]
    fn removing_one_of_two_keeps_the_other() {
        let out = click_and_read(
            r#"
            var log = '';
            function h1() { log += '1'; }
            function h2() { log += '2'; }
            var b = document.getElementById('b');
            b.addEventListener('click', h1);
            b.addEventListener('click', h2);
            b.removeEventListener('click', h1);
        "#,
        );
        assert_eq!(out, "2", "removing h1 must leave h2 registered");
    }

    #[test]
    fn removing_an_unrelated_handler_leaves_both() {
        let out = click_and_read(
            r#"
            var log = '';
            function h1() { log += '1'; }
            function h2() { log += '2'; }
            function other() { log += 'X'; }
            var b = document.getElementById('b');
            b.addEventListener('click', h1);
            b.addEventListener('click', h2);
            b.removeEventListener('click', other);
        "#,
        );
        assert_eq!(out, "12", "a foreign handler must not remove anything");
    }

    #[test]
    fn a_later_script_can_remove_an_earlier_registration() {
        // The registration table survives bind_and_run, so a script executed
        // later can remove what an earlier one registered.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="b">b</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var log = '';
            function h1() { log += '1'; }
            window.__h1 = h1;
            document.getElementById('b').addEventListener('click', h1);
        "#,
        );
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('b').removeEventListener('click', window.__h1);"#,
        );
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("b"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #b");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(log));"#,
        );
        let out = doc
            .get_node(blitz_traits::NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "",
            "a later script must be able to remove an earlier registration"
        );
    }
}

#[cfg(test)]
mod onclick_single_eval_tests {
    //! An inline `onclick` attribute must run exactly once per click, driven
    //! through the SAME helper the winit click path uses
    //! (`winit_backend::fire_page_click`) — a test calling `fire_click`
    //! directly cannot see a caller-side regression (the bug lived in the
    //! caller, which pre-evaluated the attribute and then fired it again).
    //!
    //! The counter lives in the DOM (`#out` text), not a JS `var`: a
    //! `bind_and_run` re-run would reset a script-scoped variable and hide
    //! the duplicate.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    #[test]
    fn inline_onclick_runs_once_per_click() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body>
            <button id="b" onclick="var o = document.getElementById('out'); o.setText('n=' + ((parseInt(o.textContent.split('=')[1]) || 0) + 1));">go</button>
            <div id="out">n=0</div>
            </body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, "");
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("b"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #b");
        crate::winit_backend::fire_page_click(&mut rt, &mut doc, NodeId::from_u64(b_id));
        let out = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "n=1",
            "an inline onclick handler must run exactly once per click"
        );
    }
}

#[cfg(test)]
mod document_listener_tests {
    //! L940 follow-up: a click listener registered on `document` must fire
    //! when a page element is clicked (event bubbling / capture to the
    //! document). `fire_click` used to look only at the target node, so
    //! `document.addEventListener('click', fn)` never ran on a real click.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn click_and_read(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="wrap"><button id="b">b</button></div><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("b"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #b");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(log));"#,
        );
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(id, _)| id.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn document_click_listener_fires_on_element_click() {
        let out = click_and_read(
            r#"
            var log = '';
            document.addEventListener('click', function () { log += 'D'; });
        "#,
        );
        assert_eq!(
            out, "D",
            "a document-level click listener must fire for an element click"
        );
    }

    #[test]
    fn ancestor_listener_fires_before_document() {
        let out = click_and_read(
            r#"
            var log = '';
            document.addEventListener('click', function () { log += 'D'; });
            document.getElementById('wrap').addEventListener('click', function () { log += 'W'; });
            document.getElementById('b').addEventListener('click', function () { log += 'B'; });
        "#,
        );
        assert_eq!(
            out, "BWD",
            "bubbling order: target, then ancestors outward, then document"
        );
    }
}

#[cfg(test)]
mod handler_this_props_tests {
    //! `this` inside an event listener must carry the element's properties.
    //! `fire_click` used to hand out a bare handle whose `id` / `tagName` were
    //! empty, so any handler branching on `this.id` silently took the empty
    //! branch (L940 follow-up).

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn click_and_read(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn" class="cta">go</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(log));"#,
        );
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(id, _)| id.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn this_id_is_populated() {
        let out = click_and_read(
            r#"
            var log = '';
            document.getElementById('btn').addEventListener('click', function () {
                log = this.id;
            });
        "#,
        );
        assert_eq!(
            out, "btn",
            "this.id inside a listener must be the element id"
        );
    }

    #[test]
    fn this_tag_name_and_class_are_populated() {
        let out = click_and_read(
            r#"
            var log = '';
            document.getElementById('btn').addEventListener('click', function () {
                log = this.tagName + '/' + this.className;
            });
        "#,
        );
        assert_eq!(
            out, "BUTTON/cta",
            "this.tagName and this.className must reflect the element"
        );
    }

    #[test]
    fn ancestor_listener_sees_its_own_node_not_the_target() {
        // DOM: `this` (currentTarget) is the node the listener is attached
        // to — an ancestor's handler must not see the click target.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="wrap"><button id="btn">go</button></div><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var log = '';
            document.getElementById('wrap').addEventListener('click', function () {
                log += 'wrap=' + this.id + ';';
            });
            document.getElementById('btn').addEventListener('click', function () {
                log += 'btn=' + this.id + ';';
            });
            "#,
        );
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(log));"#,
        );
        let out = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "btn=btn;wrap=wrap;",
            "the target's handler sees #btn and the ancestor's sees #wrap"
        );
    }

    #[test]
    fn this_works_for_inline_onclick_too() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body>
            <button id="btn" onclick="mark = this.id;">go</button>
            <div id="out">0</div>
            </body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, "var mark = '';");
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(mark));"#,
        );
        let out = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(out, "btn", "inline onclick's this.id must be populated");
    }
}

#[cfg(test)]
mod document_listener_lifecycle_tests {
    //! L940 follow-up: the document listener surface must behave like the DOM
    //! contract — `removeEventListener` actually unregisters, and `this`
    //! inside a document listener is the document object (passing null was
    //! coerced to the global object by sloppy-mode functions).

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn click_and_read(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn">go</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(log));"#,
        );
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(id, _)| id.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn document_remove_event_listener_unregisters() {
        let out = click_and_read(
            r#"
            var log = '';
            function onD() { log += 'D'; }
            document.addEventListener('click', onD);
            document.removeEventListener('click', onD);
        "#,
        );
        assert_eq!(out, "", "a removed document listener must not fire");
    }

    #[test]
    fn document_remove_leaves_other_document_listeners() {
        let out = click_and_read(
            r#"
            var log = '';
            function a() { log += 'A'; }
            function b() { log += 'B'; }
            document.addEventListener('click', a);
            document.addEventListener('click', b);
            document.removeEventListener('click', a);
        "#,
        );
        assert_eq!(out, "B", "only the removed document listener stops firing");
    }

    #[test]
    fn removal_leaves_no_stale_tail_entries() {
        // insert_property bypasses array [[Set]], so shrinking `length` alone
        // would leave the old indices behind as page-visible own properties.
        // The surviving listener fires on the click, so the measured value
        // lives in its own variable (`log` is appended to by the listener).
        let out = click_and_read(
            r#"
            var log = '';
            var report = 'unset';
            function a() {}
            function b() {}
            document.addEventListener('click', a);
            document.addEventListener('click', b);
            document.removeEventListener('click', b);
            var arr = document._js_listeners.click;
            // Count own INDICES only: `length` is an own property too.
            var idx = 0;
            for (var k in arr) { if (k !== 'length') idx++; }
            report = 'len=' + arr.length + '|idx=' + idx;
            log = report;
        "#,
        );
        assert_eq!(
            out, "len=1|idx=1",
            "the removed entry must leave no stale own property behind"
        );
    }

    #[test]
    fn document_listener_this_is_the_document() {
        let out = click_and_read(
            r#"
            var log = '';
            document.addEventListener('click', function () {
                // The document object is the only one with getElementById.
                log = (typeof this.getElementById === 'function') ? 'doc' : 'not-doc';
            });
        "#,
        );
        assert_eq!(
            out, "doc",
            "this inside a document listener must be the document"
        );
    }
}

#[cfg(test)]
mod handle_event_tests {
    //! DOM: an event listener is a function OR an object with a `handleEvent`
    //! method. The callable-only guard used to drop the latter, so such a
    //! listener registered fine but never ran (L940 follow-up).

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn click_and_read(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn">go</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(log));"#,
        );
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(id, _)| id.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn handle_event_object_runs_on_element_listener() {
        let out = click_and_read(
            r#"
            var log = '';
            var listener = {
                handleEvent: function () { log += 'H'; }
            };
            document.getElementById('btn').addEventListener('click', listener);
        "#,
        );
        assert_eq!(
            out, "H",
            "an object with handleEvent must be invoked for a click"
        );
    }

    #[test]
    fn handle_event_this_is_the_listener_object() {
        let out = click_and_read(
            r#"
            var log = '';
            var listener = {
                name: 'L',
                handleEvent: function () { log += this.name; }
            };
            document.getElementById('btn').addEventListener('click', listener);
        "#,
        );
        assert_eq!(
            out, "L",
            "inside handleEvent, `this` must be the listener object"
        );
    }

    #[test]
    fn handle_event_object_runs_on_document_listener() {
        let out = click_and_read(
            r#"
            var log = '';
            var listener = {
                name: 'D',
                handleEvent: function () { log += this.name; }
            };
            document.addEventListener('click', listener);
        "#,
        );
        assert_eq!(
            out, "D",
            "a handleEvent object registered on the document must be invoked"
        );
    }

    #[test]
    fn non_callable_handle_event_is_silently_ignored() {
        // An object whose `handleEvent` is not callable must register without
        // error and must not fire. NOTE (round-3 S3): the `is_callable()`
        // guard itself is NOT observable — invoking a non-callable throws a
        // TypeError that this dispatcher swallows with `let _ =`, so removing
        // the guard leaves behaviour identical. The guard is defence in depth;
        // this test pins the end-to-end contract, not the guard.
        let out = click_and_read(
            r#"
            var log = 'start|';
            document.getElementById('btn').addEventListener('click', {handleEvent: 42});
            log += 'registered';
        "#,
        );
        assert_eq!(
            out, "start|registered",
            "a non-callable handleEvent must not fire or surface an error"
        );
    }
}

#[cfg(test)]
mod fragment_and_eventtarget_tests {
    //! Two L940 screening findings:
    //!  - `document.createDocumentFragment().lastChild` was bound to the
    //!    firstChild getter, so it returned the FIRST child.
    //!  - `new EventTarget()`'s three methods were stubs: listeners were
    //!    discarded and dispatchEvent always returned true.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn run(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(id, _)| id.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn fragment_last_child_is_the_last_one() {
        let out = run(r#"
            var frag = document.createDocumentFragment();
            frag.appendChild(document.createTextNode('a'));
            frag.appendChild(document.createTextNode('b'));
            document.getElementById('out').setText(
                'first=' + frag.firstChild.data + ' last=' + frag.lastChild.data
            );
        "#);
        assert_eq!(
            out, "first=a last=b",
            "lastChild must be the last child, not the first"
        );
    }

    #[test]
    fn event_target_listener_runs() {
        let out = run(r#"
            var t = new EventTarget();
            t.addEventListener('ping', function () {
                document.getElementById('out').setText('fired');
            });
            t.dispatchEvent(new Event('ping'));
        "#);
        assert_eq!(
            out, "fired",
            "a listener on a self-made EventTarget must run"
        );
    }

    #[test]
    fn event_target_removal_stops_the_listener() {
        let out = run(r#"
            var t = new EventTarget();
            function h() { document.getElementById('out').setText('fired'); }
            t.addEventListener('ping', h);
            t.removeEventListener('ping', h);
            t.dispatchEvent(new Event('ping'));
        "#);
        assert_eq!(out, "0", "a removed EventTarget listener must not run");
    }

    #[test]
    fn event_target_sets_event_target_and_current_target() {
        let out = run(r#"
            var t = new EventTarget();
            var seen = 'unset';
            t.addEventListener('ping', function (e) {
                seen = (e.target === t ? 'T' : 't') + (e.currentTarget === t ? 'C' : 'c');
            });
            t.dispatchEvent(new Event('ping'));
            document.getElementById('out').setText(seen);
        "#);
        assert_eq!(
            out, "TC",
            "dispatchEvent must set event.target and event.currentTarget"
        );
    }

    #[test]
    fn event_target_is_set_even_without_listeners() {
        // Round-3 S3: the assignment used to sit inside the listener-array
        // guard, so a listener-less target left event.target null.
        let out = run(r#"
            var t = new EventTarget();
            var e = new Event('ping');
            t.dispatchEvent(e);
            document.getElementById('out').setText(e.target === t ? 'T' : 'null');
        "#);
        assert_eq!(out, "T", "dispatchEvent must set target unconditionally");
    }

    #[test]
    fn current_target_is_null_after_dispatch() {
        let out = run(r#"
            var t = new EventTarget();
            var during = 'unset';
            var e = new Event('ping');
            t.addEventListener('ping', function (ev) { during = ev.currentTarget === t ? 'C' : 'c'; });
            t.dispatchEvent(e);
            document.getElementById('out').setText(during + '|' + (e.currentTarget === null ? 'null' : 'stale'));
        "#);
        assert_eq!(
            out, "C|null",
            "currentTarget must be live during dispatch and null afterwards"
        );
    }

    #[test]
    fn html_document_fragment_last_child_is_the_last_one() {
        // Round-3: the third fragment builder is REACHABLE via
        // document.implementation.createHTMLDocument(...).
        let out = run(r#"
            var d = document.implementation.createHTMLDocument('t');
            var frag = d.createDocumentFragment();
            frag.appendChild(d.createTextNode('a'));
            frag.appendChild(d.createTextNode('b'));
            document.getElementById('out').setText(
                'first=' + frag.firstChild.data + ' last=' + frag.lastChild.data
            );
        "#);
        assert_eq!(
            out, "first=a last=b",
            "the createHTMLDocument fragment builder must report the last child"
        );
    }

    #[test]
    fn event_target_dispatches_only_the_matching_type() {
        let out = run(r#"
            var t = new EventTarget();
            t.addEventListener('other', function () {
                document.getElementById('out').setText('WRONG');
            });
            t.dispatchEvent(new Event('ping'));
        "#);
        assert_eq!(out, "0", "a listener for another type must not run");
    }
}

#[cfg(test)]
mod replay_idempotence_tests {
    //! The browser re-runs the page's `<script>` blocks on every resize /
    //! zoom / reload (`build_doc`). Each run used to register the same
    //! listener AGAIN under a fresh global name, so one click fired N times
    //! after N re-runs. `bind_page_scripts(.., replay = true)` numbers slots
    //! per (node, type) from zero and reconciles after the run, so an
    //! unchanged page re-uses the same names and collapses (L940 registry
    //! idempotence).

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn click_after_replays(script: &str, replays: usize) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn">go</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        let scripts = vec![script.to_string()];
        rt.bind_page_scripts(&mut doc, &scripts, "", true);
        for _ in 0..replays {
            rt.bind_page_scripts(&mut doc, &scripts, "", true);
        }
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText(String(count));"#,
        );
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(id, _)| id.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn a_replayed_script_fires_once() {
        let out = click_after_replays(
            r#"
            var count = 0;
            document.getElementById('btn').addEventListener('click', function () { count++; });
        "#,
            1,
        );
        assert_eq!(out, "1", "one replay must not double the handler");
    }

    #[test]
    fn five_replays_still_fire_once() {
        let out = click_after_replays(
            r#"
            var count = 0;
            document.getElementById('btn').addEventListener('click', function () { count++; });
        "#,
            5,
        );
        assert_eq!(out, "1", "five replays must still fire the handler once");
    }

    #[test]
    fn two_listeners_keep_their_order_across_replays() {
        // Two handlers must stay A-then-B (registration order) and each fire
        // exactly once after replays — not "ABAB" or "ABABAB".
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn">go</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        let scripts = vec![
            r#"
            var log = '';
            document.getElementById('btn').addEventListener('click', function () { log += 'A'; });
            document.getElementById('btn').addEventListener('click', function () { log += 'B'; });
            "#
            .to_string(),
        ];
        for _ in 0..3 {
            rt.bind_page_scripts(&mut doc, &scripts, "", true);
        }
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        let out = doc
            .get_node(NodeId::from_u64(b_id))
            .map(|_| ())
            .and(Some(()))
            .map(|_| ())
            .and_then(|_| {
                doc.get_node(NodeId::from_u64(
                    doc.tree()
                        .iter()
                        .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                        .map(|(id, _)| id.as_u64())
                        .expect("missing #out"),
                ))
            })
            .map(|n| n.text_content())
            .unwrap_or_default();
        // The out div is still "0" (no script writes it), so read `log`
        // through the runtime instead.
        rt.bind_and_run(&mut doc, "document.getElementById('out').setText(log);");
        let out2 = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        let _ = out;
        assert_eq!(
            out2, "AB",
            "two handlers keep registration order and fire once each after replays"
        );
    }
}

#[cfg(test)]
mod replay_shape_change_tests {
    //! Round-2 S1 repros: a page whose re-run registers a DIFFERENT shape.
    //! The bare counter reset silently dropped or stranded listeners here;
    //! slot reconciliation must keep the run's own set exactly.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn run_replays(script: &str, replays: usize) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn">go</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        let scripts = vec![script.to_string()];
        rt.bind_page_scripts(&mut doc, &scripts, "", true);
        for _ in 0..replays {
            rt.bind_page_scripts(&mut doc, &scripts, "", true);
        }
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            "document.getElementById('out').setText(String(log));",
        );
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(id, _)| id.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn a_later_run_that_registers_fewer_drops_the_stale_tail() {
        // Run 1 registers two handlers; later runs register one. The second
        // run's handler must NOT keep firing (round-2 S1 stale-extra).
        let out = run_replays(
            r#"
            var log = '';
            var count = 0;
            document.getElementById('btn').addEventListener('click', function () { log += 'A'; });
            document.getElementById('btn').addEventListener('click', function () { log += 'B'; });
            "#,
            0,
        );
        assert_eq!(out, "AB", "baseline: both handlers fire once");

        // Now a page whose first run registers two and whose replays register
        // one (a conditional that has since flipped).
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn">go</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_page_scripts(
            &mut doc,
            &vec![r#"
            var log = '';
            document.getElementById('btn').addEventListener('click', function () { log += 'ONCE'; });
            document.getElementById('btn').addEventListener('click', function () { log += 'TWICE'; });
            "#
            .to_string()],
            "",
            true,
        );
        rt.bind_page_scripts(
            &mut doc,
            &vec![
                r#"
            var log = '';
            document.getElementById('btn').addEventListener('click', function () { log += 'NEW'; });
            "#
                .to_string(),
            ],
            "",
            true,
        );
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            "document.getElementById('out').setText(String(log));",
        );
        let after = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            after, "NEW",
            "a later run with fewer listeners must drop the stale tail"
        );
    }

    #[test]
    fn an_order_swap_keeps_both_handlers() {
        // Round-2 S1: with a bare counter reset, a same-count but different
        // order made the displaced handler vanish. Per-(node,type) slots mean
        // the second run simply re-owns slots 0..n, so BOTH handlers survive
        // and the new registration order wins.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="btn">go</button><div id="out">0</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_page_scripts(
            &mut doc,
            &vec![
                r#"
            var log = '';
            document.getElementById('btn').addEventListener('click', function () { log += 'A'; });
            document.getElementById('btn').addEventListener('click', function () { log += 'B'; });
            "#
                .to_string(),
            ],
            "",
            true,
        );
        // Same count, swapped order.
        rt.bind_page_scripts(
            &mut doc,
            &vec![
                r#"
            var log = '';
            document.getElementById('btn').addEventListener('click', function () { log += 'B'; });
            document.getElementById('btn').addEventListener('click', function () { log += 'A'; });
            "#
                .to_string(),
            ],
            "",
            true,
        );
        let b_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("btn"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #btn");
        rt.fire_click(&mut doc, b_id);
        rt.bind_and_run(
            &mut doc,
            "document.getElementById('out').setText(String(log));",
        );
        let out = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(id, _)| id.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "BA",
            "both handlers survive an order swap and the new order wins"
        );
    }
}

#[cfg(test)]
mod text_content_assignment_tests {
    //! L940 S1 (goal rounds 9-10): `el.textContent = x` is one of the most
    //! common DOM writes in real pages, but the setter only rebuilt the
    //! JS-side `_children` mirror — the rendered document never changed. The
    //! retired `js_interactive` bridge masked the gap by rewriting the source
    //! to `setText`, and `offline_onclick_test` guarded that dead path.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn read(doc: &HtmlDocument, id: &str) -> String {
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                .map(|(i, _)| i.as_u64())
                .expect("missing node"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn assigning_text_content_reaches_the_dom() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">empty</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').textContent = 'Hello aris';"#,
        );
        assert_eq!(
            read(&doc, "out"),
            "Hello aris",
            "assigning textContent must update the rendered document"
        );
    }

    #[test]
    fn the_readback_reflects_the_assignment() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">empty</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.textContent = 'x';
            document.getElementById('res').setText(el.textContent);
            "#,
        );
        assert_eq!(read(&doc, "res"), "x", "the getter must see the new value");
        assert_eq!(read(&doc, "out"), "x", "and so must the DOM");
    }

    #[test]
    fn a_clear_then_set_cycle_does_not_resurrect_the_old_text() {
        // Round-2 S1: the counter pattern. An empty assignment must be
        // authoritative — the getter may not fall back to the parsed value.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="c">5</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var c = document.getElementById('c');
            c.textContent = '';
            c.textContent = '6';
            document.getElementById('res').setText(c.textContent);
            "#,
        );
        assert_eq!(read(&doc, "c"), "6", "the DOM must hold the new value");
        assert_eq!(read(&doc, "res"), "6", "and the getter must agree with it");
    }

    #[test]
    fn clearing_is_visible_to_the_getter() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="c">5</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var c = document.getElementById('c');
            c.textContent = '';
            document.getElementById('res').setText('[' + c.textContent + ']');
            "#,
        );
        assert_eq!(read(&doc, "c"), "", "the DOM text must be cleared");
        assert_eq!(
            read(&doc, "res"),
            "[]",
            "the getter must report empty, not the pre-clear value"
        );
    }

    #[test]
    fn repeated_assignment_keeps_the_last_value() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">z</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.textContent = 'a';
            el.textContent = 'b';
            document.getElementById('res').setText(el.textContent);
            "#,
        );
        assert_eq!(read(&doc, "out"), "b", "the last assignment wins");
        assert_eq!(read(&doc, "res"), "b", "and the getter agrees");
    }

    #[test]
    fn a_fresh_handle_sees_text_written_through_another_handle() {
        // Round-3 S1: the write flag must be keyed by NODE. `getElementById`
        // returns a new handle every call, so a per-handle flag let the DOM
        // say "NEW" while a second handle read the parsed "5".
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="c">5</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('c').textContent = 'NEW';
            // A DIFFERENT handle for the same node:
            document.getElementById('res').setText(document.getElementById('c').textContent);
            "#,
        );
        assert_eq!(read(&doc, "c"), "NEW", "the DOM must hold the write");
        assert_eq!(
            read(&doc, "res"),
            "NEW",
            "a freshly-fetched handle must read the written value, not the snapshot"
        );
    }

    #[test]
    fn a_handle_captured_before_the_write_sees_it_too() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="c">5</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var held = document.getElementById('c');   // captured BEFORE the write
            document.getElementById('c').textContent = 'NEW';  // other handle writes
            document.getElementById('res').setText(held.textContent);
            "#,
        );
        assert_eq!(
            read(&doc, "res"),
            "NEW",
            "a handle captured before the write must also observe it"
        );
    }

    /// FIXED by the pending-tuple carry (was the #76 known divergence): a
    /// child created with `createElement`, given text, and appended now lands
    /// WITH its text, so the DOM and the getter agree.
    #[test]
    fn append_after_a_text_write_is_consistent() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="c">5</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var c = document.getElementById('c');
            c.textContent = 'A';
            var b = document.createElement('b');
            b.textContent = 'B';
            c.appendChild(b);
            document.getElementById('res').setText(c.textContent);
            "#,
        );
        assert_eq!(
            read(&doc, "c"),
            "AB",
            "the DOM must hold A and the appended B"
        );
        assert_eq!(
            read(&doc, "res"),
            "AB",
            "textContent must agree with the DOM now that the pending write lands"
        );
    }

    #[test]
    fn set_text_is_visible_to_a_freshly_fetched_handle() {
        // Round-4 S2: `setText` used to leave the node snapshot stale (and to
        // overwrite the textContent accessor with a data property), so a new
        // handle read the pre-write value.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="c">5</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('c').setText('M');
            document.getElementById('res').setText(document.getElementById('c').textContent);
            "#,
        );
        assert_eq!(read(&doc, "c"), "M", "the DOM must hold the setText value");
        assert_eq!(
            read(&doc, "res"),
            "M",
            "a fresh handle must read the setText value, not the parsed text"
        );
    }

    #[test]
    fn an_empty_assignment_clears_the_text() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">gone</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.textContent = '';
            document.getElementById('res').setText('[' + el.textContent + ']');
            "#,
        );
        assert_eq!(
            read(&doc, "out"),
            "",
            "assigning an empty string must clear the element's text"
        );
        assert_eq!(
            read(&doc, "res"),
            "[]",
            "and the getter must not resurrect the parsed text"
        );
    }
}

#[cfg(test)]
mod style_css_text_tests {
    //! `el.style.cssText = v` used to throw (`style` was undefined), and the
    //! retired `js_interactive` bridge hid that by rewriting the source to
    //! `setAttribute('style', v)`. The production path now exposes a style
    //! object whose `cssText` writes the attribute.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    #[test]
    fn css_text_assigns_the_style_attribute() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">x</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').style.cssText = 'color:#ff0000';"#,
        );
        let style = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
            .and_then(|(id, _)| doc.get_node(id))
            .and_then(|n| {
                n.attr(blitz_dom::local_name!("style"))
                    .map(|v| v.to_string())
            })
            .unwrap_or_default();
        assert_eq!(
            style, "color:#ff0000",
            "cssText must write the style attribute"
        );
    }

    #[test]
    fn styling_a_pending_create_element_handle_does_not_panic() {
        // Round-2 S1: a `createElement` handle has no tree id yet (`nid == 0`),
        // and pushing an op against it panicked inside blitz. Master raised a
        // caught JS TypeError here; the fix must not turn that into a crash.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="list"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var d = document.createElement('div');
            d.style.cssText = 'color:red';
            document.getElementById('list').appendChild(d);
            "#,
        );
        // Reaching here at all is the assertion: no panic.
        assert!(
            doc.tree()
                .iter()
                .any(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("list"))
        );
    }

    #[test]
    fn css_text_is_readable_back() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">x</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.style.cssText = 'color:red';
            document.getElementById('res').setText(el.style.cssText);
            "#,
        );
        let res = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
            .and_then(|(id, _)| doc.get_node(id))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(res, "color:red", "cssText must read back what was written");
    }
}

#[cfg(test)]
mod apply_ops_drain_tests {
    //! L940 S1: `apply_ops` used to CLONE the op log and never clear it, so
    //! every call replayed everything recorded so far and `Op::AppendChild`
    //! minted a fresh node per replay — one inline onclick append landed two
    //! nodes and each further click added four more (unbounded). The drain
    //! makes every op apply exactly once.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    #[test]
    fn repeated_clicks_grow_the_list_by_exactly_one_each() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><button id="add" onclick="var d = document.createElement('div'); document.getElementById('list').appendChild(d);">add</button><div id="list"></div><div id="out"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, "");
        let add_id = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("add"))
            .map(|(id, _)| id.as_u64())
            .expect("missing #add");
        let count = |doc: &HtmlDocument| -> usize {
            doc.get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("list"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #list"),
            ))
            .map(|n| n.children.len())
            .unwrap_or(0)
        };
        for expected in 1..=5 {
            rt.fire_click(&mut doc, add_id);
            assert_eq!(
                count(&doc),
                expected,
                "click {expected}: the list must grow by exactly one per click"
            );
        }
    }
}

#[cfg(test)]
mod pending_write_tests {
    //! L940 (#78 residual): a detached `createElement` handle has no tree id,
    //! so `textContent` / `setAttribute` / `style.cssText` writes used to be
    //! dropped at the source — the most common dynamic-DOM pattern
    //! (create → set → append) produced empty nodes. The writes now ride the
    //! pending tuple and land when the node is attached.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn read_list_child(doc: &HtmlDocument) -> (usize, String) {
        let node = doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("list"))
                .map(|(i, _)| i.as_u64())
                .expect("missing #list"),
        ));
        match node {
            Some(n) => match n.children.last() {
                Some(&cid) => (
                    n.children.len(),
                    doc.get_node(cid)
                        .map(|c| c.text_content())
                        .unwrap_or_default(),
                ),
                None => (n.children.len(), String::new()),
            },
            None => (0, String::new()),
        }
    }

    #[test]
    fn detached_text_content_lands_on_append() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="list"></div><div id="out"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var d = document.createElement('div');
            d.textContent = 'hello';
            document.getElementById('list').appendChild(d);
            document.getElementById('out').setText(d.textContent);
            "#,
        );
        let (n, text) = read_list_child(&doc);
        assert_eq!(n, 1, "the node must be appended");
        assert_eq!(text, "hello", "the detached write must land");
    }

    #[test]
    fn detached_set_attribute_lands_on_append() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="list"></div><div id="out"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var d = document.createElement('div');
            d.setAttribute('title', 'v1');
            document.getElementById('list').appendChild(d);
            "#,
        );
        // Read the attribute from the live tree: the JS-side `children`/
        // `lastChild` mirrors are empty on getElementById handles (pre-existing
        // gap), so a JS read-back cannot see the appended node at all.
        let k = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("title")) == Some("v1"))
            .map(|_| "v1")
            .unwrap_or("MISSING");
        assert_eq!(
            k, "v1",
            "an attribute set before append must land on the attached node"
        );
    }

    #[test]
    fn detached_style_css_text_lands_on_append() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="list"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var d = document.createElement('div');
            d.style.cssText = 'color:red';
            document.getElementById('list').appendChild(d);
            "#,
        );
        let style = doc
            .tree()
            .iter()
            .find(|(_, n)| {
                n.parent.is_some()
                    && doc
                        .get_node(n.parent.expect("parent"))
                        .map(|p| p.attr(blitz_dom::local_name!("id")) == Some("list"))
                        .unwrap_or(false)
            })
            .and_then(|(id, _)| doc.get_node(id))
            .and_then(|n| {
                n.attr(blitz_dom::local_name!("style"))
                    .map(|v| v.to_string())
            })
            .unwrap_or_default();
        assert_eq!(
            style, "color:red",
            "a detached node's cssText must land as the style attribute"
        );
    }
}

#[cfg(test)]
mod pending_write_shape_tests {
    //! Round-2 shapes: repeated attribute writes must be last-wins with a
    //! single entry, and a handle captured before a rebind must NOT route its
    //! write onto a different, freshly-created node.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    #[test]
    fn repeated_attribute_writes_are_last_wins_with_one_entry() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="list"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var d = document.createElement('div');
            d.setAttribute('title', 'v1');
            d.setAttribute('title', 'v2');
            document.getElementById('list').appendChild(d);
            "#,
        );
        let title = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("title")) == Some("v2"))
            .and_then(|(id, _)| doc.get_node(id))
            .and_then(|n| {
                n.attr(blitz_dom::local_name!("title"))
                    .map(|v| v.to_string())
            });
        assert_eq!(title, Some("v2".to_string()), "the last write must win");
        // exactly ONE title attribute on the node (no stacked duplicates)
        let dupes = doc
            .tree()
            .iter()
            .filter(|(_, n)| n.attr(blitz_dom::local_name!("title")).is_some())
            .count();
        assert_eq!(dupes, 1, "no duplicate title attributes may exist");
    }

    #[test]
    fn a_stale_handle_write_after_rebind_does_not_misroute() {
        // Round-2 S2: pending ids used to restart at 0 on every bind, so a
        // handle captured in run 1 shared its pid with run 2's freshly-created
        // node — the stale write POISONED the new node's tuple before the
        // append applied, and the fresh node landed with the stale text.
        // Monotonic ids mean the stale write finds no tuple and drops.
        //
        // Shape (round-3): the stale write must happen in the SAME bind as
        // fresh's create/append — before apply_ops runs — or the bind-entry
        // pending clear hides the collision entirely.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="list"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var stale = document.createElement('div');
            stale.textContent = 'OLD';
            document.getElementById('list').appendChild(stale);
            "#,
        );
        // rebind: ONE bind does fresh's create + append AND the stale write,
        // so with a pid reset the stale write poisons fresh's tuple.
        rt.bind_page_scripts(
            &mut doc,
            &vec![
                r#"
            var f = document.createElement('div');
            f.textContent = 'FRESH';
            document.getElementById('list').appendChild(f);
            stale.textContent = 'HIJACK';
            "#
                .to_string(),
            ],
            "",
            true,
        );
        // Count ELEMENT nodes only: a text child mirrors its parent's text,
        // so a naive text match counts the div and its text node as two.
        let hijack = doc
            .tree()
            .iter()
            .filter(|(_, n)| n.text_content() == "HIJACK" && !n.children.is_empty())
            .count();
        assert_eq!(
            hijack, 0,
            "a stale handle's write must not poison the fresh node"
        );
        let fresh = doc
            .tree()
            .iter()
            .filter(|(_, n)| n.text_content() == "FRESH" && !n.children.is_empty())
            .count();
        assert_eq!(fresh, 1, "the fresh node must be untouched");
    }
}

#[cfg(test)]
mod set_html_tests {
    //! Two L940 defects on the `setHTML` (innerHTML) surface, both of the
    //! classes already fixed for `setText` / `textContent`:
    //! - a detached `createElement` write was dropped at the source (no tree
    //!   id, no pending tuple write) — the text never landed on append;
    //! - the setter inserted a `textContent` DATA property, replacing the
    //!   accessor and silently breaking later reads through the same handle.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn read_out(doc: &HtmlDocument) -> String {
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                .map(|(i, _)| i.as_u64())
                .expect("missing #out"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn set_html_updates_the_dom_and_the_handle_readback() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">empty</div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.setHTML('<b>hi</b>');
            document.getElementById('out').setText(el.textContent);
            "#,
        );
        // DOM: the stripped text landed.
        assert_eq!(
            read_out(&doc),
            "hi",
            "setHTML must update the rendered document (tags stripped)"
        );
    }

    #[test]
    fn set_html_does_not_clobber_the_text_content_accessor() {
        // Round-2 class: the data-property insert replaced the accessor, so a
        // later `el.textContent` read through the SAME handle returned the
        // stale data property forever.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">empty</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.setHTML('x');
            el.textContent = 'fresh';
            document.getElementById('res').setText(el.textContent);
            "#,
        );
        assert_eq!(
            read_out(&doc),
            "fresh",
            "a textContent write after setHTML must still reach the DOM"
        );
    }

    #[test]
    fn detached_set_html_lands_on_append() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="list"></div><div id="out"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var d = document.createElement('div');
            d.setHTML('carried');
            document.getElementById('list').appendChild(d);
            "#,
        );
        let text = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("list"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #list"),
            ))
            .and_then(|n| n.children.last().and_then(|&cid| doc.get_node(cid)))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            text, "carried",
            "a detached setHTML write must ride the pending tuple"
        );
    }
}

#[cfg(test)]
mod style_shadowing_tests {
    //! L940 S2 (#79 R3): `setAttribute('style', v)` inserted a STRING data
    //! property over the style OBJECT, so `el.style` became a primitive and
    //! every later `el.style.cssText = …` silently no-op'ed.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn setup(script: &str) -> (HtmlDocument, String, String) {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">x</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        let dom_style = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
            .and_then(|(id, _)| doc.get_node(id))
            .and_then(|n| {
                n.attr(blitz_dom::local_name!("style"))
                    .map(|v| v.to_string())
            })
            .unwrap_or_default();
        let res = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        (doc, dom_style, res)
    }

    #[test]
    fn set_attribute_style_keeps_the_style_object() {
        let (_, dom_style, res) = setup(
            r#"
            var el = document.getElementById('out');
            el.setAttribute('style', 'color:blue');
            var kind = typeof el.style;
            var css = (typeof el.style === 'object' && el.style.cssText !== undefined)
                ? String(el.style.cssText) : 'UNDEFINED';
            document.getElementById('res').setText(kind + '|' + css);
            "#,
        );
        assert_eq!(
            dom_style, "color:blue",
            "the DOM style attribute must be set"
        );
        assert_eq!(
            res, "object|color:blue",
            "el.style must stay an object with the cssText reflected"
        );
    }

    #[test]
    fn css_text_write_after_set_attribute_still_works() {
        // The exact silent-no-op shape: setAttribute first, then cssText.
        let (_, dom_style, res) = setup(
            r#"
            var el = document.getElementById('out');
            el.setAttribute('style', 'color:blue');
            el.style.cssText = 'color:red';
            var css = String(el.style.cssText);
            document.getElementById('res').setText(css);
            "#,
        );
        assert_eq!(
            dom_style, "color:red",
            "the later cssText write must win in the DOM"
        );
        assert_eq!(res, "color:red", "and the cssText readback must agree");
    }

    #[test]
    fn plain_attribute_writes_are_unaffected() {
        let (_, dom_style, res) = setup(
            r#"
            var el = document.getElementById('out');
            el.setAttribute('title', 'v1');
            el.setAttribute('title', 'v2');
            document.getElementById('res').setText(el.getAttribute('title'));
            "#,
        );
        assert_eq!(dom_style, "", "no style attribute should exist");
        assert_eq!(res, "v2", "non-style attributes keep their path");
    }
}

#[cfg(test)]
mod style_shadowing_fresh_handle_tests {
    //! Round-2 S2: the two paths that defeated the fix on FRESHLY-FETCHED
    //! handles (the `getElementById` case — the most common). Both are
    //! one-line special-cases of the same shape the PR already implemented
    //! for `setAttribute`.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    #[test]
    fn the_cache_property_is_hidden_from_object_keys_on_all_paths() {
        // Round-4: two of the three cache-write sites still used enumerable
        // descriptors (populate_props seed / removeAttribute clear), so
        // Object.keys(el.style) leaked the internal name on fresh,
        // parsed-style and post-removeAttribute handles.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body>
            <div id="out" style="color:blue">x</div>
            <div id="out2">y</div>
            <div id="res"></div>
            </body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var parsed = document.getElementById('out');
            parsed.setAttribute('style', 'color:green');   // live-handle write
            var fresh = document.getElementById('out');    // fresh handle
            document.getElementById('out').removeAttribute('style');
            // fresh keys after all three write paths
            var k1 = Object.keys(parsed.style).join(',');
            var k2 = Object.keys(fresh.style).join(',');
            document.getElementById('res').setText('P[' + k1 + ']F[' + k2 + ']');
            "#,
        );
        let res = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            res, "P[cssText]F[cssText]",
            "the cache must be hidden from Object.keys on every path"
        );
    }

    #[test]
    fn a_fresh_handle_keeps_the_style_object_from_a_parsed_style_attribute() {
        // parse-time style="color:blue": a fresh `getElementById` handle must
        // carry the style OBJECT (not a string) with the parsed value seeded
        // as its cssText.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out" style="color:blue">x</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            var kind = typeof el.style;
            var css = String(el.style.cssText);
            document.getElementById('res').setText(kind + '|' + css);
            "#,
        );
        let res = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            res, "object|color:blue",
            "a fresh handle must carry the style object with the parsed cssText"
        );
    }

    #[test]
    fn remove_attribute_style_keeps_the_style_object_readable() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out" style="color:blue">x</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.removeAttribute('style');
            var kind = typeof el.style;
            var css = 'THROWS';
            try { css = String(el.style.cssText); } catch (e) { /* stays THROWS */ }
            var written = 'THROWS';
            try { el.style.cssText = 'color:red'; written = String(el.style.cssText); } catch (e) { /* stays */ }
            document.getElementById('res').setText(kind + '|' + css + '|' + written);
            "#,
        );
        let res = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            res, "object||color:red",
            "removeAttribute('style') must keep the style object usable"
        );
        let dom_style = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
            .and_then(|(id, _)| doc.get_node(id))
            .and_then(|n| {
                n.attr(blitz_dom::local_name!("style"))
                    .map(|v| v.to_string())
            })
            .unwrap_or_else(|| "__present__".to_string());
        // PINNED production behaviour: `removeAttribute` is a JS-side mirror
        // (it pushes no op), so the DOM keeps the old style attribute. The
        // JS-side object/readback contract is what this test pins; DOM removal
        // belongs to the pending-setattr gap.
        assert_eq!(
            dom_style, "color:blue",
            "pinned: removeAttribute is JS-mirror only — DOM keeps the value"
        );
    }

    #[test]
    fn a_fresh_handle_sees_css_text_written_via_set_attribute() {
        // The exact gap: setAttribute('style') on ONE handle, then a FRESH
        // getElementById handle must see the style (both an object and the
        // attribute).
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">x</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('out').setAttribute('style', 'color:blue');
            var el = document.getElementById('out');   // FRESH handle
            var kind = typeof el.style;
            var css = String(el.style.cssText);
            document.getElementById('res').setText(kind + '|' + css);
            "#,
        );
        let res = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            res, "object|color:blue",
            "a fresh handle must reflect the setAttribute style"
        );
    }
}

#[cfg(test)]
mod text_setter_mirror_tests {
    //! L940 getter-precedence, three shapes: the `textContent` getter prefers
    //! the `_children` mirror, so every element text setter must keep that
    //! mirror in step. `setText` / `setHTML` used to update only the DOM +
    //! snapshot, leaving the mirror stale — a held handle then read the
    //! pre-write value while the DOM held the new one.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn run(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">base</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                .map(|(i, _)| i.as_u64())
                .expect("missing #res"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn shape_i_text_then_set_html_is_visible_to_the_held_handle() {
        let out = run(r#"
            var el = document.getElementById('out');
            el.textContent = 'AAA';
            el.setHTML('BBB');
            document.getElementById('res').setText(el.textContent);
            "#);
        assert_eq!(
            out, "BBB",
            "setHTML after a textContent write must update the mirror the getter reads"
        );
    }

    #[test]
    fn shape_iiia_set_text_as_the_last_write_is_readable() {
        // The discriminating shape for setText's rebuild: the mirror was
        // populated by an earlier textContent write; setText is the LAST
        // write. Without setText's rebuild the getter reads the stale 'AAA'.
        let out = run(r#"
            var el = document.getElementById('out');
            el.textContent = 'AAA';
            el.setText('BBB');
            document.getElementById('res').setText(el.textContent);
            "#);
        assert_eq!(
            out, "BBB",
            "setText as the last write must update the mirror the getter reads"
        );
    }

    #[test]
    fn shape_iii_set_text_then_read_via_held_handle() {
        let out = run(r#"
            var el = document.getElementById('out');
            el.setText('AAA');
            el.textContent = 'BBB';
            document.getElementById('res').setText(el.textContent);
            "#);
        assert_eq!(
            out, "BBB",
            "textContent write after setText must win and be readable"
        );
    }

    #[test]
    fn set_text_then_set_html_last_wins() {
        let out = run(r#"
            var el = document.getElementById('out');
            el.setText('AAA');
            el.setHTML('BBB');
            document.getElementById('res').setText(el.textContent);
            "#);
        assert_eq!(out, "BBB", "the last setter wins for both DOM and readback");
    }

    #[test]
    fn interleaved_setters_stay_consistent_with_the_dom() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">base</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.setText('one');
            el.setHTML('two');
            el.textContent = 'three';
            // DOM text via a marker (setText is known-good)
            el.setText(el.textContent);
            document.getElementById('res').setText(document.getElementById('out').textContent);
            "#,
        );
        // DOM text is read via the same getter path the page uses.
        let dom = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            dom, "three",
            "the DOM must hold the last write after interleaved setters"
        );
    }
}

#[cfg(test)]
mod set_text_replace_tests {
    //! L940 (#83 R2 S2): assigning `textContent` must REPLACE children, per
    //! DOM. The old implementation only rewrote the first text child, so old
    //! element children survived — the DOM held old+new while the JS mirror
    //! held exactly the new value, and a held handle vs a freshly-fetched one
    //! answered differently.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn dom_text_and_tags(doc: &HtmlDocument, id: &str) -> (String, Vec<String>) {
        let nid = doc
            .tree()
            .iter()
            .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
            .map(|(i, _)| i)
            .expect("missing node");
        let node = doc.get_node(nid).expect("missing node data");
        let text = node.text_content();
        let tags: Vec<String> = node
            .children
            .iter()
            .filter_map(|&c| doc.get_node(c))
            .map(|cn| {
                if cn.text_data().is_some() {
                    "#text".to_string()
                } else {
                    cn.element_data()
                        .map(|e| e.name.local.to_string())
                        .unwrap_or_else(|| "?".to_string())
                }
            })
            .collect();
        (text, tags)
    }

    #[test]
    fn set_text_replaces_element_children() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out"><p>a</p><p>b</p></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').setText('new');"#,
        );
        let (text, tags) = dom_text_and_tags(&doc, "out");
        assert_eq!(text, "new", "the DOM must hold only the new text");
        assert_eq!(
            tags,
            vec!["#text".to_string()],
            "the old <p> children must be gone (replace, not append)"
        );
    }

    #[test]
    fn text_content_assignment_replaces_element_children() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out"><p>a</p><p>b</p></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"document.getElementById('out').textContent = 'new';"#,
        );
        let (text, tags) = dom_text_and_tags(&doc, "out");
        assert_eq!(text, "new", "textContent must replace the children");
        assert_eq!(tags, vec!["#text".to_string()], "old children removed");
    }

    #[test]
    fn set_text_empty_clears_parsed_element_children() {
        // The shape L940 recorded: an empty write must clear the element's
        // parsed children, not leave them behind while the mirror says empty.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out">Hello <b>world</b></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, r#"document.getElementById('out').setText('');"#);
        let (text, tags) = dom_text_and_tags(&doc, "out");
        assert_eq!(text, "", "the DOM must be empty");
        assert!(
            tags.is_empty(),
            "every child (including the parsed <b>) must be removed, got {tags:?}"
        );
    }

    #[test]
    fn a_pre_held_child_handle_does_not_panic_after_a_replace() {
        // Round-4 S0: removing children FREES their slab slots, but a handle
        // the page captured earlier keeps its `_arisId`. Mutating through such
        // a handle used to index `doc.nodes[id]` inside blitz and panic
        // ("invalid SlotMap key used") — master survived because it never
        // removed children. Reaching the end of this test is the assertion.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body>
            <div id="pc"><button id="kid">b</button></div>
            <div id="out">base</div>
            </body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var kid = document.getElementById('kid');   // pre-held child
            document.getElementById('pc').setText('new'); // frees its slot
            var pc = document.getElementById('pc');
            kid.setAttribute('class', 'x');   // stale write: must be dropped
            kid.setText('ZOMBIE');            // stale write: must be dropped
            pc.appendChild(kid);              // stale append: must be dropped
            document.getElementById('out').setText('survived');
            "#,
        );
        let out = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #out"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "survived",
            "stale child writes must be dropped, not panic"
        );
        // The parent still holds exactly the replacement text.
        let (text, tags) = dom_text_and_tags(&doc, "pc");
        assert_eq!(text, "new", "the replace result must be intact");
        assert_eq!(
            tags,
            vec!["#text".to_string()],
            "no stale child re-attached"
        );
    }

    #[test]
    fn a_held_and_a_fresh_handle_agree_after_a_replace() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="out"><p>a</p></div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var el = document.getElementById('out');
            el.setText('new');
            var held = el.textContent;
            var fresh = document.getElementById('out').textContent;
            document.getElementById('res').setText(held + '|' + fresh);
            "#,
        );
        let res = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            res, "new|new",
            "held and freshly-fetched handles must agree after a replace"
        );
    }
}

#[cfg(test)]
mod child_nodes_accessor_tests {
    //! L940 F3: `populate_props` built a `childNodes` array WITHOUT a `length`
    //! property and an always-empty `children`, so a freshly-fetched handle
    //! read `childNodes.length === undefined` even when the parsed element
    //! really had children.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn run(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="list"><li>a</li><li>b</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                .map(|(i, _)| i.as_u64())
                .expect("missing #res"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn a_fresh_handle_reports_child_nodes_length() {
        let out = run(r#"
            var el = document.getElementById('list');
            document.getElementById('res').setText('n=' + el.childNodes.length);
            "#);
        assert_eq!(out, "n=2", "a parsed element with two <li> must report 2");
    }

    #[test]
    fn children_reflects_the_same_count() {
        let out = run(r#"
            var el = document.getElementById('list');
            document.getElementById('res').setText(
                'children=' + el.children.length +
                '|count=' + el.childElementCount
            );
            "#);
        assert_eq!(
            out, "children=2|count=2",
            "children and childElementCount must follow the parsed tree"
        );
    }

    #[test]
    fn children_counts_only_element_children_not_text_nodes() {
        // Round-2 S2: `children` / `childElementCount` must count ELEMENTS.
        // `text<b>x</b>more` has 3 children but 1 element child.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="m">text<b>x</b>more</div><div id="all">only text</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var m = document.getElementById('m');
            var all = document.getElementById('all');
            document.getElementById('res').setText(
                'cn=' + m.childNodes.length +
                '|ch=' + m.children.length +
                '|cec=' + m.childElementCount +
                '|textonly_cec=' + all.childElementCount
            );
            "#,
        );
        let out = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "cn=3|ch=1|cec=1|textonly_cec=0",
            "children/childElementCount must exclude text nodes"
        );
    }

    #[test]
    fn a_deep_clone_keeps_its_collection_lengths() {
        // Round-4 S1: cloneNode(true) rebuilt the arrays copying only OBJECT
        // values, so `length` (a number) was dropped — the clone read
        // childNodes.length === undefined and hasChildNodes() answered FALSE
        // for a node with children. That is exactly the symptom this PR fixes,
        // re-created through the clone path.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="m">text<b>x</b>more</div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var m = document.getElementById('m');
            var c = m.cloneNode(true);
            document.getElementById('res').setText(
                'cn=' + c.childNodes.length +
                '|ch=' + c.children.length +
                '|cec=' + c.childElementCount +
                '|has=' + c.hasChildNodes()
            );
            "#,
        );
        let out = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(
            out, "cn=3|ch=1|cec=1|has=true",
            "a deep clone must keep its collection lengths and answer hasChildNodes()"
        );
    }

    #[test]
    fn an_element_without_children_reports_zero() {
        let out = run(r#"
            var el = document.getElementById('res');
            document.getElementById('res').setText('n=' + el.childNodes.length);
            "#);
        assert_eq!(out, "n=0", "an empty element must report 0, not undefined");
    }
}

#[cfg(test)]
mod remove_child_dom_tests {
    //! L940 (PR #85 R7): `removeChild` only updated the JS `_children` mirror
    //! and pushed NO op, so the parsed tree kept the child — even a rebind
    //! could not see a removal that never happened.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn child_count(doc: &HtmlDocument, id: &str) -> usize {
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                .map(|(i, _)| i.as_u64())
                .expect("missing node"),
        ))
        .map(|n| n.children.len())
        .unwrap_or(usize::MAX)
    }

    #[test]
    fn remove_child_takes_the_node_out_of_the_tree() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="list"><li id="a">a</li><li id="b">b</li><li id="c">c</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        assert_eq!(child_count(&doc, "list"), 3, "precondition");
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var list = document.getElementById('list');
            var b = document.getElementById('b');
            list.removeChild(b);
            "#,
        );
        assert_eq!(
            child_count(&doc, "list"),
            2,
            "removeChild must actually remove the node from the tree"
        );
    }

    #[test]
    fn removing_the_same_child_twice_does_not_panic() {
        // Round-2 S0: the op was pushed twice and `remove_and_drop_node`
        // indexes the (now freed) slab key -> the process aborted. Master's
        // no-op was safe only because it pushed nothing.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="list"><li id="a">a</li><li id="b">b</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var list = document.getElementById('list');
            var b = document.getElementById('b');
            list.removeChild(b);
            var second = 'ok';
            try { list.removeChild(b); } catch (e) { second = 'threw'; }
            document.getElementById('res').setText('second=' + second);
            "#,
        );
        // Reaching here at all is the assertion (no panic).
        assert_eq!(
            child_count(&doc, "list"),
            1,
            "only one removal may take effect"
        );
    }

    #[test]
    fn removing_a_node_that_is_not_a_child_throws_and_destroys_nothing() {
        // Round-2 S1: the "assume it's in the tree" fallback silently deleted
        // an UNRELATED live node.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="list"><li id="a">a</li></ul><ul id="other"><li id="cousin">c</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var list = document.getElementById('list');
            var cousin = document.getElementById('cousin');
            list.removeChild(cousin);
            "#,
        );
        let cousin_alive = doc
            .tree()
            .iter()
            .any(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("cousin"));
        assert!(
            cousin_alive,
            "a non-child removal must not delete an unrelated live node"
        );
        assert_eq!(child_count(&doc, "list"), 1, "the parent keeps its child");
    }

    #[test]
    fn a_removed_child_is_gone_after_a_rebind() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="list"><li id="a">a</li><li id="b">b</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('list').removeChild(document.getElementById('b'));
            "#,
        );
        // A later run must not see the removed child.
        rt.bind_and_run(&mut doc, r#"var gone = document.getElementById('b');"#);
        assert_eq!(child_count(&doc, "list"), 1, "the tree must hold one child");
        // removeChild DETACHES (remove_node), it does not drop: `b` stays in
        // the slab without a parent — exactly what makes the re-insert/move
        // idiom possible. Assert the detachment, not slab disappearance.
        let b_detached = doc
            .tree()
            .iter()
            .filter(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("b"))
            .all(|(_, n)| n.parent.is_none());
        assert!(
            b_detached,
            "the removed node must be detached (no parent), not gone"
        );
    }
}

#[cfg(test)]
mod insert_before_dom_tests {
    //! L940 (PR #86 R2): `insertBefore` was mirror-only, so a remove+insert
    //! "move" DESTROYED the node from the tree instead of relocating it.
    //! With RemoveChild landing (#86) and InsertBefore landing here, the move
    //! finally works end to end.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn kid_ids(doc: &HtmlDocument, id: &str) -> Vec<String> {
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                .map(|(i, _)| i.as_u64())
                .expect("missing node"),
        ))
        .map(|n| {
            n.children
                .iter()
                .filter_map(|&c| doc.get_node(c))
                .map(|cn| {
                    cn.attr(blitz_dom::local_name!("id"))
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "#text".to_string())
                })
                .collect()
        })
        .unwrap_or_default()
    }

    #[test]
    fn insert_before_relocates_an_existing_child() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="list"><li id="a">a</li><li id="b">b</li><li id="anchor">c</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var list = document.getElementById('list');
            var b = document.getElementById('b');
            var anchor = document.getElementById('anchor');
            list.insertBefore(b, anchor);
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "list"),
            vec!["a".to_string(), "b".to_string(), "anchor".to_string()],
            "b must be relocated before the anchor, not duplicated or destroyed"
        );
    }

    #[test]
    fn remove_then_insert_moves_a_node_without_destroying_it() {
        // The exact idiom that used to destroy the node: removeChild freed
        // the tree slot while insertBefore never touched the tree.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body>
            <ul id="from"><li id="mv">moving</li></ul>
            <ul id="to"><li id="keep">keep</li></ul>
            <div id="res"></div>
            </body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var mv = document.getElementById('mv');
            document.getElementById('from').removeChild(mv);
            document.getElementById('to').insertBefore(mv, document.getElementById('keep'));
            var fromAfter = document.getElementById('from').children.length;
            var toAfter = document.getElementById('to').children.length;
            document.getElementById('res').setText(fromAfter + '|' + toAfter);
            "#,
        );
        let from_kids = kid_ids(&doc, "from");
        let to_kids = kid_ids(&doc, "to");
        assert!(from_kids.is_empty(), "the source must lose the moved node");
        assert_eq!(
            to_kids,
            vec!["mv".to_string(), "keep".to_string()],
            "the node must be relocated before the anchor"
        );
    }
}

#[cfg(test)]
mod insert_before_guard_tests {
    //! Round-2 S0/S1/S2: the InsertBefore arm dereferenced ids it never
    //! checked. blitz indexes `nodes[id]` directly — a freed anchor, a
    //! pending (id 0) child, a self-insert and an own-descendant anchor all
    //! either aborted the process or built a tree cycle that aborted the next
    //! bind. Every shape here must end the test alive.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn run(script: &str) -> String {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="list"><li id="a">a</li><li id="b">b</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(&mut doc, script);
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                .map(|(i, _)| i.as_u64())
                .expect("missing #res"),
        ))
        .map(|n| n.text_content())
        .unwrap_or_default()
    }

    #[test]
    fn a_freed_anchor_does_not_abort_the_run() {
        // The anchor was freed by a textContent replace; the handle survives.
        let out = run(r#"
            var list = document.getElementById('list');
            var anchor = document.getElementById('b');
            list.textContent = 'gone';           // frees the anchor's slot
            var mover = document.createElement('li');
            list.insertBefore(mover, anchor);    // anchor id is dead now
            document.getElementById('res').setText('survived');
            "#);
        assert_eq!(
            out, "survived",
            "a freed anchor must be rejected, not panic"
        );
    }

    #[test]
    fn a_pending_child_anchor_does_not_index_node_zero() {
        // A createElement handle carries _arisId 0; using it must not make
        // the arm index node 0.
        let out = run(r#"
            var list = document.getElementById('list');
            var a = document.getElementById('a');
            list.insertBefore(document.createElement('li'), a);
            document.getElementById('res').setText('survived');
            "#);
        assert_eq!(
            out, "survived",
            "a pending child must be rejected, not panic"
        );
    }

    #[test]
    fn self_insert_and_descendant_insert_do_not_build_cycles() {
        // insertBefore(b, b) used to panic; inserting an ancestor into its
        // own descendant built a cycle that aborted the NEXT bind.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="outer"><div id="inner"><b id="leaf">x</b></div></div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var list = document.getElementById('list');
            var b = document.getElementById('b');
            list.insertBefore(b, b);                       // self-insert
            var outer = document.getElementById('outer');
            var leaf = document.getElementById('leaf');
            outer.insertBefore(leaf, document.getElementById('inner')); // descendant anchor
            document.getElementById('res').setText('survived');
            "#,
        );
        // Reaching the next bind alive is the assertion — the cycle used to
        // abort it with a stack overflow.
        rt.bind_and_run(&mut doc, r#"void 0;"#);
        assert!(true, "no cycle abort");
    }

    #[test]
    fn an_anchor_from_another_tree_does_not_steal_the_node() {
        // The anchor lives under #other; the requested parent is #list. The
        // old arm used the ANCHOR's parent, silently moving into #other.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="list"><li id="a">a</li></ul><ul id="other"><li id="anchor">x</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var mover = document.getElementById('a');
            document.getElementById('list').insertBefore(mover, document.getElementById('anchor'));
            document.getElementById('res').setText('survived');
            "#,
        );
        let list_kids = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("list"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #list"),
            ))
            .map(|n| n.children.len())
            .unwrap_or(usize::MAX);
        let other_kids = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("other"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #other"),
            ))
            .map(|n| n.children.len())
            .unwrap_or(usize::MAX);
        assert_eq!(list_kids, 1, "the mover must stay under its own parent");
        assert_eq!(other_kids, 1, "the anchor's tree must be untouched");
    }
}

#[cfg(test)]
mod append_child_relocate_tests {
    //! L940 (PR #87 R4): the element `appendChild` pushed NO op for a REAL
    //! tree node — only pending (createElement) and canvas children had ops.
    //! `parent.appendChild(nodeFromAnotherParent)` was a TOTAL no-op: no
    //! mirror move, no tree move.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn kid_ids(doc: &HtmlDocument, id: &str) -> Vec<String> {
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                .map(|(i, _)| i.as_u64())
                .expect("missing node"),
        ))
        .map(|n| {
            n.children
                .iter()
                .filter_map(|&c| doc.get_node(c))
                .map(|cn| {
                    cn.attr(blitz_dom::local_name!("id"))
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "#text".to_string())
                })
                .collect()
        })
        .unwrap_or_default()
    }

    #[test]
    fn append_child_relocates_a_node_from_another_parent() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="mv">moving</li></ul><ul id="dst"><li id="k">keep</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var mv = document.getElementById('mv');
            document.getElementById('dst').appendChild(mv);
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "src"),
            Vec::<String>::new(),
            "the source parent must lose the moved node"
        );
        assert_eq!(
            kid_ids(&doc, "dst"),
            vec!["k".to_string(), "mv".to_string()],
            "the destination must gain it at the end (DOM appendChild moves)"
        );
    }

    #[test]
    fn appending_a_node_into_its_own_subtree_is_rejected() {
        // `outer.appendChild(outer)` / appending an ancestor under its own
        // descendant builds a parent cycle that aborts the NEXT bind with a
        // stack overflow. Both shapes must be rejected and survive.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="outer"><div id="inner"><b id="leaf">x</b></div></div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var outer = document.getElementById('outer');
            var inner = document.getElementById('inner');
            outer.appendChild(outer);   // self-append
            inner.appendChild(outer);   // ancestor under own descendant
            document.getElementById('res').setText('survived');
            "#,
        );
        // Reaching the next bind alive is the assertion — the cycle used to
        // abort it.
        rt.bind_and_run(&mut doc, r#"void 0;"#);
        assert!(true, "no cycle abort");
    }
}

#[cfg(test)]
mod append_prepend_dom_tests {
    //! L940 (PR #88 R2): `append(...)` / `prepend(...)` were mirror-only —
    //! the tree never moved. Same family as appendChild (#88).

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn kid_ids(doc: &HtmlDocument, id: &str) -> Vec<String> {
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                .map(|(i, _)| i.as_u64())
                .expect("missing node"),
        ))
        .map(|n| {
            n.children
                .iter()
                .filter_map(|&c| doc.get_node(c))
                .map(|cn| {
                    cn.attr(blitz_dom::local_name!("id"))
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "#text".to_string())
                })
                .collect()
        })
        .unwrap_or_default()
    }

    #[test]
    fn append_moves_a_node_from_another_parent_to_the_end() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="mv">moving</li></ul><ul id="dst"><li id="k">keep</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('dst').append(document.getElementById('mv'));
            "#,
        );
        assert_eq!(kid_ids(&doc, "src"), Vec::<String>::new(), "source drained");
        assert_eq!(
            kid_ids(&doc, "dst"),
            vec!["k".to_string(), "mv".to_string()],
            "append lands the node at the end"
        );
    }

    #[test]
    fn prepend_inserts_before_the_first_child() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="from"><li id="mv">moving</li></ul><ul id="dst"><li id="k">keep</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('dst').prepend(document.getElementById('mv'));
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "from"),
            Vec::<String>::new(),
            "source drained"
        );
        assert_eq!(
            kid_ids(&doc, "dst"),
            vec!["mv".to_string(), "k".to_string()],
            "prepend lands the node before the first child"
        );
    }

    #[test]
    fn multi_arg_prepend_keeps_its_order() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="x">x</li><li id="y">y</li></ul><ul id="dst"><li id="k">keep</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var x = document.getElementById('x');
            var y = document.getElementById('y');
            document.getElementById('dst').prepend(x, y);
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "dst"),
            vec!["x".to_string(), "y".to_string(), "k".to_string()],
            "prepend(x, y) must yield [x, y, keep] — DOM order"
        );
    }

    #[test]
    fn prepend_of_the_current_first_child_is_a_no_op() {
        // Round-2 S0: `l.prepend(l.firstChild)` used to PANIC — blitz
        // detaches the child, then index_of_child(severed anchor) unwraps
        // None. DOM semantics: already in position → no-op → [a,b,c].
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="l"><li id="a">a</li><li id="b">b</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var l = document.getElementById('l');
            l.prepend(l.firstChild);
            var lbl = document.getElementById('res');
            lbl.setText('after');
            "#,
        );
        // Reaching the later statements alive is the core assertion (the
        // panic killed the rest of the script run).
        assert_eq!(kid_ids(&doc, "l"), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn insert_before_own_next_sibling_is_a_no_op() {
        // x already sits immediately before the anchor → insertBefore must
        // be a no-op (it used to detach x and panic on the severed anchor).
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="l"><li id="a">a</li><li id="x">x</li><li id="anchor">n</li><li id="z">z</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var l = document.getElementById('l');
            l.insertBefore(document.getElementById('x'), document.getElementById('anchor'));
            l.insertBefore(document.getElementById('x'), document.getElementById('anchor'));
            document.getElementById('res').setText('survived');
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "l"),
            vec![
                "a".to_string(),
                "x".to_string(),
                "anchor".to_string(),
                "z".to_string(),
            ],
            "an already-positioned insert must be a no-op"
        );
        let res = doc
            .get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default();
        assert_eq!(res, "survived");
    }

    #[test]
    fn mixed_prepend_is_consistent_within_the_same_run() {
        // Round-5 S2: prepend(a, m) where `a` IS the snapshot first child —
        // the tree skips `a` (self-anchor no-op) and moves `m` to the head,
        // so the in-run MIRROR must read [m, a, b, c] too. The old code
        // appended to the mirror tail, leaving [a, b, c] with `m` invisible.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="l"><li id="a">a</li><li id="b">b</li></ul><ul id="src"><li id="m">m</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var l = document.getElementById('l');
            var a = document.getElementById('a');
            var m = document.getElementById('m');
            l.prepend(a, m);
            document.getElementById('res').setText('ran');
            "#,
        );
        // NOTE: an in-run `l.childNodes` read would show the BIND-TIME
        // snapshot (childNodes is snapshot-backed; the JS `_children` mirror
        // is internal and page-invisible), so the observable assertion here
        // is the post-apply TREE. The mirror fix itself (head splice, not
        // tail append) restores master parity and is only indirectly
        // observable through later same-run splice positions.
        assert_eq!(
            kid_ids(&doc, "l"),
            vec!["m".to_string(), "a".to_string(), "b".to_string()],
            "the tree must match the in-run view after apply"
        );
        assert_eq!(
            kid_ids(&doc, "src"),
            Vec::<String>::new(),
            "m must have left its source parent"
        );
    }

    #[test]
    fn self_and_descendant_append_via_append_is_rejected() {
        // The cycle guard is shared with appendChild's path — verify it on
        // the append() entry too.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="outer"><div id="inner"><b id="leaf">x</b></div></div><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var outer = document.getElementById('outer');
            var inner = document.getElementById('inner');
            outer.append(outer);   // self
            inner.append(outer);   // ancestor under own descendant
            document.getElementById('res').setText('survived');
            "#,
        );
        rt.bind_and_run(&mut doc, r#"void 0;"#);
        assert!(true, "no cycle abort");
    }
}

#[cfg(test)]
mod before_after_dom_tests {
    //! L940 (PR #89 R2/R4): `before(...)` / `after(...)` were mirror-only —
    //! `x.before(y)` where y is x's child even SILENTLY CORRUPTED the mirror
    //! (duplicated child). With `Op::InsertAdjacent` the tree moves too.

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn kid_ids(doc: &HtmlDocument, id: &str) -> Vec<String> {
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                .map(|(i, _)| i.as_u64())
                .expect("missing node"),
        ))
        .map(|n| {
            n.children
                .iter()
                .filter_map(|&c| doc.get_node(c))
                .map(|cn| {
                    cn.attr(blitz_dom::local_name!("id"))
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "#text".to_string())
                })
                .collect()
        })
        .unwrap_or_default()
    }

    #[test]
    fn before_relocates_a_cross_parent_node_before_the_ref() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="mv">moving</li></ul><ul id="l"><li id="a">a</li><li id="b">b</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('b').before(document.getElementById('mv'));
            "#,
        );
        assert_eq!(kid_ids(&doc, "src"), Vec::<String>::new(), "source drained");
        assert_eq!(
            kid_ids(&doc, "l"),
            vec!["a".to_string(), "mv".to_string(), "b".to_string()],
            "mv must land immediately before `b`"
        );
    }

    #[test]
    fn after_relocates_a_cross_parent_node_after_the_ref() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="mv">moving</li></ul><ul id="l"><li id="a">a</li><li id="b">b</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('a').after(document.getElementById('mv'));
            "#,
        );
        assert_eq!(kid_ids(&doc, "src"), Vec::<String>::new(), "source drained");
        assert_eq!(
            kid_ids(&doc, "l"),
            vec!["a".to_string(), "mv".to_string(), "b".to_string()],
            "mv must land immediately after `a`"
        );
    }

    #[test]
    fn self_reference_and_in_position_inserts_are_safe_no_ops() {
        // Round-2 S0: `x.before(x)` and `a.after(<an element that already
        // follows a>)` aborted the process — blitz detaches the child first,
        // then index_of_child(severed anchor) unwraps None. DOM semantics:
        // both are no-ops. (Note: `a.after(a.nextSibling)` is NOT a witness —
        // nextSibling is null on parsed-element stubs, so no op is pushed.)
        // `a.after(a)` also survives: its anchor resolves via the append
        // branch, which is why only the before-direction self-insert panics.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="l"><li id="a">a</li><li id="x">x</li><li id="z">z</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var x = document.getElementById('x');
            var a = document.getElementById('a');
            x.before(x);            // self-insert
            a.after(x);             // x already immediately after a
            document.getElementById('res').setText('survived');
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "l"),
            vec!["a".to_string(), "x".to_string(), "z".to_string()],
            "self and in-position inserts must not corrupt the order"
        );
        // The panic used to kill the rest of the script run — surviving to
        // a rebind is part of the assertion.
        rt.bind_and_run(&mut doc, r#"void 0;"#);
    }

    #[test]
    fn multi_arg_before_keeps_its_order() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="x">x</li><li id="y">y</li></ul><ul id="l"><li id="a">a</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var x = document.getElementById('x');
            var y = document.getElementById('y');
            document.getElementById('a').before(x, y);
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "l"),
            vec!["x".to_string(), "y".to_string(), "a".to_string()],
            "before(x, y) must yield [x, y, a] — DOM order"
        );
    }

    #[test]
    fn multi_arg_after_keeps_its_order() {
        // Round-5 S2: after(x, y) used to land [a, y, x, z] — each apply
        // anchored on next_sibling(ref) resolved at apply time, so arg2 was
        // spliced before arg1. DOM order is [a, x, y, z].
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="x">x</li><li id="y">y</li></ul><ul id="l"><li id="a">a</li><li id="z">z</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var x = document.getElementById('x');
            var y = document.getElementById('y');
            document.getElementById('a').after(x, y);
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "l"),
            vec![
                "a".to_string(),
                "x".to_string(),
                "y".to_string(),
                "z".to_string(),
            ],
            "after(x, y) must yield [a, x, y, z] — DOM order"
        );
        assert_eq!(
            kid_ids(&doc, "src"),
            Vec::<String>::new(),
            "sources drained"
        );
    }

    #[test]
    fn after_with_a_mixed_argument_list_still_lands_after_the_ref() {
        // Round-6 S0: with per-arg ops, any TEXT/非树 arg in the list made
        // the real nodes land BEFORE the ref (the mirror and the op batch
        // disagreed about the anchor). One batch op fixes the contract.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="x">x</li><li id="y">y</li></ul><ul id="l"><li id="a">a</li><li id="z">z</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var x = document.getElementById('x');
            var y = document.getElementById('y');
            document.getElementById('a').after(x, 'TEXT', y);
            "#,
        );
        // The text arg never becomes a tree node (pre-existing contract);
        // the two REAL nodes must still land AFTER the ref, in order.
        let kids = kid_ids(&doc, "l");
        assert_eq!(
            kids.first().map(|s| s.as_str()),
            Some("a"),
            "the ref must stay first; got {kids:?}"
        );
        let pos_x = kids.iter().position(|s| s == "x").expect("x missing");
        let pos_y = kids.iter().position(|s| s == "y").expect("y missing");
        assert!(pos_x < pos_y, "x must precede y (DOM order); got {kids:?}");
        assert!(
            pos_x > 0,
            "the real nodes must land AFTER the ref, not before it; got {kids:?}"
        );
    }

    #[test]
    fn after_batch_containing_the_anchor_stays_safe() {
        // Round-7 S0: `a.after(x, y)` where x IS a's next sibling — the
        // anchor is a batch member, blitz detaches it first, and
        // index_of_child(severed anchor) unwrapped None. DOM semantics:
        // x keeps its slot, y lands right after it → [a, x, y].
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="l"><li id="a">a</li><li id="x">x</li><li id="z">z</li></ul><ul id="src"><li id="y">y</li></ul><div id="res"></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var x = document.getElementById('x');
            var y = document.getElementById('y');
            document.getElementById('a').after(x, y);
            document.getElementById('res').setText('survived');
            "#,
        );
        // NOTE on the expected order: per WHATWG, viableNextSibling is
        // computed ONCE (= x), the nodes are pre-inserted reversed
        // (y before x), and re-inserting x with reference==x adjusts the
        // reference to x's first child (null -> append). Chrome yields
        // [a, y, x, z] for this shape — so we assert SAFETY + membership,
        // not a specific permutation.
        assert_eq!(kid_ids(&doc, "src"), Vec::<String>::new(), "source drained");
        let kids = kid_ids(&doc, "l");
        assert_eq!(
            kids.iter()
                .filter(|s| s.as_str() == "x" || s.as_str() == "y")
                .count(),
            2,
            "both moved nodes must be present exactly once; got {kids:?}"
        );
        assert_eq!(
            kids.first().map(|s| s.as_str()),
            Some("a"),
            "the ref must stay first; got {kids:?}"
        );
        // The panic used to kill the rest of the script run.
        assert_eq!(
            doc.get_node(NodeId::from_u64(
                doc.tree()
                    .iter()
                    .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("res"))
                    .map(|(i, _)| i.as_u64())
                    .expect("missing #res"),
            ))
            .map(|n| n.text_content())
            .unwrap_or_default(),
            "survived"
        );
    }

    #[test]
    fn moving_a_node_relative_to_its_own_child_is_a_clean_relocation() {
        // leaf is inner's child: `inner.after(leaf)` detaches leaf from inner
        // and inserts it after inner under body — no cycle (blitz detaches
        // first), and inner must come out empty.
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="outer"><div id="inner"><b id="leaf">x</b></div></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('inner').after(document.getElementById('leaf'));
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "inner"),
            Vec::<String>::new(),
            "inner emptied"
        );
        assert_eq!(
            kid_ids(&doc, "outer"),
            vec!["inner".to_string(), "leaf".to_string()],
            "leaf relocated after inner, no cycle"
        );
        // A rebind must walk the relocated tree without aborting.
        rt.bind_and_run(&mut doc, r#"void 0;"#);
    }
}

#[cfg(test)]
mod replace_with_dom_tests {
    //! L940 (PR #90 R5): `replaceWith(...)` was mirror-only — the tree never
    //! changed. One batch op replaces the ref in the tree and DETACHES it
    //! (remove_node, not drop).

    use super::JsRuntime;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;
    use blitz_traits::NodeId;

    fn kid_ids(doc: &HtmlDocument, id: &str) -> Vec<String> {
        doc.get_node(NodeId::from_u64(
            doc.tree()
                .iter()
                .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some(id))
                .map(|(i, _)| i.as_u64())
                .expect("missing node"),
        ))
        .map(|n| {
            n.children
                .iter()
                .filter_map(|&c| doc.get_node(c))
                .map(|cn| {
                    cn.attr(blitz_dom::local_name!("id"))
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "#text".to_string())
                })
                .collect()
        })
        .unwrap_or_default()
    }

    #[test]
    fn replace_with_swaps_the_ref_for_the_batch_in_the_tree() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="src"><li id="x">x</li><li id="y">y</li></ul><ul id="l"><li id="old">old</li><li id="keep">keep</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            var x = document.getElementById('x');
            var y = document.getElementById('y');
            document.getElementById('old').replaceWith(x, y);
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "src"),
            Vec::<String>::new(),
            "sources drained"
        );
        assert_eq!(
            kid_ids(&doc, "l"),
            vec!["x".to_string(), "y".to_string(), "keep".to_string()],
            "the batch must land where the ref was, ref detached"
        );
        // Detach-not-drop: the replaced node is re-insertable under a new
        // parent (its own last child position here).
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('keep').appendChild(document.getElementById('old'));
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "keep"),
            vec!["#text".to_string(), "old".to_string()],
            "detach-not-drop: the replaced node is re-insertable"
        );
        assert_eq!(
            kid_ids(&doc, "l"),
            vec!["x".to_string(), "y".to_string(), "keep".to_string()],
            "the list order is unchanged by the re-insert"
        );
    }

    #[test]
    fn replace_with_ref_in_own_subtree_is_rejected() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><div id="outer"><div id="inner"><b id="leaf">x</b></div></div></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('leaf').replaceWith(document.getElementById('outer'));
            "#,
        );
        let outer_kids = kid_ids(&doc, "outer");
        assert_eq!(
            outer_kids,
            vec!["inner".to_string()],
            "a cyclic replaceWith must be rejected wholesale"
        );
        rt.bind_and_run(&mut doc, r#"void 0;"#);
    }

    #[test]
    fn replace_with_no_args_just_detaches_the_ref() {
        let mut doc = HtmlDocument::from_html(
            r#"<html><body><ul id="l"><li id="a">a</li><li id="b">b</li></ul></body></html>"#,
            DocumentConfig::default(),
        );
        let mut rt = JsRuntime::new();
        rt.bind_and_run(
            &mut doc,
            r#"
            document.getElementById('a').replaceWith();
            "#,
        );
        assert_eq!(
            kid_ids(&doc, "l"),
            vec!["b".to_string()],
            "replaceWith() with no args detaches the ref"
        );
    }
}
