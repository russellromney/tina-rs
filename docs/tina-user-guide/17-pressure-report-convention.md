# Pressure Report Convention

A small key=value line a pressure-capable specimen *may* print at the end of a
run. This is a **convention**, not a framework: there is no shared driver and
no schema enforcement. A specimen that doesn't opt in keeps printing whatever
it printed before.

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

## Fairness/load companion lines

Keep the pressure line small. Load-capable specimens may also print companion
key=value lines:

```text
fairness side=tina fairness [isolate=1 turns=11 sleeps=5 isolate=2 turns=9 sleeps=4 ...] lag kind=progress_gap_turns subject=2 reference=1 observed=2 bound=none exceeded=false
surface name=svc.mailbox kind=mailbox capacity=4 high_water=4 final_current=0 full=2 max_messages=4 current_messages=0 high_water_messages=4 max_weight=none current_weight=none high_water_weight=none shared_max_weight=none shared_current_weight=none shared_high_water_weight=none leak_clean=true
surface name=svc.ws kind=protocol state=unavailable reason="not exercised by this profile"
```

`progress_gap_turns` is deliberately not wall-clock scheduler latency.
It means one isolate took N more handler turns than another over the
same trace window. Treat that as a lag observation, not an automatic
failure: a hot isolate that admitted more work can honestly have more
turns. If a surface cannot be measured, put it in the service
`ServicePressureReport` as `Unavailable`; do not silently omit it from the
user-facing report.

## Why a convention, not a framework

A shared "metrics struct" would force every specimen to depend on the
same shape. That's the opposite of the specimens rule. The convention
is a contract on the *line*, not a contract on the program. Anyone
can grep for `pressure ` and parse the key=value pairs; anyone can
ignore them.
