use tina::{RestartBudget, RestartPolicy};
use tina_supervisor::SupervisorConfig;

#[test]
fn supervisor_config_exposes_policy_and_budget_by_reference() {
    let config = SupervisorConfig::new(RestartPolicy::RestForOne, RestartBudget::new(3));

    assert_eq!(config.policy(), RestartPolicy::RestForOne);
    assert_eq!(config.budget(), RestartBudget::new(3));
    assert_eq!(config.policy(), RestartPolicy::RestForOne);
}

#[test]
fn supervisor_config_is_copyable_runtime_vocabulary() {
    let config = SupervisorConfig::new(RestartPolicy::OneForAll, RestartBudget::new(2));
    let copied = config;

    assert_eq!(copied, config);
}
