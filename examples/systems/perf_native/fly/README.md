# Linux/x86 perf evidence on Fly.io

Local macOS rows are noisy on a busy laptop. This runs the native hot-path
probes on a dedicated-CPU Linux/x86 Fly machine and captures the rows. Fly's
managed builder is Depot-backed and persists the BuildKit cache mounts in the
`Dockerfile`, so the expensive workspace compile is paid once and reused.

This is local/alpha evidence on one cloud machine, **not** a production
performance claim.

## One-time

```sh
fly apps create tina-perf-150 --org personal
```

## Build + push (cached after first run)

Run from the repo root so the build context is the whole worktree:

```sh
fly deploy . -c examples/systems/perf_native/fly/fly.toml --build-only --push -a tina-perf-150
```

Note the printed `image:` ref.

## Run on a clean x86 machine and capture

```sh
IMG=registry.fly.io/tina-perf-150:deployment-XXXX   # from the build output
fly machine run "$IMG" sleep 1200 -a tina-perf-150 --region iad --vm-size performance-2x --name perf-run-1
MID=$(fly machine list -a tina-perf-150 --json | jq -r '.[0].id')

# Optionally tag the rows with the source commit (the build context excludes
# .git, so the in-image git_sha is "unknown"):
fly ssh console -a tina-perf-150 --machine "$MID" -C "/app/hotpath-<hash> --nocapture"
fly ssh console -a tina-perf-150 --machine "$MID" -C "/app/perf-<hash> --nocapture"

fly machine destroy "$MID" -a tina-perf-150 --force
```

`ls /app` lists the exact binary hashes. The machine idles on `sleep` so SSH
captures stdout reliably; destroy it when done (the app + cached image stay so
the next build/run is fast).

The captured rows live in `../../../../.intent/phases/150-scheduler-turn-tail-performance/perf_sample_linux.txt`.
