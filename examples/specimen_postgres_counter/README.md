# specimen_postgres_counter

Tokio-vs-Tina counter persisted in Postgres. Each side creates its own
table with a unique suffix, increments a single-row counter 50 times,
reads it back, and ends with `final_value = 50`. Both sides drop their
table before exit.

The Tokio side drives `sqlx::PgPool` directly. The Tina side drives a
[`tina-sqlx-bridge`](../../tina-sqlx-bridge) worker; the shard thread
never blocks on SQLx, and the bridge's named caps and typed failures
stay visible.

This specimen needs a real Postgres. CI is **not** expected to run it.

## Run

```sh
DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres \
    cargo run --manifest-path examples/specimen_postgres_counter/Cargo.toml -- both
```

If `DATABASE_URL` is unset, the specimen prints a one-line skip notice
and exits cleanly with status 0 — that keeps it embeddable in repo-wide
sanity scripts without forcing a database dependency.

Modes:

```sh
# tokio only
cargo run --manifest-path examples/specimen_postgres_counter/Cargo.toml -- tokio

# tina only
cargo run --manifest-path examples/specimen_postgres_counter/Cargo.toml -- tina
```

Expected output (with a database):

```
comparison=specimen_postgres_counter side=tokio final_value=50 exit_clean=true
comparison=specimen_postgres_counter side=tina  final_value=50 exit_clean=true
specimen_postgres_counter (tina) bridge metrics: \
  admitted=51 executed=50 row=1 timeouts=0 late=0 full=0 \
  pool_acquire_timeouts=0 sqlx_errors=0 high_water=1
```

Local Postgres via Docker:

```sh
docker run -d --rm --name pg_specimen -p 5499:5432 \
    -e POSTGRES_HOST_AUTH_METHOD=trust postgres:16
DATABASE_URL=postgres://postgres@127.0.0.1:5499/postgres \
    cargo run --manifest-path examples/specimen_postgres_counter/Cargo.toml -- both
docker stop pg_specimen
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)
