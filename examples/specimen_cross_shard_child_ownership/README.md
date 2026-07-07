# specimen_cross_shard_child_ownership

the cross-shard child ownership specimen: one supervisor on shard 1 owns two workers, one on shard 1
and one on shard 2. The supervisor learns typed child addresses through
`spawn_observed(...).on_shard(...)`, stops both children with `StopChildren`,
and prints the runtime-owned `ChildLifecycleReport`.

## Run

```sh
cargo run --manifest-path examples/specimen_cross_shard_child_ownership/Cargo.toml
```

Expected report lines:

```text
child_lifecycle specimen=cross_shard_child_ownership parent=1 children=2 shards=[1, 2] state=live
child_lifecycle specimen=cross_shard_child_ownership parent=1 stopped=2 pending_remote_control=0
```
