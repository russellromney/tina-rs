fn main() -> anyhow::Result<()> {
    let config = system_webhook_relay::RunConfig {
        events: 4,
        call_timeout_ms: 2_000,
        program: vec![
            system_webhook_relay::FakeOutboundProgram::Deliver("backend-1".into()),
            system_webhook_relay::FakeOutboundProgram::Fail(
                system_webhook_relay::OutboundError::Throttled,
            ),
            system_webhook_relay::FakeOutboundProgram::Fail(
                system_webhook_relay::OutboundError::NotFound,
            ),
            system_webhook_relay::FakeOutboundProgram::Deliver("backend-2".into()),
        ],
    };
    let report = system_webhook_relay::run(config)?;
    println!("{report:#?}");
    Ok(())
}
