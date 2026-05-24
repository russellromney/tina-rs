use tina_runtime::RuntimeEventKind;

#[test]
fn cross_shard_child_ownership_sim_trace_vocabulary_is_available() {
    let _ = RuntimeEventKind::RemoteChildControlPressure { capacity: 4 };
}
