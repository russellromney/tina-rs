//! Deterministic simulator proof for the outbound connect helper's DNS
//! classification.
//!
//! Under scripted DNS, a resolved / failed / timed-out lookup must map to a
//! distinct `DnsOutcome` row when fed through `ConnectAttempts::record_dns`.
//! This is the wiring proof above the direct unit tests: a real `dns_lookup`
//! runtime call, driven by the simulator, produces the `CallError` variants
//! the helper classifies. The facts are reproduced deterministically (a
//! supported replay fact), so no exact-replay lie is needed.

#![allow(dead_code)]

use std::cell::RefCell;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use tina::prelude::*;
use tina_http::{
    AddressFamilyPolicy, ConnectAttempts, ConnectPolicy, ConnectSecurity, DnsOutcome,
    EndpointGeneration, EndpointId,
};
use tina_runtime::{CallError, RuntimeCall, dns_lookup};
use tina_sim::{
    ScriptedDnsConfig, ScriptedDnsLookupConfig, ScriptedDnsResult, Simulator, SimulatorConfig,
};

const TEST_SHARD_ID: u32 = 311;

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(TEST_SHARD_ID)
    }
}

#[derive(Debug)]
enum ProbeMsg {
    Lookup {
        host: String,
        port: u16,
        timeout: Duration,
    },
    Done(Result<Vec<SocketAddr>, CallError>),
}

struct DnsProbe {
    observed: Rc<RefCell<Vec<DnsOutcome>>>,
}

impl DnsProbe {
    fn policy() -> ConnectPolicy {
        let mut policy = ConnectPolicy::balanced();
        policy.address_family = AddressFamilyPolicy::PreserveOrder;
        policy.max_resolved_addresses = 4;
        policy
    }
}

impl Isolate for DnsProbe {
    type Message = ProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<ProbeMsg>;
    type Fact = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProbeMsg::Lookup {
                host,
                port,
                timeout,
            } => dns_lookup(host, port, timeout).then(ProbeMsg::Done),
            ProbeMsg::Done(result) => {
                let mut attempts: ConnectAttempts<()> = ConnectAttempts::new(
                    EndpointId::new(1),
                    EndpointGeneration::first(),
                    "host.local",
                    8080,
                    "host.local",
                    &ConnectSecurity::Plain,
                    Self::policy(),
                );
                attempts.record_dns(result);
                self.observed
                    .borrow_mut()
                    .push(attempts.dns_outcome().clone());
                Effect::Noop
            }
        }
    }
}

fn local_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}").parse().unwrap()
}

#[test]
fn scripted_dns_success_failure_timeout_map_to_distinct_outcome_rows() {
    let config = SimulatorConfig {
        dns: ScriptedDnsConfig {
            pending_completion_capacity: 4,
            lookups: vec![
                ScriptedDnsLookupConfig {
                    host: "ok.local".to_string(),
                    port: 8080,
                    complete_after_step: 1,
                    result: ScriptedDnsResult::Resolved(vec![local_addr(48100), local_addr(48101)]),
                },
                ScriptedDnsLookupConfig {
                    host: "fail.local".to_string(),
                    port: 8080,
                    complete_after_step: 1,
                    result: ScriptedDnsResult::Failed,
                },
                ScriptedDnsLookupConfig {
                    host: "slow.local".to_string(),
                    port: 8080,
                    complete_after_step: 1,
                    result: ScriptedDnsResult::Timeout,
                },
            ],
        },
        ..Default::default()
    };

    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, config);
    let probe = sim.register(DnsProbe {
        observed: Rc::clone(&observed),
    });

    for host in ["ok.local", "fail.local", "slow.local"] {
        sim.try_send(
            probe,
            ProbeMsg::Lookup {
                host: host.to_string(),
                port: 8080,
                timeout: Duration::from_millis(50),
            },
        )
        .unwrap();
    }
    sim.run_until_quiescent();

    let rows = observed.borrow();
    assert!(
        rows.contains(&DnsOutcome::Resolved { count: 2 }),
        "scripted success maps to Resolved: {rows:?}"
    );
    assert!(
        rows.contains(&DnsOutcome::Failed),
        "scripted failure maps to Failed: {rows:?}"
    );
    assert!(
        rows.contains(&DnsOutcome::Timeout),
        "scripted timeout maps to Timeout: {rows:?}"
    );
    // Three distinct DNS truths — never collapsed into one generic error.
    assert_eq!(rows.len(), 3);
}
