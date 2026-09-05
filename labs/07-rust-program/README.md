# Lab 7: the Program model

Every supported stack is now a Rust emitter. The small contract is:

```rust
use ifx_program::emit_program;

fn main() -> std::process::ExitCode {
    emit_program(|stack, context| {
        // Declare typed resources on `stack`; read runtime config from `context`.
        Ok(())
    })
}
```

ifxd compiles the trusted Cargo manifest, runs the artifact with `IFX_STACK` and
`IFX_CONFIG`, parses one Program JSON document, validates it, hashes it, and stores the
immutable revision. Clients cannot upload an alternate Program. Provider code and
credentials remain inside ifxd.

References are expressions in the Program, not values read during authoring. Handles
provide typed output methods; `select`, `at`, and `concat!` build deferred expressions.
Read [the design](../../docs/design.md) for the execution boundary and [provider
authoring](../../docs/provider-authoring.md) for schema generation.
