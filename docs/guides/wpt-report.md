# aris W3C Web-Platform-Tests Report

> Auto-generated from WPT runner output.
> **Tests: 2367** | **Pass: 839** | **Fail: 1528** | **Pass rate: 35%**
> Source: web-platform-tests DOM test suite (real W3C tests)
> Engine: Boa 0.21.1 (upgraded from 0.20 for instanceof support)

## Summary of Improvements (3.5% → 35.4%)

| Milestone | Pass Rate | Tests Passed |
|-----------|-----------|--------------|
| Initial (Boa 0.20) | 3.5% | ~60/1700 |
| Boa 0.21 + instanceof fix | 6.1% | 111/1829 |
| Harness shim + CharacterData + NamedNodeMap | 29.7% | 647/2178 |
| createElementNS + Node methods | 35.3% | 834/2367 |
| Text.splitText + element nav | 35.4% | 839/2367 |

## Key Fixes
- **Boa 0.20 → 0.21**: Fixed `instanceof` (prototype chain), `window === globalThis`, atom tag parsing
- **bind_and_run fix**: Now calls `install_dom_globals` + `install_event_api` after `install_document`
- **CharacterData methods**: Rewritten with UTF-16 code units + IndexSizeError throwing
- **NamedNodeMap**: element.attributes returns Attr objects; setAttribute updates in-place
- **createElementNS**: Namespace-aware creation with case handling
- **Node methods**: contains, hasChildNodes, isEqualNode, isSameNode, compareDocumentPosition, getRootNode
- **Text.splitText/wholeText**: Proper text node splitting
- **Element navigation**: firstElementChild, lastElementChild, nextElementSibling, previousElementSibling

## Summary by Category
| Category | Files | Tests | Pass | Fail | Pass Rate |
|----------|-------|-------|------|------|-----------|
| abort | 5 | 2 | 0 | 2 | 0% |
| collections | 10 | 38 | 0 | 38 | 0% |
| crashtests | 4 | 0 | 0 | 0 | 0% |
| events | 165 | 361 | 43 | 318 | 11% |
| lists | 5 | 183 | 175 | 8 | 95% |
| nodes | 274 | 1403 | 484 | 919 | 34% |
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
| `nodes\Element-childElement-null.html` | 1 |
| `nodes\Element-childElementCount-nochild.html` | 1 |
| `nodes\Element-siblingElement-null.html` | 1 |
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

## Top Failing Tests (>5 failures)
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
| `nodes\insert-adjacent.html` | 4 | 14 | 10 |
| `nodes\Node-baseURI.html` | 0 | 9 | 9 |
| `nodes\Node-isEqualNode.html` | 0 | 9 | 9 |
| `collections\HTMLCollection-own-props.html` | 0 | 8 | 8 |
| `events\Event-cancelBubble.html` | 0 | 8 | 8 |
| `events\Event-defaultPrevented.html` | 0 | 8 | 8 |
| `nodes\Node-isSameNode.html` | 1 | 9 | 8 |
| `nodes\attributes-namednodemap.html` | 0 | 8 | 8 |
| `ranges\tentative\OpaqueRange-highlight.html` | 0 | 8 | 8 |
| `collections\HTMLCollection-empty-name.html` | 0 | 7 | 7 |
| `collections\HTMLCollection-supported-property-indices.html` | 0 | 7 | 7 |
| `nodes\Node-appendChild.html` | 4 | 11 | 7 |
| `nodes\Node-nodeValue.html` | 0 | 7 | 7 |
| `collections\HTMLCollection-supported-property-names.html` | 0 | 6 | 6 |
| `events\Event-dispatch-click.tentative.html` | 0 | 6 | 6 |
| `events\Event-returnValue.html` | 1 | 7 | 6 |
| `events\EventListener-invoke-legacy.html` | 0 | 6 | 6 |
| `events\EventTarget-this-of-listener.html` | 0 | 6 | 6 |
| `lists\DOMTokenList-iteration.html` | 0 | 6 | 6 |
| `nodes\CharacterData-appendData.html` | 8 | 14 | 6 |
| `nodes\CharacterData-deleteData.html` | 12 | 18 | 6 |
| `nodes\remove-unscopable.html` | 0 | 6 | 6 |
| `ranges\tentative\OpaqueRange-geometry-complexity-and-visibility.html` | 0 | 6 | 6 |
| `ranges\tentative\OpaqueRange-supported-elements.html` | 0 | 6 | 6 |
| `traversal\NodeIterator.html` | 0 | 6 | 6 |
| `traversal\TreeWalker-basic.html` | 0 | 6 | 6 |
