# aris W3C Web-Platform-Tests Report

> **Tests: 2537** | **Pass: 1115** | **Fail: 1425** | **Pass rate: 43%**

## Progress: 3.5% → 43.9% (12.5x improvement)

| Milestone | Tests Passed |
|-----------|-------------|
| Initial (Boa 0.20) | ~60/1700 (~3.5%) |
| Boa 0.21 + instanceof | 111/1829 (6.1%) |
| Harness + CharacterData | 647/2178 (29.7%) |
| createElementNS + Node methods | 834/2367 (35.3%) |
| createDocumentType + toggleAttribute | 969/2385 (40.6%) |
| _children + innerHTML + firstChild | 983/2385 (41.2%) |
| createEvent + initEvent | 1003/2392 (41.9%) |
| setup() + EventTarget + passive events | 1055/2537 (41.6%) |
| Fix duplicate dispatchEvent + passive tracking | **1115/2537 (43.9%)** |

**62 test files pass 100%.**

## Notable Improvements
- passive-by-default: 0/100 → 80/100
- Node-cloneNode: 0/135 → 122/135
- DOMImplementation-createDocumentType: 1/82 → 80/82
- CharacterData-replaceData: 0/34 → 30/34
