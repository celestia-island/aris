# aris W3C Web-Platform-Tests Report

> Auto-generated from WPT runner output.
> **Tests: 2367** | **Pass: 834** | **Fail: 1533** | **Pass rate: 35%**
> Source: web-platform-tests DOM test suite (real W3C tests)
> Engine: Boa 0.21.1 (upgraded from 0.20 for instanceof support)

## Recent Improvements
- **Boa 0.20 → 0.21 upgrade**: Fixed `instanceof` (element prototype chain), `window === globalThis`, and atom tag parsing.
- **testharness shim**: `async_test` + `window.addEventListener("load")` now fire synchronously, unlocking hundreds of tests.
- **CharacterData methods**: Rewritten to use UTF-16 code units with proper `IndexSizeError` throwing.
- **NamedNodeMap**: `element.attributes` returns array-like of Attr objects; `setAttribute` updates it in-place.
- **createElementNS**: Namespace-aware element creation with proper case handling.
- **Node methods**: `contains`, `hasChildNodes`, `isEqualNode`, `isSameNode`, `compareDocumentPosition`, `getRootNode`.
- **getElementsByTagName/getElementsByClassName**: Return real element handles.

## Summary by Category
| Category | Files | Tests | Pass | Fail | Pass Rate |
|----------|-------|-------|------|------|-----------|
| abort | 5 | 2 | 0 | 2 | 0% |
| collections | 10 | 38 | 0 | 38 | 0% |
| crashtests | 4 | 0 | 0 | 0 | 0% |
| events | 165 | 361 | 43 | 318 | 11% |
| lists | 5 | 183 | 175 | 8 | 95% |
| nodes | 274 | 1403 | 479 | 924 | 34% |
| observable | 1 | 0 | 0 | 0 | 0% |
| ranges | 60 | 227 | 38 | 189 | 16% |
| root | 9 | 123 | 98 | 25 | 79% |
| traversal | 19 | 30 | 1 | 29 | 3% |

## Fully Passing Test Files
| File | Tests |
|------|-------|
| `events\click-on-absolute-pseudo.html` | 1 |
| `events\Event-dispatch-redispatch.html` | 3 |
| `events\Event-dispatch-throwing-multiple-globals.html` | 2 |
| `events\event-handler-attribute-replace-preserves-passive.html` | 2 |
| `events\Event-init-while-dispatching.html` | 1 |
| `events\Event-timestamp-cross-realm-getter.html` | 1 |
| `events\EventListener-incumbent-global-1.sub.html` | 1 |
| `events\EventListener-incumbent-global-2.sub.html` | 1 |
| `events\no-focus-events-at-clicking-editable-content-in-link.html` | 2 |
| `events\preventDefault-during-activation-behavior.html` | 1 |
| `events\scrolling\iframe-chains.html` | 1 |
| `events\scrolling\scroll-cross-origin-iframes.html` | 1 |
| `events\scrolling\scroll-event-fired-to-element.html` | 4 |
| `events\scrolling\scroll-event-fired-to-iframe.html` | 4 |
| `events\scrolling\scrollend-event-fired-after-instant-scroll-in-microtask.html` | 1 |
| `events\scrolling\scrollend-fires-to-text-input.html` | 5 |
| `events\scrolling\wheel-event-composed.html` | 1 |
| `events\scrolling\wheel-event-transactions-multiple-action-chains.html` | 1 |
| `events\scrolling\wheel-event-transactions-target-move.html` | 1 |
| `events\scrolling\wheel-event-transactions-target-removal.html` | 1 |
| `historical-mutation-events.html` | 10 |
| `lists\DOMTokenList-coverage-for-attributes.html` | 175 |
| `nodes\DOMImplementation-hasFeature.html` | 137 |
| `nodes\Element-childElementCount-dynamic-add.html` | 1 |
| `nodes\moveBefore\continue-css-animation-left.html` | 1 |
| `nodes\moveBefore\continue-css-animation-transform.html` | 1 |
| `nodes\moveBefore\continue-css-transition-left-pseudo.html` | 1 |
| `nodes\moveBefore\continue-css-transition-left.html` | 1 |
| `nodes\moveBefore\continue-css-transition-transform-pseudo.html` | 1 |
| `nodes\moveBefore\continue-css-transition-transform.html` | 1 |
| `nodes\moveBefore\css-animation-commit-styles.html` | 1 |
| `nodes\moveBefore\css-transition-cross-document.html` | 1 |
| `nodes\moveBefore\css-transition-cross-shadow.html` | 1 |
| `nodes\moveBefore\css-transition-to-disconnected-document.html` | 1 |
| `nodes\moveBefore\css-transition-trigger.html` | 1 |
| `nodes\moveBefore\custom-element-move-reactions.html` | 6 |
| `nodes\moveBefore\fullscreen-preserve.html` | 1 |
| `nodes\moveBefore\hover-style-update.html` | 2 |
| `nodes\moveBefore\modal-dialog.html` | 1 |
| `nodes\moveBefore\moveBefore-shadow-inside.html` | 1 |
| `nodes\moveBefore\pointer-events.html` | 1 |
| `nodes\moveBefore\popover-preserve.html` | 1 |
| `nodes\moveBefore\relevant-mutations.html` | 2 |
| `nodes\moveBefore\role-updates-after-move.html` | 1 |
| `nodes\moveBefore\selection-preserve.html` | 6 |
| `nodes\moveBefore\style-applies.html` | 1 |
| `nodes\ParentNode-querySelector-All.html` | 1 |
| `ranges\Range-attributes.html` | 1 |
| `ranges\Range-constructor.html` | 1 |
| `ranges\Range-detach.html` | 1 |
| `ranges\tentative\OpaqueRange-interactive-basic.html` | 16 |
| `ranges\tentative\OpaqueRange-interactive-overlap-and-selection.html` | 10 |
| `ranges\tentative\OpaqueRange-update-event-order.html` | 4 |
| `traversal\TreeWalker-acceptNode-filter-cross-realm-null-browsing-context.html` | 1 |

## Top Failing Tests
| File | Pass | Tests | Fail |
|------|------|-------|------|
| `nodes\Document-createElement.html` | 39 | 147 | 108 |
| `events\passive-by-default.html` | 0 | 100 | 100 |
| `nodes\DOMImplementation-createDocumentType.html` | 1 | 82 | 81 |
| `nodes\ParentNode-querySelector-escapes.html` | 7 | 68 | 61 |
| `nodes\Document-createElement-namespace.html` | 1 | 51 | 50 |
| `nodes\attributes.html` | 8 | 58 | 50 |
| `events\Body-FrameSet-Event-Handlers.html` | 0 | 48 | 48 |
| `nodes\ChildNode-after.html` | 0 | 45 | 45 |
| `nodes\ChildNode-before.html` | 0 | 45 | 45 |
| `events\Event-dispatch-click.html` | 0 | 33 | 33 |
| `nodes\ChildNode-replaceWith.html` | 0 | 33 | 33 |
| `nodes\Node-lookupNamespaceURI.html` | 0 | 28 | 28 |
| `ranges\tentative\OpaqueRange-programmatic-updates.html` | 0 | 28 | 28 |
| `ranges\Range-attribute-nodes.html` | 1 | 26 | 25 |
| `nodes\Element-closest.html` | 6 | 29 | 23 |
| `nodes\Node-replaceChild.html` | 1 | 24 | 23 |
| `ranges\tentative\OpaqueRange-display-none.html` | 0 | 18 | 18 |
| `ranges\tentative\OpaqueRange-auto-disconnect.html` | 0 | 17 | 17 |
| `ranges\tentative\OpaqueRange-geometry-multiline-and-mutations.html` | 0 | 15 | 15 |
| `nodes\CharacterData-data.html` | 2 | 16 | 14 |
| `ranges\tentative\OpaqueRange-geometry-basic.html` | 0 | 14 | 14 |
| `nodes\Node-cloneNode.html` | 122 | 135 | 13 |
| `events\Event-dispatch-detached-input-and-change.html` | 0 | 12 | 12 |
| `events\Event-initEvent.html` | 0 | 12 | 12 |
| `events\non-cancelable-when-passive\synthetic-events-cancelable.html` | 0 | 12 | 12 |
| `interface-objects.html` | 11 | 23 | 12 |
| `nodes\CharacterData-substringData.html` | 16 | 28 | 12 |
| `nodes\Node-parentElement.html` | 1 | 12 | 11 |
| `ranges\tentative\OpaqueRange-highlightsFromPoint.html` | 0 | 11 | 11 |
| `ranges\tentative\OpaqueRange-offset.html` | 0 | 11 | 11 |
