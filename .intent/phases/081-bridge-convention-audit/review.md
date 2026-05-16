# Hostile Review: 081 Bridge Convention Audit

## Finding 1: "Convention" can turn into framework fog

The plan could tempt an implementor to invent `tina-bridge-common` too early.
The fix is in the plan now: three exact repeated code shapes are required
before extraction. Similar vibes are not evidence.

## Finding 2: Harmonizing words can hide true backend differences

Reqwest, sqlite, sqlx, aws, tower, tokio, and rpc do not observe the same
facts. The plan now says docs must name worker-terminal vs caller-terminal
truth and must not force same API when backend truth differs.

## Finding 3: Late-result truth is easy to overclaim

Some backends keep working after a caller timeout while the bridge cannot
observe final backend completion. The plan now requires each crate to define
exactly what `late_results` means, or to avoid that promise.

## Finding 4: Supplied-client paths are sharp

Supplied clients/pools often own their own runtime, drop context, timeout, and
connection settings. The plan now has a dedicated ownership rock so docs/code
do not validate ignored config or imply ownership the bridge lacks.

## Finding 5: Docs-only would be too weak if code lies

The audit may find only stale docs, but if a metric/report uses caller-supplied
capacity or terminal events omit promised fields, docs-only is not enough. The
plan now requires code fixes for real mismatches and tests/doc assertions for
fixed overclaims.

## Result

Plan is grug enough: inventory, fix lies, extract only boring proved helpers,
run bridge checks.
