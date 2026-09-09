# IFX language: testable MVP

`ifx-lang` implements the experimental `ifx/0.1-experimental` grammar in Rust. The
same parser and evaluator power CLI checks, compilation and the stdio language
server. Configuration runs against an in-memory host only. Existing Rust stacks
and IFXD execution are unchanged; this is not yet wired into `burn` or real host
execution.

## Try it

From the repository root:

```sh
cargo build -p ifx-lang
cargo run -p ifx-lang -- check examples/language/linode.ifx
cargo run -p ifx-lang -- check examples/language/modules.ifx
cargo run -p ifx-lang -- simulate examples/language/configure.ifx --applies 2
cargo run -p ifx-lang -- simulate examples/language/configure.ifx --applies 3 --replace-at 3
cargo run -p ifx-lang -- compile examples/language/linode.ifx
```

The two-apply simulation reports `initialized: 1`, `visited: 2`, `reloaded: 1`, and
`observed-after-creation: 2`. Removing a file from the simulator causes its ordinary
`.ensure()` to repair it; removing a directory created inside a completed `once`
body does not rerun that body. Tests exercise both cases.

`compile` emits a versioned envelope containing `program`, `configurations`, and
`outputs`, with SHA-256 catalog/source fingerprints. `program` is IFX's existing `Program`/`ResourceDecl` graph, including
`$ref`/`$path` and `$concat` values. Configuration bodies are separate deferred data;
passing just the graph to an old consumer would omit them. Do not submit this
experimental envelope to managed IFXD. The CLI never applies cloud resources.

Simulation accepts explicit resource output fixtures with `--outputs facts.json`:

```json
{"linode.instance:origin": {"ipv4": "192.0.2.1", "ipv4s": ["192.0.2.1"]}}
```

No addresses, credentials or successful provider operations are fabricated. Missing
referenced facts stop simulation. Each invocation starts a new in-memory simulator;
`--applies` exercises repeat applies in that process. The Rust API lets tests change
source, inject drift, replace targets and reconcile an uncertain action between
applies. There is no persisted journal or production recovery command yet.

## Grammar

The executable parser is `crates/ifx-lang/src/syntax.rs`. Its accepted subset is:

```ebnf
file       = { statement } ;
statement  = binding | import | for | if | expression, ";" ;
binding    = ("let" | "resource" | "module" | "input" | "output"), identifier,
             [":", type], "=", expression, ";" ;
import     = "import", identifier, "from", string, ";" ;
for        = "for", identifier, [",", identifier], "in", expression, block ;
if         = "if", expression, block, ["else", block] ;
block      = "{", { statement }, "}" ;
type       = "String" | "Int" | "Bool" | "List[", type, "]" | "Map[", type, "]" ;
expression = equality ;
equality   = sum, { ("==" | "!="), sum } ;
sum        = unary, { "+", unary } ;
unary      = "!", unary | postfix ;
postfix    = primary, { ".", identifier | "(", [arguments], ")" |
                       "[", expression, "]" } ;
arguments  = expression, { ",", expression }, [","] ;
primary    = string | integer | "true" | "false" | identifier |
             "(", expression, ")" | "[", [arguments], "]" |
             "{", [entry, {",", entry}, [","]], "}" | lambda ;
entry      = (identifier | string), ":", expression ;
lambda     = "|", [identifier, {",", identifier}], "|", block ;
```

Identifiers use ASCII letters, digits and underscores, starting with a letter or
underscore. Strings use JSON escapes and may contain Unicode. `//` starts a
line comment outside strings. Integers are decimal; native arithmetic is checked
signed 64-bit addition. Types are mandatory on inputs and outputs and inferred for
locals. Inputs currently require defaults; module calls override them. Bindings
are immutable and shadowing is rejected. Lists must have compatible element types;
map values are checked against declared map/input types when constrained.

The parser recovers missing delimiters and partial chains for editor diagnostics;
recovered invalid input never compiles. This grammar deliberately has no shell
escape, arbitrary function calls, user function definitions, general closure values,
mutation, `while`, remote imports or implicit package downloads. Lambdas are accepted
by `.configure` and the three action-policy methods only.

## Resources and modules

```text
import machine from "./machine.ifx";
module east = machine("east").label("web-east").region("us-east");
output address: String = east.address;
```

See `examples/language/machine.ifx` for the typed module body. Resource/module
builders finalize implicitly at their declaration. A resource symbol and its
persistent key are separate. Keys may use known variables and map keys. Module
instances prepend a stable namespace; nested names are encoded as JSON arrays to
avoid delimiter ambiguity. Renaming local symbols or reordering maps preserves
resource identity. Duplicate expanded resource identities fail compilation.

Declarations can be generated by known conditions and collection loops. Use
`for value in list` or `for key, value in map`; maps iterate in lexical key order
regardless of dependency feature flags. Resource outputs cannot determine
resource keys or author-time control flow. Typed output references, indexed list
outputs and deferred string concatenation lower to the existing graph. Deferred
comparisons and integer arithmetic must stay inside configuration, where supplied
facts can resolve them; graph compilation rejects unsupported deferred expressions
instead of losing their operands.

The supported provider kinds are `linode.instance` and `memory.value`. `ifx-gen`
exports their existing schemas into the checked-in catalog; its normal `--check`
prevents drift. Schema types and validators now live in `ifx-program`, with an
unchanged re-export from `ifx::schema`. Tooling does not load provider executors.
This is the general IFX schema, **not a Burnable managed-resource allowlist**. Catalog
values are offline hints, not live price/capacity facts or authorization.

Imports are declarations at module scope. CLI imports are relative `.ifx` files
under the entry directory, with no parent/hidden components or symlinks. Files are
opened relative to directory descriptors with no-follow/nonblocking flags and must
be regular files. The LSP performs no filesystem discovery: open the entry and its
imported files in the editor. Unsaved imported buffers override their earlier
versions, and changes/closure recheck importers. Module errors identify the call
site and child path/byte offset; cross-file navigation is still a later increment.

## Ordered configuration and policies

```text
resource web = memory.value("primary-web")
    .value("test target")
    .configure("bootstrap", |host| {
        host.package("nginx").installed(true).ensure();
        host.file("/etc/nginx/app.conf").content("workers=2").ensure();
        host.service("nginx").enabled(true).running(true).ensure();
        host.once("initialize", || { host.record("initialized"); });
        host.always("visit", || { host.record("visited"); });
        host.on_change("reload", "revision-1", || { host.record("reloaded"); });
    });
```

Configuration attaches deferred code to a ready resource. Statements and loop
iterations execute in source order. `.ensure()` finishes before the next statement;
`host.exists(path)` observes earlier operations in that same apply. The simulator
runs configurations serially and stops the apply on failure, so dependent work
cannot proceed. It does not model production concurrency or cloud readiness.

The host parameter also exposes typed target outputs such as `host.ipv4` for a
Linode target. Simulation resolves these from the explicit output fixtures.

| Operation | In-memory behavior |
|---|---|
| `directory(path).mode(string).ensure()` | Converge a directory record; default mode `0755` |
| `file(path).content(string).mode(string).ensure()` | Converge file contents/mode; content required, default mode `0644` |
| `package(name).installed(bool).ensure()` | Converge package record; defaults to installed |
| `service(name).enabled(bool).running(bool).ensure()` | Converge service record; defaults to enabled/running |
| `exists(path)` | Inspect file/directory records |
| `record(key)` | Increment a test-observable counter |
| `fail(label)` | Raise an explicit simulated failure, without echoing the label |

These are records for language tests, not a virtual operating system: installing a
package has no implicit service/file effects. Every operation stays in process.

| Policy | Selection and completion |
|---|---|
| `once(key, || {...})` | Stop selecting after confirmed success for action key + configuration/resource namespace + target incarnation |
| `always(key, || {...})` | Select every explicit apply |
| `on_change(key, watched, || {...})` | Select initially, then only when the explicit watched value changes |

`watched` can be a list/map to watch several inputs. Body edits alone do not select
`on_change`; source authors choose what counts as change. Nested policies are
rejected in this subset. An enclosing policy controls whether its `.ensure()` calls
are reached. Reaching the same action key twice in one apply fails rather than
conflating two actions. Keys are encoded without delimiter collisions.

The simulator marks an action uncertain **before** entering its body, then records
success after the whole body succeeds. Partial failure leaves it uncertain, stops
later statements and blocks replay. Tests may explicitly call `reconcile_success`
with confirmed completion; there is no automatic retry or exactly-once claim.
Target replacement creates a new incarnation. Losing the process loses this test
state; a durable implementation and recovery UX remain necessary before real hosts.

## Editor support

Start any LSP 3.17 client with `ifx-lang lsp`. The server supports full-document
sync, UTF-16 positions, diagnostics, member/value/name completion, schema hover,
local go-to-definition, document symbols and conservative whitespace formatting.
Formatting preserves existing vertical chains and comments; it is not yet a full
opinionated reformatter. Requests are synchronous and bounded, without background
jobs; cancellation notifications do not interrupt an active computation.

Neovim 0.11+ configuration is in `editors/ifx.lua`. Put the built binary on PATH
or change `cmd` to its absolute path. Open imported `.ifx` files as buffers too.
Typing performs no commands, provider calls, secret resolution or disk imports.
The compiler, editor and simulator do not start a local IFXD.

A thin [VS Code adapter](../editors/vscode/README.md) starts the same Rust server
using the standard language client. Its package and JavaScript syntax are checked;
an end-to-end VS Code UI session remains unverified on this host. The CLI's bounded
file loader currently targets Unix (Linux/macOS); Windows packaging is not included.

Limits: 256 KiB/source, 32 modules and 1 MiB aggregate source, 24,000 tokens/source,
48 syntax nesting/chain levels, 8 module nesting levels, 10,000 evaluation steps per
pass/configuration, 256 loop entries, 128 bindings per environment, 256 KiB environment
data, 64 KiB per expanded value, and 128 resources/configurations. The editor retains
analysis data but discards compiled configurations. Limits fail with diagnostics;
this is not a process sandbox or a claim of hostile-code isolation.

## Validation

Fast behavioral suite: `cargo test -p ifx-lang`. Fixtures test graph lowering,
identity under rename/map reorder, typed local modules, unknown values, policies,
drift/replacement/recovery, source order, incomplete buffers, Unicode positions,
CLI/LSP agreement, stdio framing, import boundaries and bounded input handling.

Full checks include workspace formatting, Clippy, tests and generated-schema/docs
checks. Optional installed-tool checks, from the repository root:

```sh
IFX_LANG_BIN="$PWD/target/debug/ifx-lang" nvim --headless -u NONE -l crates/ifx-lang/tests/editor.lua
cargo mutants --in-place -p ifx-lang -f crates/ifx-lang/src/simulator.rs \
  -F 'Host::(begin|finish|replace)' -- --test language
cd crates/ifx-lang
cargo +nightly fuzz run language --target x86_64-unknown-linux-gnu -- -runs=2000 -seed=42 -max_len=8192
```

The mutation command temporarily changes source; run it with no concurrent edits.
Fuzzing requires the separately declared `libfuzzer-sys` development dependency and
an installed compatible nightly/host target. No test performs live cloud actions.
