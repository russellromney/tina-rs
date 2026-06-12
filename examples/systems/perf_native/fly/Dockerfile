# syntax=docker/dockerfile:1
#
# Linux/x86 perf evidence image for the native runtime hot-path probes.
#
# Build context is the repo root (the perf_native crate is a separate cargo
# workspace with path deps reaching the whole tree). Fly builds this on its
# Depot-backed managed builder, which PERSISTS the BuildKit cache mounts below
# across builds — so the expensive dependency + workspace compile is paid once
# and reused on every future run. The compiled test binaries are copied out of
# the cache mount into /app so they survive into the runnable image.
FROM rust:1-bookworm

WORKDIR /src
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/examples/systems/perf_native/target,sharing=locked \
    set -eux; \
    mkdir -p /app; \
    cargo test --release \
        --manifest-path examples/systems/perf_native/Cargo.toml \
        --test hotpath --test perf --no-run --message-format=json \
      | grep -oE '"executable":"[^"]+"' \
      | sed 's/.*"executable":"//; s/"$//' \
      | grep -E '/deps/(hotpath|perf)-[0-9a-f]+$' > /tmp/bins.txt; \
    while read -r b; do [ -n "$b" ] && cp "$b" /app/; done < /tmp/bins.txt; \
    ls -la /app

# Build the Linux validation test binaries from the main workspace too, so the
# explicit-step socket path is exercised under the actual test suite on real
# Linux/x86 (not only the perf probes). Separate target + cache mount from the
# perf_native workspace above.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eux; \
    mkdir -p /app/tests; \
    cargo test --release --no-run --message-format=json \
        -p tina-runtime --test mailbox_readiness \
        --test scheduler_turn_tail --test betelgeuse_substrate --test cancel_call \
        -p tina-http --test pending_read_park --test client_against_native \
        --test grpc_live \
      | grep -oE '"executable":"[^"]+"' \
      | sed 's/.*"executable":"//; s/"$//' \
      | grep -E '/deps/(mailbox_readiness|scheduler_turn_tail|betelgeuse_substrate|cancel_call|pending_read_park|client_against_native|grpc_live)-[0-9a-f]+$' > /tmp/tbins.txt; \
    while read -r b; do [ -n "$b" ] && cp "$b" /app/tests/; done < /tmp/tbins.txt; \
    ls -la /app/tests

WORKDIR /app
# Print every probe row (untraced + traced tail rows included), then run the
# Linux validation tests single-threaded, and exit. Fly captures stdout to the
# machine log; the orchestration script scrapes it.
CMD set -eux; \
    for b in /app/hotpath-* /app/perf-*; do \
      echo "##### RUN $(basename "$b") #####"; \
      "$b" --nocapture || echo "##### $(basename "$b") EXIT $? #####"; \
    done; \
    echo "##### PERF DONE #####"; \
    for b in /app/tests/*; do \
      echo "##### TEST $(basename "$b") #####"; \
      "$b" --test-threads=1 || echo "##### $(basename "$b") EXIT $? #####"; \
    done; \
    echo "##### TESTS DONE #####"
