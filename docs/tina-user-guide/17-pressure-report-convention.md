# Pressure Report Convention

A small key=value line a pressure-capable specimen *may* print at the
end of a run. Phase 059 Rock 9 ships this as a **convention**, not a
framework: there is no shared driver and no schema enforcement. A
specimen that doesn't opt in keeps printing whatever it printed
before.

## Shape

```text
pressure side=<name> accepted=N full=N closed=N timeouts=N other=N rss_peak_kb=N exit=<status>
```

- `side` — free-text label for the side ("tokio", "tina", or
  whatever the specimen calls it).
- `accepted` — calls/messages the system accepted.
- `full` — rejections that surfaced as `Full` (any of: mailbox-full,
  reply-path-full, send-full).
- `closed` — rejections that surfaced as `Closed` (lifecycle, not
  capacity).
- `timeouts` — caller-observed timeouts.
- `other` — catch-all bucket the specimen counts (decode errors,
  internal errors, anything not in the four above).
- `rss_peak_kb` — *optional*; peak resident set size in KiB. Skip
  the field if the specimen doesn't measure RSS.
- `exit` — typically `clean` or `fail`.

## Helpers

`tina_runtime::format_pressure_line(&PressureReport { ... })` builds
the canonical line. `tina_runtime::PressureSummary::from_events(...)`
walks the trace and tallies the `full`/`closed` halves automatically;
the specimen adds its own `accepted`, `timeouts`, `other` counts and
exit status.

## Runner behavior

The pressure runners (`specimen_cpu_run`, `specimen_mem_run`) capture the
target's stdout, intercept lines starting with `pressure `, and
re-emit them in the runner's summary. Other lines pass through to
the runner's stdout verbatim. A specimen that emits no `pressure`
line is uninstrumented and the runner just prints its duration plus
exit status.

## Why a convention, not a framework

A shared "metrics struct" would force every specimen to depend on the
same shape. That's the opposite of the specimens rule. The convention
is a contract on the *line*, not a contract on the program. Anyone
can grep for `pressure ` and parse the key=value pairs; anyone can
ignore them.
