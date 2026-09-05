# Provider authoring

A resource is a lifecycle `Handler` plus a schema. The schema drives validation, plan
rendering, replacement, CLI documentation, and generated Rust builders.

Start with `crates/ifx/src/providers/memory.rs`, compare
`providers/linode/instance.rs` for an API object, and
`providers/qemu/instance.rs` for a process-backed object.

## Lifecycle

Implement `Handler` from `crates/ifx/src/provider.rs`:

- `schema` declares canonical inputs, outputs, defaults, sensitivity, and replacement;
- `read` returns `Actual` or `None` after observing reality;
- `diff` normally uses the schema-aware default;
- `create` and `update` return `Applied`;
- `delete` tolerates an already-absent object;
- replacement hooks stage, recover, abort, and finalize transactional replacement.

Override `read_for_recovery` when normal observation could discard rollback material.
`Ctx` supplies the URN, transports, and trigger state; handlers do not access stack
directories or state internals.

## Schema

Use `field(name, FieldType)` with `.required()`, `.default()`, `.replace()`,
`.sensitive()`, `.group()`, and `.required_when()` deliberately. Use
`FieldType::open_enum` for extensible provider catalogs, `object_named` for reusable
nested records, and `Connection` for outputs consumed by `host.*`.

`read` must return the same canonical shapes as desired inputs. Normalize ordering,
modes, aliases, absent values, and provider defaults before diffing.

## Identity and adoption

Adoption must prove identity. API resources commonly use a stored provider ID with a
unique label fallback. Local processes use deterministic storage plus a validated
identity record. Never adopt merely because an object is reachable.

## Registration and generation

1. Add the handler under `crates/ifx/src/providers/<provider>/`.
2. Register it in the provider module and built-in registry.
3. Add or map its Cargo feature.
4. Run `cargo run -p ifx-gen`.
5. Regenerate `docs/resources.md`.

Generated modules live under `crates/ifx-program/src/generated/`; do not hand-edit them.

## Safety and tests

Preflight errors must name the missing executable, field, credential, permission, or
conflicting identity. Keep identifiers, arguments, outputs, logs, and fixtures free of
secrets. Validate process identity before signals and prefer provider-native graceful
shutdown.

Test parsing and command/request construction at the pure-function layer, schema
constraints at the registry layer, lifecycle/idempotency/adoption with controlled
reality, and real external infrastructure only behind an explicit environment gate.

```console
$ cargo fmt --all -- --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
$ cargo run -p ifx-gen -- --check
$ cargo run -p ifx-cli -- stubs markdown -o /tmp/resources.md
$ diff -u docs/resources.md /tmp/resources.md
```
