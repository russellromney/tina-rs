# Phase 114: Post-113 Framework Ergonomics

## Status

- Future IDD outline.
- Runs after phases 110-113 land.
- One PR when executed.

## Purpose

Digest the active helper/report/fact/bridge work into one copied Tina service
path before the next core wave starts.

This is not a broad redesign. It is a cleanup pass over real code that now
exists.

## Includes

- update the blessed service skeleton after 110-113
- tighten prelude/import guidance
- refresh the noun guide for:
  - request/event split
  - pending workflow helpers
  - service reports
  - protocol facts
  - bridge vocabulary
- migrate a small fixed set of systems to the copied path
- move stale findings to history
- add compile-fail examples for common wrong copied shapes
- run one cheap-model style build/proof against the docs

## Does Not Include

- no new protocol feature
- no new bridge feature
- no flow macro
- no public rename
- no new runtime semantics

## Proof Shape

- selected systems still pass
- at least one system is shorter or safer in a measurable way
- docs contain one copied service path
- compile-fail tests catch the main bad wiring learned from 110-113
- findings close solved pain instead of leaving it current

