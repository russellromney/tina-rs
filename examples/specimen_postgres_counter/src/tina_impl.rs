//! Tina side. Driver isolate calls into a `tina-sqlx-bridge` worker
//! that owns the `sqlx::PgPool`. Each SQL op goes through
//! `tina_runtime::call(...)`; the shard thread never blocks on SQLx.
//! Compare to `tokio_impl.rs` (`pool.execute(...)` directly): same
//! Postgres pool underneath, but the Tina side names every pressure
//! cap and surfaces typed failures.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime};
use tina_sqlx_bridge::{
    PgAddress, PgCloser, PgConfig, PgExecutedOutcome, PgFetchOneOutcome, PgPoolConfig, PgWorker,
    execute_call, fetch_one_call,
};

use crate::{INCREMENTS, Report, unique_table};

#[derive(Debug)]
enum DriverMsg {
    CreateTable,
    Created(PgExecutedOutcome),
    Seed,
    Seeded(PgExecutedOutcome),
    Step,
    Stepped(PgExecutedOutcome),
    Finalize,
    Finalized(PgFetchOneOutcome),
    Drop,
    Dropped(#[allow(dead_code)] PgExecutedOutcome),
}

struct Driver {
    db: PgAddress,
    table: String,
    remaining: u32,
    timeout: Duration,
    report: Report,
    closer: PgCloser,
}

impl Driver {
    fn create_sql(&self) -> String {
        format!(
            "CREATE TABLE {} (id INT8 PRIMARY KEY, value INT8 NOT NULL)",
            self.table
        )
    }

    fn seed_sql(&self) -> String {
        format!("INSERT INTO {} (id, value) VALUES (0, 0)", self.table)
    }

    fn step_sql(&self) -> String {
        format!("UPDATE {} SET value = value + 1 WHERE id = 0", self.table)
    }

    fn finalize_sql(&self) -> String {
        format!("SELECT value FROM {} WHERE id = 0", self.table)
    }

    fn drop_sql(&self) -> String {
        format!("DROP TABLE {}", self.table)
    }

    fn give_up(&self, ctx: &str, outcome: impl std::fmt::Debug) -> Effect<Self> {
        eprintln!("specimen_postgres_counter (tina) {ctx}: unexpected outcome {outcome:?}");
        self.closer.close();
        stop_with(self.report)
    }
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: Report,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::CreateTable => {
                execute_call(self.db, self.create_sql(), vec![], self.timeout)
                    .reply(DriverMsg::Created)
            }
            DriverMsg::Created(outcome) => match outcome {
                CallOutcome::Replied(Ok(_)) => tina_runtime::sleep(Duration::from_millis(0))
                    .reply(|_| DriverMsg::Seed),
                other => self.give_up("create", other),
            },
            DriverMsg::Seed => execute_call(self.db, self.seed_sql(), vec![], self.timeout)
                .reply(DriverMsg::Seeded),
            DriverMsg::Seeded(outcome) => match outcome {
                CallOutcome::Replied(Ok(_)) => tina_runtime::sleep(Duration::from_millis(0))
                    .reply(|_| DriverMsg::Step),
                other => self.give_up("seed", other),
            },
            DriverMsg::Step => execute_call(self.db, self.step_sql(), vec![], self.timeout)
                .reply(DriverMsg::Stepped),
            DriverMsg::Stepped(outcome) => match outcome {
                CallOutcome::Replied(Ok(1)) => {
                    self.remaining -= 1;
                    let next = if self.remaining == 0 {
                        DriverMsg::Finalize
                    } else {
                        DriverMsg::Step
                    };
                    tina_runtime::sleep(Duration::from_millis(0)).reply(move |_| next)
                }
                other => self.give_up("step", other),
            },
            DriverMsg::Finalize => {
                fetch_one_call(self.db, self.finalize_sql(), vec![], self.timeout)
                    .reply(DriverMsg::Finalized)
            }
            DriverMsg::Finalized(outcome) => match outcome {
                CallOutcome::Replied(Ok(Some(row))) => {
                    if let Some(v) = row.get_i64(0) {
                        self.report.final_value = u64::try_from(v).unwrap_or(0);
                        self.report.exit_clean = true;
                    }
                    tina_runtime::sleep(Duration::from_millis(0)).reply(|_| DriverMsg::Drop)
                }
                other => self.give_up("finalize", other),
            },
            DriverMsg::Drop => execute_call(self.db, self.drop_sql(), vec![], self.timeout)
                .reply(DriverMsg::Dropped),
            DriverMsg::Dropped(_) => {
                self.closer.close();
                stop_with(self.report)
            }
        }
    }
}

pub fn run(url: &str) -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let cfg = PgConfig::new()
        .with_pool(
            PgPoolConfig::new(url)
                .with_max_connections(2)
                .with_acquire_timeout(Duration::from_secs(5)),
        )
        .with_default_timeout(Duration::from_secs(5))
        .with_poll_interval(Duration::from_millis(1))
        .with_max_in_flight(2);
    let bridge = PgWorker::<SingleShard>::install(&runtime, cfg)
        .map_err(|e| anyhow::anyhow!("install pg bridge: {e}"))?;

    let driver = Driver {
        db: bridge.address,
        table: unique_table("tina_counter"),
        remaining: INCREMENTS,
        timeout: Duration::from_secs(5),
        report: Report::default(),
        closer: bridge.closer.clone(),
    };
    let driver_addr = runtime
        .register_with_capacity::<_, Infallible>(driver, 16)
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let waiter = runtime
        .observe_result::<Report, _, _>(driver_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver_addr, DriverMsg::CreateTable)
        .map_err(|e| anyhow::anyhow!("try_send CreateTable: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(60))
        .map_err(|e| anyhow::anyhow!("driver did not finish: {e:?}"))?;

    let snap = bridge.metrics.snapshot();
    eprintln!(
        "specimen_postgres_counter (tina) bridge metrics: \
         admitted={} executed={} row={} timeouts={} late={} full={} \
         pool_acquire_timeouts={} sqlx_errors={} high_water={}",
        snap.admitted,
        snap.responses_executed,
        snap.responses_row,
        snap.timeouts,
        snap.late_results,
        snap.full,
        snap.pool_acquire_timeouts,
        snap.sqlx_errors,
        snap.in_flight_high_water,
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(report)
}
