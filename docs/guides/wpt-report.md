# aris W3C Web-Platform-Tests Report

> Auto-generated from WPT runner output.
> **Tests: 2178** | **Pass: 649** | **Fail: 1529** | **Pass rate: 29%**
> Source: web-platform-tests (real W3C test suite)

## Summary by Category
| Category | Files | Tests | Pass | Fail | Pass Rate |
|----------|-------|-------|------|------|-----------|
| abort | 5 | 2 | 0 | 2 | 0% |
| collections | 10 | 38 | 0 | 38 | 0% |
| crashtests | 4 | 0 | 0 | 0 | 0% |
| events | 165 | 361 | 43 | 318 | 11% |
| lists | 5 | 8 | 0 | 8 | 0% |
| nodes | 274 | 1389 | 473 | 916 | 34% |
| observable | 1 | 0 | 0 | 0 | 0% |
| ranges | 60 | 227 | 38 | 189 | 16% |
| root | 9 | 123 | 94 | 29 | 76% |
| traversal | 19 | 30 | 1 | 29 | 3% |

## Passing Tests
| File | Pass | Tests |
|------|------|-------|
| `nodes\DOMImplementation-hasFeature.html` | 137 | 137 |
| `nodes\Node-cloneNode.html` | 120 | 135 |
| `historical.html` | 72 | 80 |
| `nodes\Document-createElement.html` | 39 | 147 |
| `nodes\CharacterData-replaceData.html` | 30 | 34 |
| `nodes\CharacterData-substringData.html` | 16 | 28 |
| `ranges\tentative\OpaqueRange-interactive-basic.html` | 16 | 16 |
| `nodes\CharacterData-insertData.html` | 14 | 18 |
| `nodes\CharacterData-deleteData.html` | 12 | 18 |
| `interface-objects.html` | 11 | 23 |
| `historical-mutation-events.html` | 10 | 10 |
| `ranges\tentative\OpaqueRange-interactive-overlap-and-selection.html` | 10 | 10 |
| `nodes\CharacterData-appendData.html` | 8 | 14 |
| `nodes\attributes.html` | 8 | 58 |
| `nodes\ParentNode-querySelector-escapes.html` | 7 | 68 |
| `nodes\Element-closest.html` | 6 | 29 |
| `nodes\moveBefore\custom-element-move-reactions.html` | 6 | 6 |
| `nodes\moveBefore\selection-preserve.html` | 6 | 6 |
| `events\Event-dispatch-on-disabled-elements.html` | 5 | 9 |
| `events\scrolling\scrollend-fires-to-text-input.html` | 5 | 5 |
| `events\scrolling\scroll-event-fired-to-element.html` | 4 | 4 |
| `events\scrolling\scroll-event-fired-to-iframe.html` | 4 | 4 |
| `nodes\CharacterData-appendChild.html` | 4 | 9 |
| `nodes\CharacterData-surrogates.html` | 4 | 8 |
| `nodes\Node-appendChild.html` | 4 | 11 |
| `nodes\insert-adjacent.html` | 4 | 14 |
| `ranges\tentative\OpaqueRange-update-event-order.html` | 4 | 4 |
| `events\Event-dispatch-redispatch.html` | 3 | 3 |
| `nodes\Node-nodeName.html` | 3 | 6 |
| `nodes\moveBefore\focus-preserve.html` | 3 | 4 |
| `events\Event-dispatch-throwing-multiple-globals.html` | 2 | 2 |
| `events\event-handler-attribute-replace-preserves-passive.html` | 2 | 2 |
| `events\no-focus-events-at-clicking-editable-content-in-link.html` | 2 | 2 |
| `nodes\CharacterData-data.html` | 2 | 16 |
| `nodes\moveBefore\fire-focusin-focusout.html` | 2 | 5 |
| `nodes\moveBefore\focus-within.html` | 2 | 5 |
| `nodes\moveBefore\hover-style-update.html` | 2 | 2 |
| `nodes\moveBefore\relevant-mutations.html` | 2 | 2 |
| `events\Event-dispatch-detached-click.html` | 1 | 2 |
| `events\Event-init-while-dispatching.html` | 1 | 1 |
| `events\Event-returnValue.html` | 1 | 7 |
| `events\Event-timestamp-cross-realm-getter.html` | 1 | 1 |
| `events\Event-type.html` | 1 | 3 |
| `events\EventListener-incumbent-global-1.sub.html` | 1 | 1 |
| `events\EventListener-incumbent-global-2.sub.html` | 1 | 1 |
| `events\click-on-absolute-pseudo.html` | 1 | 1 |
| `events\preventDefault-during-activation-behavior.html` | 1 | 1 |
| `events\scrolling\iframe-chains.html` | 1 | 1 |
| `events\scrolling\scroll-cross-origin-iframes.html` | 1 | 1 |
| `events\scrolling\scrollend-event-fired-after-instant-scroll-in-microtask.html` | 1 | 1 |
| `events\scrolling\wheel-event-composed.html` | 1 | 1 |
| `events\scrolling\wheel-event-transactions-multiple-action-chains.html` | 1 | 1 |
| `events\scrolling\wheel-event-transactions-target-move.html` | 1 | 1 |
| `events\scrolling\wheel-event-transactions-target-removal.html` | 1 | 1 |
| `nodes\DOMImplementation-createDocumentType.html` | 1 | 82 |
| `nodes\Document-adoptNode.html` | 1 | 4 |
| `nodes\Document-createElement-namespace.html` | 1 | 51 |
| `nodes\Document-implementation.html` | 1 | 2 |
| `nodes\Element-childElementCount-dynamic-add.html` | 1 | 1 |
| `nodes\Element-insertAdjacentElement.html` | 1 | 6 |
| `nodes\Element-insertAdjacentText.html` | 1 | 6 |
| `nodes\Element-tagName.html` | 1 | 6 |
| `nodes\Node-childNodes.html` | 1 | 6 |
| `nodes\Node-parentElement.html` | 1 | 12 |
| `nodes\Node-parentNode.html` | 1 | 5 |
| `nodes\Node-replaceChild.html` | 1 | 24 |
| `nodes\ParentNode-querySelector-All.html` | 1 | 1 |
| `nodes\Text-splitText.html` | 1 | 6 |
| `nodes\moveBefore\continue-css-animation-left.html` | 1 | 1 |
| `nodes\moveBefore\continue-css-animation-transform.html` | 1 | 1 |
| `nodes\moveBefore\continue-css-transition-left-pseudo.html` | 1 | 1 |
| `nodes\moveBefore\continue-css-transition-left.html` | 1 | 1 |
| `nodes\moveBefore\continue-css-transition-transform-pseudo.html` | 1 | 1 |
| `nodes\moveBefore\continue-css-transition-transform.html` | 1 | 1 |
| `nodes\moveBefore\css-animation-commit-styles.html` | 1 | 1 |
| `nodes\moveBefore\css-transition-cross-document.html` | 1 | 1 |
| `nodes\moveBefore\css-transition-cross-shadow.html` | 1 | 1 |
| `nodes\moveBefore\css-transition-to-disconnected-document.html` | 1 | 1 |
| `nodes\moveBefore\css-transition-trigger.html` | 1 | 1 |
| `nodes\moveBefore\fullscreen-preserve.html` | 1 | 1 |
| `nodes\moveBefore\modal-dialog.html` | 1 | 1 |
| `nodes\moveBefore\moveBefore-shadow-inside.html` | 1 | 1 |
| `nodes\moveBefore\pointer-events.html` | 1 | 1 |
| `nodes\moveBefore\popover-preserve.html` | 1 | 1 |
| `nodes\moveBefore\role-updates-after-move.html` | 1 | 1 |
| `nodes\moveBefore\style-applies.html` | 1 | 1 |
| `ranges\Range-adopt-test.html` | 1 | 4 |
| `ranges\Range-attribute-nodes.html` | 1 | 26 |
| `ranges\Range-attributes.html` | 1 | 1 |
| `ranges\Range-commonAncestorContainer-2.html` | 1 | 2 |
| `ranges\Range-constructor.html` | 1 | 1 |
| `ranges\Range-detach.html` | 1 | 1 |
| `ranges\Range-stringifier.html` | 1 | 5 |
| `ranges\tentative\OpaqueRange-validation.html` | 1 | 2 |
| `traversal\TreeWalker-acceptNode-filter-cross-realm-null-browsing-context.html` | 1 | 1 |
| `window-extends-event-target.html` | 1 | 3 |

## Top Failing Tests
| File | Fail | Tests |
|------|------|-------|
| `nodes\Document-createElement.html` | 108 | 147 |
| `events\passive-by-default.html` | 100 | 100 |
| `nodes\DOMImplementation-createDocumentType.html` | 81 | 82 |
| `nodes\ParentNode-querySelector-escapes.html` | 61 | 68 |
| `nodes\Document-createElement-namespace.html` | 50 | 51 |
| `nodes\attributes.html` | 50 | 58 |
| `events\Body-FrameSet-Event-Handlers.html` | 48 | 48 |
| `nodes\ChildNode-after.html` | 45 | 45 |
| `nodes\ChildNode-before.html` | 45 | 45 |
| `events\Event-dispatch-click.html` | 33 | 33 |
| `nodes\ChildNode-replaceWith.html` | 33 | 33 |
| `ranges\tentative\OpaqueRange-programmatic-updates.html` | 28 | 28 |
| `ranges\Range-attribute-nodes.html` | 25 | 26 |
| `nodes\Element-closest.html` | 23 | 29 |
| `nodes\Node-replaceChild.html` | 23 | 24 |
| `nodes\Node-lookupNamespaceURI.html` | 18 | 18 |
| `ranges\tentative\OpaqueRange-display-none.html` | 18 | 18 |
| `ranges\tentative\OpaqueRange-auto-disconnect.html` | 17 | 17 |
| `nodes\Node-cloneNode.html` | 15 | 135 |
| `ranges\tentative\OpaqueRange-geometry-multiline-and-mutations.html` | 15 | 15 |
| `nodes\CharacterData-data.html` | 14 | 16 |
| `ranges\tentative\OpaqueRange-geometry-basic.html` | 14 | 14 |
| `events\Event-dispatch-detached-input-and-change.html` | 12 | 12 |
| `events\Event-initEvent.html` | 12 | 12 |
| `events\non-cancelable-when-passive\synthetic-events-cancelable.html` | 12 | 12 |
| `interface-objects.html` | 12 | 23 |
| `nodes\CharacterData-substringData.html` | 12 | 28 |
| `nodes\Node-parentElement.html` | 11 | 12 |
| `ranges\tentative\OpaqueRange-highlightsFromPoint.html` | 11 | 11 |
| `ranges\tentative\OpaqueRange-offset.html` | 11 | 11 |
