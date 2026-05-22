# tina-extension-compile-fail

Proof that an extension crate **cannot** mint runtime-owned tokens or construct
private runtime report/capability state. Each probe is a `compile_fail` doctest:
code an extension author might wish worked, pinned to *not* compile.

## The four probes

1. **No private backdoor module.** `use tina_runtime::runtime_internal;` is an
   unresolved import — there is no internal escape hatch.
2. **Permits cannot be forged.** `ConcurrencyPermit { inner: None, .. }` fails:
   `error[E0451]: fields of ConcurrencyPermit are private`. A permit is minted
   only by a `ConcurrencyLimit` admitting work.
3. **Bridge pressure cannot be forged.** `BridgePressure { name, capacity, .. }`
   fails: fields are private. Build it through the validated `measured(..)`
   constructor so installed capacity cannot be faked.
4. **Capability rows cannot be forged.** `ResourceCapability { support, .. }`
   fails: fields are private. Capability truth comes from the runtime, not a
   hand-written literal.

The lesson: reach for the public hook (`SyncCodec`, `ServicePolicy`,
`CapacitySurfaceReport`, the `tina_runtime::bridge` vocabulary), never a forged
private value.

## Run the proof

```sh
cargo test --doc --manifest-path examples/extensions/tina-extension-compile-fail/Cargo.toml
```

The count guard (`tests::documented_count_matches_readme`) runs with:

```sh
cargo test --manifest-path examples/extensions/tina-extension-compile-fail/Cargo.toml
```
