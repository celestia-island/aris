# aris W3C Web-Platform-Tests Report

> Auto-generated from WPT runner output.
> **Tests: 2367** | **Pass: 961** | **Fail: 1406** | **Pass rate: 40%**
> Source: web-platform-tests DOM test suite (real W3C tests, via git submodule)
> Engine: Boa 0.21.1

## Progress Timeline

| Milestone | Pass Rate | Tests Passed |
|-----------|-----------|--------------|
| Initial (Boa 0.20) | ~3.5% | ~60/1700 |
| Boa 0.21 + instanceof fix | 6.1% | 111/1829 |
| Harness shim + CharacterData + NamedNodeMap | 29.7% | 647/2178 |
| createElementNS + Node methods | 35.3% | 834/2367 |
| createDocumentType ownerDocument fix | 38.8% | 919/2367 |
| toggleAttribute + CSS selectors + namespace | 40.0% | 946/2367 |
| nodeValue/data accessors + createProcessingInstruction | 40.6% | 961/2367 |

**58 test files pass 100% of their subtests.**

## Summary by Category
| Category | Files | Tests | Pass | Fail | Pass Rate |
|----------|-------|-------|------|------|-----------|
| abort | 5 | 2 | 0 | 2 | 0% |
| collections | 10 | 38 | 0 | 38 | 0% |
| crashtests | 4 | 0 | 0 | 0 | 0% |
| events | 165 | 361 | 43 | 318 | 11% |
| lists | 5 | 183 | 175 | 8 | 95% |
| nodes | 274 | 1403 | 599 | 804 | 42% |
| observable | 1 | 0 | 0 | 0 | 0% |
| ranges | 60 | 227 | 45 | 182 | 19% |
| root | 9 | 123 | 98 | 25 | 79% |
| traversal | 19 | 30 | 1 | 29 | 3% |

## Top Failing Tests (>5 failures)
| File | Pass | Tests | Fail |
|------|------|-------|------|
| `nodes\Document-createElement.html` | 42 | 147 | 105 |
| `events\passive-by-default.html` | 0 | 100 | 100 |
| `nodes\ParentNode-querySelector-escapes.html` | 7 | 68 | 61 |
| `nodes\Document-createElement-namespace.html` | 1 | 51 | 50 |
| `events\Body-FrameSet-Event-Handlers.html` | 0 | 48 | 48 |
| `nodes\attributes.html` | 12 | 58 | 46 |
| `nodes\ChildNode-after.html` | 0 | 45 | 45 |
| `nodes\ChildNode-before.html` | 0 | 45 | 45 |
| `events\Event-dispatch-click.html` | 0 | 33 | 33 |
| `nodes\ChildNode-replaceWith.html` | 0 | 33 | 33 |
| `ranges\tentative\OpaqueRange-programmatic-updates.html` | 0 | 28 | 28 |
| `nodes\Node-replaceChild.html` | 1 | 24 | 23 |
| `nodes\Element-closest.html` | 8 | 29 | 21 |
| `nodes\Node-lookupNamespaceURI.html` | 9 | 28 | 19 |
| `ranges\Range-attribute-nodes.html` | 8 | 26 | 18 |
| `ranges\tentative\OpaqueRange-display-none.html` | 0 | 18 | 18 |
| `ranges\tentative\OpaqueRange-auto-disconnect.html` | 0 | 17 | 17 |
| `ranges\tentative\OpaqueRange-geometry-multiline-and-mutations.html` | 0 | 15 | 15 |
| `ranges\tentative\OpaqueRange-geometry-basic.html` | 0 | 14 | 14 |
| `nodes\Node-cloneNode.html` | 122 | 135 | 13 |
| `events\Event-dispatch-detached-input-and-change.html` | 0 | 12 | 12 |
| `events\Event-initEvent.html` | 0 | 12 | 12 |
| `events\non-cancelable-when-passive\synthetic-events-cancelable.html` | 0 | 12 | 12 |
| `interface-objects.html` | 11 | 23 | 12 |
| `nodes\CharacterData-substringData.html` | 16 | 28 | 12 |
