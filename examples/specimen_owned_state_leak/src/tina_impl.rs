//! Adversarial probes against Tina's owned-state contract.
//!
//! The `compile_fail` doctests below are positive tests: each one
//! passes when the snippet does *not* compile. The README has the
//! prose; this file is the test surface.
//!
//! `run` exercises the one shape that *does* compile: an
//! `Arc<Mutex<u64>>` built outside the isolate. The README labels it
//! the anti-pattern.

pub mod compile_fail {
    /// Probe 1: `Rc<RefCell<_>>` in a message; `try_send` requires `Send`.
    ///
    /// ```compile_fail
    /// use std::cell::RefCell;
    /// use std::convert::Infallible;
    /// use std::rc::Rc;
    /// use tina::prelude::*;
    /// use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};
    ///
    /// enum Msg { Leak(Rc<RefCell<u32>>) }
    /// struct I;
    ///
    /// #[tina::isolate(message = Msg)]
    /// impl I {
    ///     fn handle(&mut self, _msg: Msg, _cx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> { noop() }
    /// }
    ///
    /// fn _smuggle() {
    ///     let r = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory).unwrap();
    ///     let a = r.register_with_capacity::<_, Infallible>(I, 1).unwrap();
    ///     let _ = r.try_send(a, Msg::Leak(Rc::new(RefCell::new(0))));
    /// }
    /// ```
    pub fn _probe_1() {}

    /// Probe 2: `&mut self.value` in a `.then(...)` translator.
    ///
    /// ```compile_fail
    /// use std::time::Duration;
    /// use tina::prelude::*;
    /// use tina_runtime::{SleepReply, sleep};
    ///
    /// struct I { value: u32 }
    /// enum Msg { Tick(SleepReply) }
    ///
    /// #[tina_runtime::isolate(message = Msg)]
    /// impl I {
    ///     fn handle(&mut self, _: Msg, _: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
    ///         let r = &mut self.value;
    ///         sleep(Duration::from_millis(1)).then(move |_| { *r += 1; Msg::Tick(Ok(())) })
    ///     }
    /// }
    /// ```
    pub fn _probe_2() {}

    /// Probe 3: `&mut self.value` in an outbound `Send` payload closure.
    ///
    /// ```compile_fail
    /// use tina::prelude::*;
    ///
    /// struct I { value: u32 }
    /// enum Msg { Out(Box<dyn FnMut()>) }
    ///
    /// #[tina::isolate(message = Msg, send = tina::Outbound<Msg>)]
    /// impl I {
    ///     fn handle(&mut self, _: Msg, ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
    ///         let me = ctx.me::<Msg>();
    ///         let r = &mut self.value;
    ///         send(me, Msg::Out(Box::new(move || { *r += 1; })))
    ///     }
    /// }
    /// ```
    pub fn _probe_3() {}

    /// Probe 4: non-`Send` value in a `ChildDefinition` bootstrap message.
    ///
    /// ```compile_fail
    /// use std::cell::RefCell;
    /// use std::rc::Rc;
    /// use tina::ChildDefinition;
    /// use tina::prelude::*;
    ///
    /// struct C;
    /// enum Msg { Leak(Rc<RefCell<u32>>) }
    ///
    /// #[tina::isolate(message = Msg)]
    /// impl C {
    ///     fn handle(&mut self, _: Msg, _: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> { noop() }
    /// }
    ///
    /// fn _send<T: Send>(_: T) {}
    /// fn _smuggle() {
    ///     let cd = ChildDefinition::new(C, 1)
    ///         .with_initial_message(Msg::Leak(Rc::new(RefCell::new(0))));
    ///     _send(cd);
    /// }
    /// ```
    pub fn _probe_4() {}
}

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};

use crate::{INTENTIONAL_ESCAPE_WRITES, Report};

#[derive(Debug)]
enum WriterMsg {
    Tick,
    Done,
}

struct Writer {
    /// User-built shared state. The README labels this the anti-pattern.
    shared: Arc<Mutex<u64>>,
    remaining: u32,
}

#[tina::isolate(message = WriterMsg)]
impl Writer {
    fn handle(
        &mut self,
        msg: WriterMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WriterMsg::Tick => {
                if self.remaining > 0 {
                    *self.shared.lock().expect("shared mutex") += 1;
                    self.remaining -= 1;
                }
                noop()
            }
            WriterMsg::Done => stop(),
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();

    let shared = Arc::new(Mutex::new(0u64));
    let writer = runtime
        .register_with_capacity::<_, Infallible>(
            Writer {
                shared: Arc::clone(&shared),
                remaining: INTENTIONAL_ESCAPE_WRITES,
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register writer: {e:?}"))?;

    let complete = runtime.observe_isolate_complete(writer);
    for _ in 0..INTENTIONAL_ESCAPE_WRITES {
        runtime
            .try_send(writer, WriterMsg::Tick)
            .map_err(|e| anyhow::anyhow!("send Tick: {e:?}"))?;
    }
    runtime
        .try_send(writer, WriterMsg::Done)
        .map_err(|e| anyhow::anyhow!("send Done: {e:?}"))?;

    complete
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("writer did not stop: {e:?}"))?;

    let writes = *shared.lock().expect("shared mutex") as u32;

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(Report {
        documented_compile_fails: crate::DOCUMENTED_COMPILE_FAILS,
        intentional_escape_writes: writes,
        exit_clean: true,
    })
}
